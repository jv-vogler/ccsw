//! Process launch: heal-on-launch, env setup, and `execve` replacement.
//!
//! `ccsw <name>` does not stay resident — it's replaced by the target command
//! (usually `claude`). On Unix this is `execvp`; on Windows we fall back to
//! spawn + wait since there's no cheap process replacement.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::profile::{self, Paths, CLAUDE_JSON, CREDENTIALS_FILE, DEFAULT_PROFILE};
use crate::symlinks::{self, HealAction};

pub const ENV_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";
pub const CLAUDE_BIN: &str = "claude";

/// Artifact of preparing a profile for launch.
#[derive(Debug, Clone)]
pub struct LaunchPrep {
    pub profile: String,
    pub profile_dir: PathBuf,
    pub is_default: bool,
    /// True if `.credentials.json` is missing — caller should hint `/login`.
    pub needs_login: bool,
}

/// Prepare a profile for launch: verify existence, heal symlinks, and classify credential state.
///
/// Does NOT fail if credentials are missing — `/login` is the legitimate way to create them.
pub fn prepare(paths: &Paths, profile_name: &str) -> Result<LaunchPrep> {
    let is_default = profile_name == DEFAULT_PROFILE;
    let profile_dir = paths.profile_dir(profile_name);

    if !is_default {
        profile::validate_profile_name(profile_name)?;
        if !profile_dir.is_dir() {
            bail!(
                "profile '{profile_name}' does not exist — create it with `ccsw add {profile_name}`"
            );
        }
    }

    if !is_default {
        let report = symlinks::heal(&paths.base, &profile_dir)
            .with_context(|| format!("healing profile '{profile_name}'"))?;
        for w in &report.warnings {
            eprintln!("ccsw: warning: {w}");
        }
        let repaired = report.count(HealAction::Repaired);
        if repaired > 0 {
            eprintln!("ccsw: repaired {repaired} symlink(s)");
        }
    }

    let needs_login = !is_default && !profile_dir.join(CREDENTIALS_FILE).exists();
    if needs_login {
        eprintln!(
            "ccsw: no credentials found for '{profile_name}'. \
             Run `/login` inside Claude Code to authenticate."
        );
    }
    let _ = profile_dir.join(CLAUDE_JSON);

    Ok(LaunchPrep {
        profile: profile_name.to_string(),
        profile_dir,
        is_default,
        needs_login,
    })
}

/// Build a [`Command`] with the profile environment already attached.
pub fn command_with_env(prep: &LaunchPrep, program: impl Into<OsString>) -> Command {
    let mut cmd = Command::new(program.into());
    if !prep.is_default {
        cmd.env(ENV_CONFIG_DIR, &prep.profile_dir);
    } else {
        cmd.env_remove(ENV_CONFIG_DIR);
    }
    cmd
}

/// Replace the current process with `claude` pinned to the profile.
pub fn replace_with_claude(
    prep: &LaunchPrep,
    extra_args: &[String],
) -> Result<std::convert::Infallible> {
    replace_process(prep, CLAUDE_BIN, extra_args)
}

/// Replace the current process with an arbitrary command pinned to the profile.
///
/// Unix uses `execvp` (argv array, no shell — no injection risk).
/// Windows spawns and waits since there is no cheap process-replacement syscall.
pub fn replace_process(
    prep: &LaunchPrep,
    program: &str,
    args: &[String],
) -> Result<std::convert::Infallible> {
    let mut cmd = command_with_env(prep, program);
    cmd.args(args);
    replace_process_impl(cmd, program)
}

#[cfg(unix)]
fn replace_process_impl(mut cmd: Command, program: &str) -> Result<std::convert::Infallible> {
    use std::os::unix::process::CommandExt;
    let err = cmd.exec();
    Err(err).with_context(|| format!("failed to launch `{program}` (is it on your PATH?)"))
}

#[cfg(windows)]
fn replace_process_impl(mut cmd: Command, program: &str) -> Result<std::convert::Infallible> {
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn `{program}`"))?;
    std::process::exit(status.code().unwrap_or(1));
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
    fn prepare_default_does_no_healing() {
        let (_tmp, paths) = setup();
        let prep = prepare(&paths, "default").unwrap();
        assert!(prep.is_default);
        assert_eq!(prep.profile_dir, paths.base);
        assert!(!prep.needs_login);
    }

    #[test]
    fn prepare_missing_profile_errors() {
        let (_tmp, paths) = setup();
        let err = prepare(&paths, "ghost").unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn prepare_heals_and_flags_missing_credentials() {
        let (_tmp, paths) = setup();
        let dir = paths.profile_dir("work");
        fs::create_dir_all(&dir).unwrap();
        fs::write(paths.base.join("settings.json"), b"{}").unwrap();
        let prep = prepare(&paths, "work").unwrap();
        assert!(!prep.is_default);
        assert!(dir.join("settings.json").is_symlink());
        assert!(prep.needs_login);
    }

    #[test]
    fn prepare_reports_logged_in_when_credentials_present() {
        let (_tmp, paths) = setup();
        let dir = paths.profile_dir("work");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".credentials.json"), b"{}").unwrap();
        let prep = prepare(&paths, "work").unwrap();
        assert!(!prep.needs_login);
    }

    #[test]
    fn command_with_env_sets_config_dir_for_non_default() {
        let (_tmp, paths) = setup();
        let dir = paths.profile_dir("work");
        fs::create_dir_all(&dir).unwrap();
        let prep = prepare(&paths, "work").unwrap();
        let cmd = command_with_env(&prep, "echo");
        let env: Vec<_> = cmd
            .get_envs()
            .filter(|(k, _)| *k == ENV_CONFIG_DIR)
            .collect();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].1.unwrap(), dir.as_os_str());
    }

    #[test]
    fn command_with_env_clears_config_dir_for_default() {
        let (_tmp, paths) = setup();
        let prep = prepare(&paths, "default").unwrap();
        let cmd = command_with_env(&prep, "echo");
        let env: Vec<_> = cmd
            .get_envs()
            .filter(|(k, _)| *k == ENV_CONFIG_DIR)
            .collect();
        assert_eq!(env.len(), 1);
        assert!(env[0].1.is_none(), "expected removal, got {:?}", env[0].1);
    }
}
