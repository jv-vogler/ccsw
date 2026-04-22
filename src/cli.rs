//! Command-line interface.
//!
//! Top-level forms:
//!
//! ```text
//! ccsw                          # TUI picker, then launch claude
//! ccsw <name>                   # shortcut for `run <name> -- claude`
//! ccsw <subcommand> [args...]   # explicit subcommand
//! ```
//!
//! We hand-roll the shortcut dispatch so that a bare profile name can coexist
//! with clap-managed subcommands without either shadowing the other.
//!
//! Three entry paths share the same `main → run` funnel:
//!
//! - `run_shortcut` — triggered when `argv[1]` is not a known subcommand,
//!   flag, or help/version token. Prepares the profile and `execve`s `claude`
//!   directly; never returns on Unix.
//! - `run_picker` — triggered by a bare `ccsw` with no args. Shows the TUI,
//!   then falls through to the same launch path as the shortcut on success.
//! - `dispatch` — triggered for every explicit subcommand (`ls`, `add`, …).
//!   Each `Cmd` variant maps to a `cmd_*` function one-to-one.
//!
//! The shortcut path exists only for ergonomics; everything it does is
//! expressible as `ccsw run <name> -- claude`. Keep it that way: any new
//! behaviour belongs on the `Cmd::Run` side first.

use std::io;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use inquire::Confirm;

use crate::backup;
use crate::launch;
use crate::picker;
use crate::profile::{
    self, copy_forward_claude_json, read_oauth_email, validate_profile_name, Paths, ProfileEntry,
    Registry, CLAUDE_JSON, DEFAULT_PROFILE,
};
use crate::symlinks;

#[derive(Debug, Parser)]
#[command(
    name = "ccsw",
    about = "Switch between Claude Code accounts per terminal with shared plugins and settings",
    version,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// List profiles.
    #[command(alias = "list")]
    Ls,
    /// Create a new profile (copy-forward `.claude.json`, build symlinks, hint `/login`).
    Add { name: String },
    /// Delete a profile (snapshot first).
    #[command(alias = "remove")]
    Rm {
        name: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Rename a profile.
    Rename { old: String, new: String },
    /// Run any command with the given profile's `CLAUDE_CONFIG_DIR`.
    Run {
        name: String,
        /// Command and its arguments (pass after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        argv: Vec<String>,
    },
    /// Spawn a subshell with `CLAUDE_CONFIG_DIR` exported for this profile.
    Shell { name: String },
    /// Re-evaluate symlinks for every profile against the current allowlist.
    Sync,
    /// Verify symlinks, credentials, and orphans without making changes.
    Doctor,
    /// Restore a profile from a backup (identified by timestamp or unique prefix).
    Restore { timestamp: String },
    /// List snapshots.
    Backups,
    /// Print the profile of the current shell (reads `CLAUDE_CONFIG_DIR`).
    Current,
    /// Generate shell completions.
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
}

/// Entrypoint. Called from `main`.
pub fn run() -> Result<()> {
    let raw: Vec<String> = std::env::args().collect();

    // Shortcut: `ccsw <profile>` where profile is not a known subcommand and not a flag.
    if let Some(first) = raw.get(1) {
        if !first.starts_with('-')
            && !is_known_subcommand(first)
            && first != "help"
            && first != "-h"
            && first != "--help"
            && first != "-V"
            && first != "--version"
        {
            return run_shortcut(first, &raw[2..]);
        }
    }

    let cli = Cli::parse();
    match cli.command {
        None => run_picker(),
        Some(cmd) => dispatch(cmd),
    }
}

fn is_known_subcommand(s: &str) -> bool {
    Cli::command()
        .get_subcommands()
        .any(|sc| sc.get_name() == s || sc.get_all_aliases().any(|a| a == s))
}

fn run_shortcut(name: &str, extra: &[String]) -> Result<()> {
    let paths = Paths::from_env()?;
    let prep = launch::prepare(&paths, name)?;
    touch_last_used(&paths, name).ok();
    let _never = launch::replace_with_claude(&prep, extra)?;
    unreachable!()
}

fn run_picker() -> Result<()> {
    let paths = Paths::from_env()?;
    let reg = Registry::load(&paths)?;
    let choice = picker::pick(&paths, &reg)?;
    match choice {
        Some(name) => {
            let prep = launch::prepare(&paths, &name)?;
            touch_last_used(&paths, &name).ok();
            let _never = launch::replace_with_claude(&prep, &[])?;
            unreachable!()
        }
        None => Ok(()),
    }
}

fn dispatch(cmd: Cmd) -> Result<()> {
    let paths = Paths::from_env()?;
    match cmd {
        Cmd::Ls => cmd_ls(&paths),
        Cmd::Add { name } => cmd_add(&paths, &name),
        Cmd::Rm { name, yes } => cmd_rm(&paths, &name, yes),
        Cmd::Rename { old, new } => cmd_rename(&paths, &old, &new),
        Cmd::Run { name, argv } => cmd_run(&paths, &name, &argv),
        Cmd::Shell { name } => cmd_shell(&paths, &name),
        Cmd::Sync => cmd_sync(&paths),
        Cmd::Doctor => cmd_doctor(&paths),
        Cmd::Restore { timestamp } => cmd_restore(&paths, &timestamp),
        Cmd::Backups => cmd_backups(&paths),
        Cmd::Current => cmd_current(&paths),
        Cmd::Completions { shell } => cmd_completions(shell),
    }
}

fn touch_last_used(paths: &Paths, name: &str) -> Result<()> {
    if name == DEFAULT_PROFILE {
        return Ok(());
    }
    let mut reg = Registry::load(paths)?;
    if reg.contains(name) {
        reg.touch(name);
        reg.save(paths)?;
    }
    Ok(())
}

// ─── subcommand impls ──────────────────────────────────────────────────────

fn cmd_ls(paths: &Paths) -> Result<()> {
    let reg = Registry::load(paths)?;
    let active = current_profile_name(paths);

    // `default` is implicit — always show first.
    print_profile_row(
        "default",
        &paths.base,
        None,
        active.as_deref() == Some("default"),
    )?;
    for p in &reg.profiles {
        let dir = paths.profile_dir(&p.name);
        let active_here = active.as_deref() == Some(p.name.as_str());
        print_profile_row(&p.name, &dir, p.last_used_at, active_here)?;
    }
    Ok(())
}

fn print_profile_row(
    name: &str,
    dir: &Path,
    last_used: Option<DateTime<Utc>>,
    active: bool,
) -> Result<()> {
    let email = read_oauth_email(dir).unwrap_or_else(|| "—".to_string());
    let last = last_used
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "—".to_string());
    let marker = if active { "*" } else { " " };
    println!("{marker} {name:<16} {email:<40} last-used: {last}");
    Ok(())
}

fn cmd_add(paths: &Paths, name: &str) -> Result<()> {
    validate_profile_name(name)?;
    paths.ensure_profiles_root()?;
    let dest = paths.profile_dir(name);
    if dest.exists() {
        bail!("profile '{name}' already exists at {}", dest.display());
    }
    let mut reg = Registry::load(paths)?;
    if reg.contains(name) {
        bail!("profile '{name}' is already in the registry");
    }

    // Snapshot the registry file only (covers rollback of corrupt registry state).
    // Passing `profiles_root` here would recurse through `.backups/` into the
    // in-progress snapshot payload until PATH_MAX (errno 36, ENAMETOOLONG).
    let registry = paths.registry_file();
    if registry.exists() {
        backup::snapshot(paths, &registry, "pre-add", name)?;
    }

    std::fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
    let copied = copy_forward_claude_json(paths, &dest)?;
    symlinks::heal(&paths.base, &dest)?;

    reg.upsert(ProfileEntry {
        name: name.to_string(),
        created_at: Utc::now(),
        last_used_at: None,
    });
    reg.save(paths)?;

    println!("Profile '{name}' created at {}.", dest.display());
    if copied {
        println!("  • .claude.json copied forward from base.");
    } else {
        println!("  • no base .claude.json yet; one will be created on /login.");
    }
    println!("  • shared plugins/settings symlinked from ~/.claude.");
    println!("\nRun `ccsw {name}`, then `/login` inside Claude Code.");

    backup::rotate(paths, &reg.retention, Utc::now())?;
    Ok(())
}

fn cmd_rm(paths: &Paths, name: &str, yes: bool) -> Result<()> {
    if name == DEFAULT_PROFILE {
        bail!("refusing to remove the reserved 'default' profile");
    }
    validate_profile_name(name)?;
    let dir = paths.profile_dir(name);
    if !dir.is_dir() {
        bail!("profile '{name}' does not exist");
    }

    if !yes && !confirm(&format!("Delete profile '{name}' at {}?", dir.display()))? {
        println!("Aborted.");
        return Ok(());
    }

    // Refuse if the current shell has this profile active (soft guard).
    if current_profile_name(paths).as_deref() == Some(name) {
        eprintln!(
            "ccsw: warning: this shell has CLAUDE_CONFIG_DIR set to '{name}'. \
             You should exit any `claude` session under this profile before deleting it."
        );
    }

    backup::snapshot(paths, &dir, "rm", name)?;
    std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;

    let mut reg = Registry::load(paths)?;
    reg.remove(name);
    reg.save(paths)?;

    println!("Profile '{name}' removed. Snapshot kept — restore with `ccsw backups`.");
    backup::rotate(paths, &reg.retention, Utc::now())?;
    Ok(())
}

fn cmd_rename(paths: &Paths, old: &str, new: &str) -> Result<()> {
    if old == DEFAULT_PROFILE || new == DEFAULT_PROFILE {
        bail!("cannot rename the reserved 'default' profile");
    }
    validate_profile_name(old)?;
    validate_profile_name(new)?;
    if old == new {
        bail!("old and new names are identical");
    }

    let old_dir = paths.profile_dir(old);
    let new_dir = paths.profile_dir(new);
    if !old_dir.is_dir() {
        bail!("profile '{old}' does not exist");
    }
    if new_dir.exists() {
        bail!("profile '{new}' already exists");
    }

    backup::snapshot(paths, &old_dir, "rename", old)?;
    std::fs::rename(&old_dir, &new_dir)
        .with_context(|| format!("renaming {} → {}", old_dir.display(), new_dir.display()))?;

    let mut reg = Registry::load(paths)?;
    if let Some(mut entry) = reg.remove(old) {
        entry.name = new.to_string();
        reg.upsert(entry);
    } else {
        reg.upsert(ProfileEntry {
            name: new.to_string(),
            created_at: Utc::now(),
            last_used_at: None,
        });
    }
    reg.save(paths)?;

    println!("Renamed '{old}' → '{new}'.");
    backup::rotate(paths, &reg.retention, Utc::now())?;
    Ok(())
}

fn cmd_run(paths: &Paths, name: &str, argv: &[String]) -> Result<()> {
    let (program, rest) = argv.split_first().context("no command given to run")?;
    let prep = launch::prepare(paths, name)?;
    touch_last_used(paths, name).ok();
    let _never = launch::replace_process(&prep, program, rest)?;
    unreachable!()
}

fn cmd_shell(paths: &Paths, name: &str) -> Result<()> {
    let prep = launch::prepare(paths, name)?;
    touch_last_used(paths, name).ok();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let _never = launch::replace_process(&prep, &shell, &[])?;
    unreachable!()
}

fn cmd_sync(paths: &Paths) -> Result<()> {
    let reg = Registry::load(paths)?;
    let mut total_repaired = 0usize;
    let mut total_created = 0usize;
    for p in &reg.profiles {
        let dir = paths.profile_dir(&p.name);
        if !dir.is_dir() {
            eprintln!(
                "ccsw: warning: profile '{}' is registered but missing on disk",
                p.name
            );
            continue;
        }
        let report = symlinks::heal(&paths.base, &dir)?;
        let c = report.count(symlinks::HealAction::Created);
        let r = report.count(symlinks::HealAction::Repaired);
        total_created += c;
        total_repaired += r;
        for w in &report.warnings {
            eprintln!("ccsw: {}: {}", p.name, w);
        }
        println!("  {}: {c} created, {r} repaired", p.name);
    }
    println!("Sync complete ({total_created} created, {total_repaired} repaired).");
    Ok(())
}

fn cmd_doctor(paths: &Paths) -> Result<()> {
    let reg = Registry::load(paths)?;
    let mut issues = 0usize;

    println!("base: {}", paths.base.display());
    if !paths.base.is_dir() {
        println!("  ! base directory does not exist");
        issues += 1;
    }
    println!("profiles root: {}", paths.profiles_root.display());

    for p in &reg.profiles {
        let dir = paths.profile_dir(&p.name);
        println!("\nprofile '{}': {}", p.name, dir.display());
        if !dir.is_dir() {
            println!("  ! registered but missing on disk");
            issues += 1;
            continue;
        }
        // Creds
        let has_creds = dir.join(profile::CREDENTIALS_FILE).exists();
        let has_json = dir.join(CLAUDE_JSON).exists();
        println!(
            "  creds: {}  .claude.json: {}",
            if has_creds {
                "present"
            } else {
                "missing (run /login)"
            },
            if has_json { "present" } else { "missing" }
        );
        if !has_creds {
            issues += 1;
        }
        // Symlinks (read-only inspection — `sync` is the command that actually heals).
        let report = symlinks::diagnose(&paths.base, &dir)?;
        let would_repair = report.count(symlinks::HealAction::Repaired);
        let would_create = report.count(symlinks::HealAction::Created);
        let real_files = report.count(symlinks::HealAction::SkippedRealFile);
        if would_create + would_repair + real_files > 0 {
            println!(
                "  symlinks: {would_create} missing, {would_repair} broken/misaimed, \
                 {real_files} real-file overrides  (run `ccsw sync` to fix)"
            );
        } else {
            println!("  symlinks: ok");
        }
        for w in &report.warnings {
            println!("  ! {w}");
        }
        issues += would_create + would_repair + real_files;
        // Orphans
        let orphans = symlinks::orphans(&dir)?;
        if !orphans.is_empty() {
            println!("  orphans (not in allowlist or keep-list):");
            for o in &orphans {
                println!("    - {}", o.display());
            }
        }
    }

    if issues == 0 {
        println!("\nAll clear.");
    } else {
        println!("\n{issues} issue(s) reported.");
    }
    Ok(())
}

fn cmd_restore(paths: &Paths, needle: &str) -> Result<()> {
    let snap = backup::restore(paths, needle)?;
    println!(
        "Restored profile '{}' from snapshot {}.",
        snap.meta.name, snap.id
    );
    Ok(())
}

fn cmd_backups(paths: &Paths) -> Result<()> {
    let all = backup::list(paths)?;
    if all.is_empty() {
        println!("No snapshots.");
        return Ok(());
    }
    for s in all {
        println!("  {:<24}  op={:<12} name={}", s.id, s.meta.op, s.meta.name);
    }
    Ok(())
}

fn cmd_current(paths: &Paths) -> Result<()> {
    match current_profile_name(paths) {
        Some(name) => println!("{name}"),
        None => println!("(unknown: CLAUDE_CONFIG_DIR is set but unrecognized)"),
    }
    Ok(())
}

fn cmd_completions(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "ccsw", &mut io::stdout());
    Ok(())
}

/// Best-effort identification of the current shell's profile.
///
/// Returns:
///   - `Some("default")` if `CLAUDE_CONFIG_DIR` is unset OR equals the base dir.
///   - `Some("<name>")` if the env var points inside `~/.claude-profiles/`.
///   - `None` if the env var is set to something unrecognized.
pub fn current_profile_name(paths: &Paths) -> Option<String> {
    let Ok(raw) = std::env::var(launch::ENV_CONFIG_DIR) else {
        return Some(DEFAULT_PROFILE.to_string());
    };
    let p = std::path::PathBuf::from(&raw);
    let canonical = p.canonicalize().unwrap_or(p);
    let base_canon = paths
        .base
        .canonicalize()
        .unwrap_or_else(|_| paths.base.clone());
    if canonical == base_canon {
        return Some(DEFAULT_PROFILE.to_string());
    }
    let pr_canon = paths
        .profiles_root
        .canonicalize()
        .unwrap_or_else(|_| paths.profiles_root.clone());
    if let Ok(rel) = canonical.strip_prefix(&pr_canon) {
        // first component is the profile name
        if let Some(first) = rel.components().next() {
            return Some(first.as_os_str().to_string_lossy().into_owned());
        }
    }
    None
}

/// Ask the user to confirm; default is No. Cancellation (Ctrl-C/ESC) and
/// non-TTY contexts are both treated as "no" — the tool must never delete
/// something the user didn't explicitly agree to.
fn confirm(prompt: &str) -> Result<bool> {
    Ok(Confirm::new(prompt)
        .with_default(false)
        .prompt_skippable()?
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Paths) {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::from_home(tmp.path());
        paths.ensure_profiles_root().unwrap();
        fs::create_dir_all(&paths.base).unwrap();
        (tmp, paths)
    }

    #[test]
    fn current_returns_default_when_env_unset() {
        let (_tmp, paths) = setup();
        std::env::remove_var(launch::ENV_CONFIG_DIR);
        assert_eq!(current_profile_name(&paths).as_deref(), Some("default"));
    }

    #[test]
    fn current_returns_profile_name_when_env_in_profiles_root() {
        let (_tmp, paths) = setup();
        let dir = paths.profile_dir("work");
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var(launch::ENV_CONFIG_DIR, &dir);
        let got = current_profile_name(&paths);
        std::env::remove_var(launch::ENV_CONFIG_DIR);
        assert_eq!(got.as_deref(), Some("work"));
    }

    #[test]
    fn is_known_subcommand_matches_clap() {
        // Sanity: every subcommand and alias clap knows about is recognised…
        for sc in Cli::command().get_subcommands() {
            assert!(
                is_known_subcommand(sc.get_name()),
                "expected {} to be known",
                sc.get_name()
            );
            for a in sc.get_all_aliases() {
                assert!(is_known_subcommand(a), "expected alias {a} to be known");
            }
        }
        // …and arbitrary strings are not.
        assert!(!is_known_subcommand("company"));
        assert!(!is_known_subcommand("default"));
    }
}
