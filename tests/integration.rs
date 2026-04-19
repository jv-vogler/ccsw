//! End-to-end tests that drive the real `ccsw` binary against an isolated $HOME.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

struct Env {
    _tmp: TempDir,
    home: PathBuf,
    profiles_root: PathBuf,
}

fn setup() -> Env {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let base = home.join(".claude");
    let profiles_root = home.join(".claude-profiles");
    fs::create_dir_all(&base).unwrap();
    fs::create_dir_all(&profiles_root).unwrap();
    // Seed a couple of shared entries so symlinks have something to link to.
    fs::write(base.join("settings.json"), b"{\"seeded\":true}").unwrap();
    fs::create_dir_all(base.join("plugins")).unwrap();
    fs::write(base.join("plugins/p.txt"), b"shared").unwrap();
    fs::write(base.join("CLAUDE.md"), b"# base").unwrap();
    let _ = base; // seeded above; not referenced again through Env
    Env {
        _tmp: tmp,
        home,
        profiles_root,
    }
}

fn ccsw(env: &Env) -> Command {
    let mut cmd = Command::cargo_bin("ccsw").unwrap();
    cmd.env("HOME", &env.home)
        // macOS uses HOME; Windows uses USERPROFILE — mirror for safety.
        .env("USERPROFILE", &env.home)
        // Make sure no ambient CLAUDE_CONFIG_DIR bleeds in.
        .env_remove("CLAUDE_CONFIG_DIR");
    cmd
}

fn profile_dir(env: &Env, name: &str) -> PathBuf {
    env.profiles_root.join(name)
}

fn is_symlink(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

#[test]
fn add_creates_profile_with_symlinks_and_registry() {
    let env = setup();
    ccsw(&env).args(["add", "work"]).assert().success();

    let dir = profile_dir(&env, "work");
    assert!(dir.is_dir());
    assert!(is_symlink(&dir.join("settings.json")));
    assert!(is_symlink(&dir.join("plugins")));
    assert!(is_symlink(&dir.join("CLAUDE.md")));
    // The symlink actually resolves to base content.
    assert_eq!(
        fs::read(dir.join("plugins/p.txt")).unwrap(),
        b"shared".to_vec()
    );
    // Registry records the profile.
    let reg: serde_json::Value =
        serde_json::from_slice(&fs::read(env.profiles_root.join(".ccsw.json")).unwrap()).unwrap();
    let names: Vec<&str> = reg["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["work"]);
}

#[test]
fn ls_shows_default_and_added_profile() {
    let env = setup();
    ccsw(&env).args(["add", "work"]).assert().success();
    ccsw(&env)
        .args(["ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("default"))
        .stdout(predicate::str::contains("work"));
}

#[test]
fn add_rejects_duplicate() {
    let env = setup();
    ccsw(&env).args(["add", "work"]).assert().success();
    ccsw(&env).args(["add", "work"]).assert().failure();
}

#[test]
fn add_rejects_reserved_default() {
    let env = setup();
    ccsw(&env)
        .args(["add", "default"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("reserved"));
}

#[test]
fn rm_creates_snapshot_and_removes_profile() {
    let env = setup();
    ccsw(&env).args(["add", "work"]).assert().success();
    ccsw(&env).args(["rm", "work", "--yes"]).assert().success();

    assert!(!profile_dir(&env, "work").exists());
    let backups_root = env.profiles_root.join(".backups");
    let snapshot = fs::read_dir(&backups_root)
        .unwrap()
        .next()
        .expect("a backup directory exists")
        .unwrap();
    assert!(snapshot.file_name().to_string_lossy().contains("rm-work"));
}

#[test]
fn rm_refuses_default() {
    let env = setup();
    ccsw(&env)
        .args(["rm", "default", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("default"));
}

#[test]
fn restore_round_trip_recovers_profile() {
    let env = setup();
    ccsw(&env).args(["add", "work"]).assert().success();
    ccsw(&env).args(["rm", "work", "--yes"]).assert().success();

    // Grab the snapshot id and restore from it.
    let backups_root = env.profiles_root.join(".backups");
    let snap = fs::read_dir(&backups_root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let id = snap.file_name().to_string_lossy().into_owned();

    ccsw(&env).args(["restore", &id]).assert().success();
    assert!(profile_dir(&env, "work").is_dir());
    // Symlinks are preserved in the restored payload.
    assert!(is_symlink(&profile_dir(&env, "work").join("settings.json")));
}

#[test]
fn run_exports_config_dir_to_subprocess() {
    let env = setup();
    ccsw(&env).args(["add", "work"]).assert().success();
    let expected = profile_dir(&env, "work");

    // `ccsw run work -- env` replaces ccsw with env; env prints the inherited environment.
    // We only grep for the CLAUDE_CONFIG_DIR line.
    let expected_substring = format!("CLAUDE_CONFIG_DIR={}", expected.display());
    ccsw(&env)
        .args(["run", "work", "--", "env"])
        .assert()
        .success()
        .stdout(predicate::str::contains(expected_substring));
}

#[test]
fn current_reports_default_without_env() {
    let env = setup();
    ccsw(&env)
        .args(["current"])
        .assert()
        .success()
        .stdout(predicate::str::contains("default"));
}

#[test]
fn current_reports_profile_when_env_set() {
    let env = setup();
    ccsw(&env).args(["add", "work"]).assert().success();
    let dir = profile_dir(&env, "work");
    ccsw(&env)
        .env("CLAUDE_CONFIG_DIR", &dir)
        .args(["current"])
        .assert()
        .success()
        .stdout(predicate::str::contains("work"));
}

#[test]
fn sync_heals_broken_symlink() {
    let env = setup();
    ccsw(&env).args(["add", "work"]).assert().success();
    let dir = profile_dir(&env, "work");
    // Break a symlink.
    fs::remove_file(dir.join("settings.json")).unwrap();
    ccsw(&env).args(["sync"]).assert().success();
    assert!(is_symlink(&dir.join("settings.json")));
}

#[test]
fn doctor_is_read_only() {
    let env = setup();
    ccsw(&env).args(["add", "work"]).assert().success();
    let dir = profile_dir(&env, "work");
    // Break a symlink so doctor has something to report.
    fs::remove_file(dir.join("settings.json")).unwrap();

    ccsw(&env).args(["doctor"]).assert().success();
    // Doctor did NOT heal — the entry is still missing.
    assert!(!dir.join("settings.json").exists());
}

#[test]
fn rename_moves_profile_and_updates_registry() {
    let env = setup();
    ccsw(&env).args(["add", "work"]).assert().success();
    ccsw(&env)
        .args(["rename", "work", "company"])
        .assert()
        .success();
    assert!(!profile_dir(&env, "work").exists());
    assert!(profile_dir(&env, "company").is_dir());

    let reg: serde_json::Value =
        serde_json::from_slice(&fs::read(env.profiles_root.join(".ccsw.json")).unwrap()).unwrap();
    let names: Vec<&str> = reg["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["company"]);
}

#[test]
fn completions_emits_bash_script() {
    let env = setup();
    ccsw(&env)
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("_ccsw"));
}
