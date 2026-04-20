//! Shared-allowlist management and heal-on-launch logic.
//!
//! Each entry in [`ALLOWLIST`] is symlinked from a profile directory back to the
//! base profile at `~/.claude`, so plugins/settings/hooks installed once are
//! visible to every account. Heal-on-launch is idempotent: already-correct
//! symlinks are left alone, broken ones are repaired, missing ones are created,
//! and real files/dirs are never clobbered.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Files and directories shared across all profiles. Each entry is symlinked
/// from a profile directory back to the same-named entry under the base profile.
///
/// Trailing `/` is cosmetic — [`strip_trailing_slash`] normalizes both forms.
pub const ALLOWLIST: &[&str] = &[
    // Top-level config files
    "settings.json",
    "CLAUDE.md",
    "RTK.md",
    "keybindings.json",
    // Directories — linked as directories, not contents
    "hooks/",
    "plugins/",
    "agents/",
    "skills/",
    "rules/",
    "ide/",
    "shell-snapshots/",
    "paste-cache/",
    "cache/",
    "downloads/",
    "file-history/",
    "plans/",
    "tasks/",
    "session-env/",
    // History lives at the top level
    "history.jsonl",
];

/// Files and directories kept real (per-profile, never symlinked). Used by
/// `doctor` to classify orphan entries.
pub const PER_PROFILE_KEEP: &[&str] = &[
    ".credentials.json",
    ".claude.json",
    "projects/",
    "sessions/",
    "backups/",
    "debug/",
    "telemetry/",
    "usage-data/",
    "mcp-needs-auth-cache.json",
];

fn strip_trailing_slash(s: &str) -> &str {
    s.strip_suffix('/').unwrap_or(s)
}

/// True if `src` has *any* entity at its path — file, directory, or broken
/// symlink. `Path::exists` follows links and returns `false` for dangling ones,
/// which is the wrong answer here: if the base has something by that name, we
/// want the profile to mirror it.
fn source_present(src: &Path) -> bool {
    src.exists() || src.symlink_metadata().is_ok()
}

/// What happened to one allowlist entry during [`heal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealAction {
    /// Created a new symlink (nothing was present).
    Created,
    /// Replaced a broken or wrongly-targeted symlink.
    Repaired,
    /// Symlink was already correct — no filesystem change.
    AlreadyOk,
    /// A real file/dir was present at the destination; left untouched.
    SkippedRealFile,
    /// Source didn't exist under the base; nothing to link yet.
    SkippedNoSource,
}

#[derive(Debug, Default)]
pub struct HealReport {
    pub actions: Vec<(String, HealAction)>,
    pub warnings: Vec<String>,
}

impl HealReport {
    pub fn count(&self, action: HealAction) -> usize {
        self.actions.iter().filter(|(_, a)| *a == action).count()
    }
}

/// Walk [`ALLOWLIST`] and bring each entry in the profile directory into agreement
/// with the base. Idempotent. Never clobbers real user data.
///
/// The profile directory must already exist; the caller is responsible for
/// creating it (via `ccsw add`).
pub fn heal(base: &Path, profile: &Path) -> Result<HealReport> {
    let mut report = HealReport::default();
    for entry in ALLOWLIST {
        let name = strip_trailing_slash(entry);
        let src = base.join(name);
        let dest = profile.join(name);
        let action = heal_one(&src, &dest, &mut report.warnings)
            .with_context(|| format!("healing {} in {}", name, profile.display()))?;
        report.actions.push((name.to_string(), action));
    }
    Ok(report)
}

/// Read-only variant of [`heal`] — reports what *would* happen without touching
/// the filesystem. Used by `ccsw doctor`.
pub fn diagnose(base: &Path, profile: &Path) -> Result<HealReport> {
    let mut report = HealReport::default();
    for entry in ALLOWLIST {
        let name = strip_trailing_slash(entry);
        let src = base.join(name);
        let dest = profile.join(name);
        let action = classify(&src, &dest, &mut report.warnings)
            .with_context(|| format!("inspecting {} in {}", name, profile.display()))?;
        report.actions.push((name.to_string(), action));
    }
    Ok(report)
}

fn classify(src: &Path, dest: &Path, warnings: &mut Vec<String>) -> Result<HealAction> {
    if !source_present(src) {
        return Ok(HealAction::SkippedNoSource);
    }
    match fs::symlink_metadata(dest) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                match fs::read_link(dest) {
                    Ok(target) if target == src => Ok(HealAction::AlreadyOk),
                    _ => Ok(HealAction::Repaired), // would repair
                }
            } else {
                warnings.push(format!(
                    "{}: real file or directory in the way of shared entry",
                    dest.display()
                ));
                Ok(HealAction::SkippedRealFile)
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(HealAction::Created),
        Err(err) => Err(err).with_context(|| format!("stat {}", dest.display())),
    }
}

fn heal_one(src: &Path, dest: &Path, warnings: &mut Vec<String>) -> Result<HealAction> {
    if !source_present(src) {
        return Ok(HealAction::SkippedNoSource);
    }

    match fs::symlink_metadata(dest) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                match fs::read_link(dest) {
                    Ok(target) if target == src => {
                        // Correct target. We still check that it resolves: a correct
                        // target whose *source* has since disappeared (handled above)
                        // won't reach here. So this path means everything's fine.
                        Ok(HealAction::AlreadyOk)
                    }
                    _ => {
                        // Wrong target or unreadable — replace.
                        fs::remove_file(dest).with_context(|| {
                            format!("removing stale symlink {}", dest.display())
                        })?;
                        create_symlink(src, dest)?;
                        Ok(HealAction::Repaired)
                    }
                }
            } else {
                warnings.push(format!(
                    "skipping {}: a real file or directory is in the way; \
                     remove it manually if you meant for this to be shared",
                    dest.display()
                ));
                Ok(HealAction::SkippedRealFile)
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            create_symlink(src, dest)?;
            Ok(HealAction::Created)
        }
        Err(err) => Err(err).with_context(|| format!("stat {}", dest.display())),
    }
}

#[cfg(unix)]
fn create_symlink(src: &Path, dest: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent of {}", dest.display()))?;
    }
    symlink(src, dest).with_context(|| format!("symlinking {} → {}", dest.display(), src.display()))
}

#[cfg(windows)]
fn create_symlink(src: &Path, dest: &Path) -> Result<()> {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent of {}", dest.display()))?;
    }
    let is_dir = fs::metadata(src).map(|m| m.is_dir()).unwrap_or(false);
    if is_dir {
        symlink_dir(src, dest)
    } else {
        symlink_file(src, dest)
    }
    .with_context(|| {
        format!(
            "symlinking {} → {} (Windows requires Developer Mode or admin)",
            dest.display(),
            src.display()
        )
    })
}

/// Enumerate entries present in a profile that are in neither the allowlist nor
/// the per-profile-keep list. Used by `doctor` to surface drift without auto-removing.
pub fn orphans(profile: &Path) -> Result<Vec<PathBuf>> {
    if !profile.is_dir() {
        return Ok(Vec::new());
    }
    let mut known: std::collections::HashSet<String> = std::collections::HashSet::new();
    for e in ALLOWLIST.iter().chain(PER_PROFILE_KEEP.iter()) {
        known.insert(strip_trailing_slash(e).to_string());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(profile)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !known.contains(&name) {
            out.push(entry.path());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Env {
        _tmp: TempDir,
        base: PathBuf,
        profile: PathBuf,
    }

    fn setup() -> Env {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("base");
        let profile = tmp.path().join("profile");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&profile).unwrap();
        Env {
            _tmp: tmp,
            base,
            profile,
        }
    }

    fn seed_minimal_base(base: &Path) {
        // One file, one dir — covers both symlink kinds.
        fs::write(base.join("settings.json"), b"{}").unwrap();
        fs::create_dir_all(base.join("plugins")).unwrap();
        fs::write(base.join("plugins/p.txt"), b"x").unwrap();
    }

    #[test]
    fn creates_missing_symlinks() {
        let env = setup();
        seed_minimal_base(&env.base);
        let report = heal(&env.base, &env.profile).unwrap();
        assert!(env.profile.join("settings.json").is_symlink());
        assert!(env.profile.join("plugins").is_symlink());
        // The symlink should resolve to base content.
        assert_eq!(
            fs::read(env.profile.join("plugins/p.txt")).unwrap(),
            b"x".to_vec()
        );
        assert!(report.count(HealAction::Created) >= 2);
    }

    #[test]
    fn idempotent_rerun_is_noop() {
        let env = setup();
        seed_minimal_base(&env.base);
        heal(&env.base, &env.profile).unwrap();
        let report = heal(&env.base, &env.profile).unwrap();
        assert_eq!(report.count(HealAction::Created), 0);
        assert_eq!(report.count(HealAction::Repaired), 0);
        assert!(report.count(HealAction::AlreadyOk) >= 2);
    }

    #[test]
    fn repairs_broken_symlink() {
        let env = setup();
        seed_minimal_base(&env.base);
        // Plant a symlink pointing nowhere.
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            env.base.join("does-not-exist"),
            env.profile.join("settings.json"),
        )
        .unwrap();
        let report = heal(&env.base, &env.profile).unwrap();
        assert!(env.profile.join("settings.json").is_symlink());
        assert_eq!(
            fs::read_link(env.profile.join("settings.json")).unwrap(),
            env.base.join("settings.json"),
        );
        assert!(report
            .actions
            .iter()
            .any(|(n, a)| n == "settings.json" && *a == HealAction::Repaired));
    }

    #[test]
    fn repairs_symlink_pointing_to_wrong_target() {
        let env = setup();
        seed_minimal_base(&env.base);
        let other = env._tmp.path().join("other.json");
        fs::write(&other, b"{}").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&other, env.profile.join("settings.json")).unwrap();
        let report = heal(&env.base, &env.profile).unwrap();
        assert_eq!(
            fs::read_link(env.profile.join("settings.json")).unwrap(),
            env.base.join("settings.json"),
        );
        assert!(report
            .actions
            .iter()
            .any(|(n, a)| n == "settings.json" && *a == HealAction::Repaired));
    }

    #[test]
    fn skips_real_file_never_clobbers() {
        let env = setup();
        seed_minimal_base(&env.base);
        // User has a real settings.json they care about.
        fs::write(env.profile.join("settings.json"), b"{\"mine\":true}").unwrap();
        let report = heal(&env.base, &env.profile).unwrap();
        // Real file still there, untouched, NOT a symlink.
        assert!(!env.profile.join("settings.json").is_symlink());
        assert_eq!(
            fs::read(env.profile.join("settings.json")).unwrap(),
            b"{\"mine\":true}".to_vec()
        );
        assert!(!report.warnings.is_empty());
        assert!(report
            .actions
            .iter()
            .any(|(n, a)| n == "settings.json" && *a == HealAction::SkippedRealFile));
    }

    #[test]
    fn skips_when_source_missing() {
        let env = setup();
        // No seeding. Every allowlist source is missing.
        let report = heal(&env.base, &env.profile).unwrap();
        assert!(report.count(HealAction::Created) == 0 && report.count(HealAction::Repaired) == 0);
        assert_eq!(report.count(HealAction::SkippedNoSource), ALLOWLIST.len());
    }

    #[test]
    fn absolute_target_paths() {
        let env = setup();
        seed_minimal_base(&env.base);
        heal(&env.base, &env.profile).unwrap();
        let target = fs::read_link(env.profile.join("settings.json")).unwrap();
        assert!(target.is_absolute(), "target was {:?}", target);
    }

    #[test]
    fn diagnose_does_not_mutate() {
        let env = setup();
        seed_minimal_base(&env.base);
        // Plant a broken symlink — heal would fix it, diagnose must NOT.
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            env.base.join("does-not-exist"),
            env.profile.join("settings.json"),
        )
        .unwrap();
        let report = diagnose(&env.base, &env.profile).unwrap();
        // Target still points at the non-existent source → still broken.
        assert_eq!(
            fs::read_link(env.profile.join("settings.json")).unwrap(),
            env.base.join("does-not-exist")
        );
        // And diagnose should have classified it as "would repair".
        assert!(report
            .actions
            .iter()
            .any(|(n, a)| n == "settings.json" && *a == HealAction::Repaired));
    }

    #[test]
    fn orphans_detects_unknown_entries() {
        let env = setup();
        fs::write(env.profile.join("random-file"), b"").unwrap();
        fs::write(env.profile.join(".credentials.json"), b"{}").unwrap(); // in keep list
        fs::create_dir_all(env.profile.join("settings.json")).ok(); // in allowlist — not orphan
        let names: Vec<String> = orphans(&env.profile)
            .unwrap()
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["random-file"]);
    }
}
