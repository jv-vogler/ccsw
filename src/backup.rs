//! Timestamped snapshots used as a safety net for destructive operations.
//!
//! Every mutating command (`add`, `rm`, `rename`, `restore`) takes a snapshot
//! first. Restore is itself reversible: it re-snapshots current state before
//! overwriting, so `ccsw restore <ts>` can always be undone by another restore.
//!
//! Snapshot layout:
//!
//! ```text
//! ~/.claude-profiles/.backups/<ts>-<op>-<name>/
//!   meta.json          {op, name, created_at}
//!   payload/           verbatim copy of the profile dir, symlinks preserved
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::profile::{Paths, Retention};

const META_FILE: &str = "meta.json";
const PAYLOAD_DIR: &str = "payload";

/// chrono format for snapshot timestamps: lexicographically sortable, filesystem-safe.
/// Example: `20260419T120101026Z` (UTC, 3-digit millisecond fraction).
const TS_FORMAT: &str = "%Y%m%dT%H%M%S%3fZ";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotMeta {
    pub op: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: String,
    pub path: PathBuf,
    pub meta: SnapshotMeta,
}

impl Snapshot {
    pub fn payload(&self) -> PathBuf {
        self.path.join(PAYLOAD_DIR)
    }
}

fn format_ts(ts: DateTime<Utc>) -> String {
    ts.format(TS_FORMAT).to_string()
}

fn ensure_backups_root(paths: &Paths) -> Result<PathBuf> {
    let root = paths.backups_root();
    fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    Ok(root)
}

/// Capture a profile directory — or a single file — into a new snapshot.
///
/// If `src` does not exist (e.g. pre-create), the payload will be an empty
/// directory and the meta still records the op — this is intentional so the
/// operation itself is reversible. A file source is copied into the payload
/// preserving its filename; this is used for registry-only snapshots, where
/// snapshotting the whole `profiles_root` would recurse into `.backups/`.
pub fn snapshot(paths: &Paths, src: &Path, op: &str, name: &str) -> Result<Snapshot> {
    let root = ensure_backups_root(paths)?;
    let created_at = Utc::now();
    let id = format!("{}-{}-{}", format_ts(created_at), op, name);
    let dest = root.join(&id);
    fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;

    let payload = dest.join(PAYLOAD_DIR);
    fs::create_dir_all(&payload)?;
    if src.is_dir() {
        copy_tree_preserving_symlinks(src, &payload)
            .with_context(|| format!("snapshotting {}", src.display()))?;
    } else if src.is_file() {
        let file_name = src
            .file_name()
            .ok_or_else(|| anyhow!("{} has no file name", src.display()))?;
        fs::copy(src, payload.join(file_name))
            .with_context(|| format!("snapshotting {}", src.display()))?;
    }

    let meta = SnapshotMeta {
        op: op.to_string(),
        name: name.to_string(),
        created_at,
    };
    fs::write(dest.join(META_FILE), serde_json::to_vec_pretty(&meta)?)?;

    Ok(Snapshot {
        id,
        path: dest,
        meta,
    })
}

/// List snapshots, newest first.
pub fn list(paths: &Paths) -> Result<Vec<Snapshot>> {
    let root = paths.backups_root();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        let meta_path = path.join(META_FILE);
        let id = entry.file_name().to_string_lossy().into_owned();
        let meta = match fs::read(&meta_path) {
            Ok(bytes) => match serde_json::from_slice::<SnapshotMeta>(&bytes) {
                Ok(m) => m,
                Err(_) => continue, // skip malformed
            },
            Err(_) => continue,
        };
        out.push(Snapshot { id, path, meta });
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(out)
}

/// Find one snapshot by full id or unique prefix. Ambiguous prefix → error.
pub fn find(paths: &Paths, needle: &str) -> Result<Snapshot> {
    let all = list(paths)?;
    let exact: Vec<_> = all.iter().filter(|s| s.id == needle).collect();
    if exact.len() == 1 {
        return Ok(exact[0].clone());
    }
    let matches: Vec<_> = all.iter().filter(|s| s.id.starts_with(needle)).collect();
    match matches.len() {
        0 => Err(anyhow!("no snapshot matches '{needle}'")),
        1 => Ok(matches[0].clone()),
        n => Err(anyhow!(
            "{n} snapshots match '{needle}'; pass a longer prefix"
        )),
    }
}

/// Restore a snapshot into its original profile directory. Takes a pre-restore
/// snapshot of the current state first so the restore itself is undoable.
pub fn restore(paths: &Paths, needle: &str) -> Result<Snapshot> {
    let snap = find(paths, needle)?;
    let target_profile = paths.profile_dir(&snap.meta.name);

    // 1. Save current state first (reversibility).
    let _pre = snapshot(paths, &target_profile, "pre-restore", &snap.meta.name)?;

    // 2. Clear target and replace with payload.
    if target_profile.exists() {
        remove_dir_contents(&target_profile)
            .with_context(|| format!("clearing {}", target_profile.display()))?;
    } else {
        fs::create_dir_all(&target_profile)?;
    }
    let payload = snap.payload();
    if payload.is_dir() {
        copy_tree_preserving_symlinks(&payload, &target_profile)
            .with_context(|| format!("restoring into {}", target_profile.display()))?;
    }

    Ok(snap)
}

/// Prune snapshots according to retention. Keeps the union of:
///   1. the most recent `max_snapshots` by id
///   2. every snapshot newer than `max_days`
///
/// Returns the ids that were pruned.
pub fn rotate(paths: &Paths, retention: &Retention, now: DateTime<Utc>) -> Result<Vec<String>> {
    let all = list(paths)?; // newest first
    if all.is_empty() {
        return Ok(Vec::new());
    }
    let cutoff = now - Duration::days(retention.max_days);

    let mut keep = std::collections::HashSet::new();
    for snap in all.iter().take(retention.max_snapshots) {
        keep.insert(snap.id.clone());
    }
    for snap in &all {
        if snap.meta.created_at >= cutoff {
            keep.insert(snap.id.clone());
        }
    }

    let mut pruned = Vec::new();
    for snap in all {
        if keep.contains(&snap.id) {
            continue;
        }
        fs::remove_dir_all(&snap.path)
            .with_context(|| format!("pruning {}", snap.path.display()))?;
        pruned.push(snap.id);
    }
    Ok(pruned)
}

fn copy_tree_preserving_symlinks(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if ft.is_symlink() {
            let target = fs::read_link(&src_path)?;
            recreate_symlink(&target, &dest_path, &src_path)?;
        } else if ft.is_dir() {
            copy_tree_preserving_symlinks(&src_path, &dest_path)?;
        } else {
            fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn recreate_symlink(target: &Path, dest: &Path, _original: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;
    symlink(target, dest).with_context(|| {
        format!(
            "recreating symlink {} → {}",
            dest.display(),
            target.display()
        )
    })
}

#[cfg(windows)]
fn recreate_symlink(target: &Path, dest: &Path, original: &Path) -> Result<()> {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    // Determine kind by resolving the original — target may be relative.
    let is_dir = fs::metadata(original).map(|m| m.is_dir()).unwrap_or(false);
    if is_dir {
        symlink_dir(target, dest)
    } else {
        symlink_file(target, dest)
    }
    .with_context(|| {
        format!(
            "recreating symlink {} → {}",
            dest.display(),
            target.display()
        )
    })
}

fn remove_dir_contents(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() && !ft.is_symlink() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Paths) {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::from_home(tmp.path());
        paths.ensure_profiles_root().unwrap();
        fs::create_dir_all(&paths.base).unwrap();
        (tmp, paths)
    }

    fn seed_profile(paths: &Paths, name: &str) -> PathBuf {
        let dir = paths.profile_dir(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".credentials.json"), b"creds").unwrap();
        // A symlink to base — must be preserved, not followed.
        fs::write(paths.base.join("settings.json"), b"base-settings").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(paths.base.join("settings.json"), dir.join("settings.json"))
            .unwrap();
        dir
    }

    #[test]
    fn snapshot_from_file_puts_only_that_file_in_payload() {
        let (_tmp, paths) = setup();
        // Seed the registry file and snapshot it by path.
        let registry = paths.registry_file();
        fs::write(&registry, br#"{"version":1,"profiles":[]}"#).unwrap();

        let snap = snapshot(&paths, &registry, "pre-add", "work").unwrap();

        let payload = snap.payload();
        assert!(payload.is_dir());
        let entries: Vec<_> = fs::read_dir(&payload)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec![".ccsw.json"]);
        assert_eq!(
            fs::read(payload.join(".ccsw.json")).unwrap(),
            br#"{"version":1,"profiles":[]}"#.to_vec()
        );
    }

    #[test]
    fn snapshot_captures_payload_and_meta() {
        let (_tmp, paths) = setup();
        let dir = seed_profile(&paths, "work");
        let snap = snapshot(&paths, &dir, "rm", "work").unwrap();
        assert!(snap.path.join("meta.json").is_file());
        let payload = snap.payload();
        assert!(payload.join(".credentials.json").is_file());
        // Symlink preserved, not dereferenced.
        assert!(payload.join("settings.json").is_symlink());
    }

    #[test]
    fn list_sorts_newest_first() {
        let (_tmp, paths) = setup();
        let dir = seed_profile(&paths, "work");
        let a = snapshot(&paths, &dir, "rm", "work").unwrap();
        // chrono's %3f precision means same-millisecond collisions are possible in tight loops.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = snapshot(&paths, &dir, "rm", "work").unwrap();
        let all = list(&paths).unwrap();
        assert_eq!(all[0].id, b.id);
        assert_eq!(all[1].id, a.id);
    }

    #[test]
    fn find_by_prefix_and_exact() {
        let (_tmp, paths) = setup();
        let dir = seed_profile(&paths, "work");
        let s = snapshot(&paths, &dir, "rm", "work").unwrap();
        let by_exact = find(&paths, &s.id).unwrap();
        assert_eq!(by_exact.id, s.id);
        // Prefix matching the timestamp head.
        let by_prefix = find(&paths, &s.id[..10]).unwrap();
        assert_eq!(by_prefix.id, s.id);
    }

    #[test]
    fn restore_round_trip_takes_pre_restore_snapshot() {
        let (_tmp, paths) = setup();
        let dir = seed_profile(&paths, "work");
        // Snapshot the initial state.
        let s = snapshot(&paths, &dir, "rm", "work").unwrap();
        // Mutate current state.
        fs::write(dir.join(".credentials.json"), b"NEW").unwrap();
        // Restore to original.
        restore(&paths, &s.id).unwrap();
        assert_eq!(
            fs::read(dir.join(".credentials.json")).unwrap(),
            b"creds".to_vec()
        );
        // A pre-restore snapshot also exists capturing the mutated state.
        let all = list(&paths).unwrap();
        let pre = all
            .iter()
            .find(|x| x.meta.op == "pre-restore")
            .expect("pre-restore snapshot present");
        assert_eq!(
            fs::read(pre.payload().join(".credentials.json")).unwrap(),
            b"NEW".to_vec()
        );
    }

    #[test]
    fn rotate_keeps_recent_count_even_when_all_old() {
        let (_tmp, paths) = setup();
        // Manually plant 5 old snapshots (backdated via meta) with real dirs + meta files.
        let root = ensure_backups_root(&paths).unwrap();
        let now = Utc::now();
        for i in 0..5 {
            let ts = now - Duration::days(100 + i);
            let id = format!("{}-rm-old", format_ts(ts));
            let dir = root.join(&id);
            fs::create_dir_all(dir.join("payload")).unwrap();
            let meta = SnapshotMeta {
                op: "rm".into(),
                name: "old".into(),
                created_at: ts,
            };
            fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
        }
        // max_snapshots=3 wins over max_days=30 (everything is older than 30 days).
        let retention = Retention {
            max_snapshots: 3,
            max_days: 30,
        };
        let pruned = rotate(&paths, &retention, now).unwrap();
        assert_eq!(pruned.len(), 2, "pruned 5 - max(3,0) = 2, got {:?}", pruned);
        assert_eq!(list(&paths).unwrap().len(), 3);
    }

    #[test]
    fn rotate_keeps_everything_within_days_even_over_count() {
        let (_tmp, paths) = setup();
        let root = ensure_backups_root(&paths).unwrap();
        let now = Utc::now();
        // 5 recent snapshots, all within the last day.
        for i in 0..5 {
            let ts = now - Duration::minutes(i);
            let id = format!("{}-rm-recent-{}", format_ts(ts), i);
            let dir = root.join(&id);
            fs::create_dir_all(dir.join("payload")).unwrap();
            let meta = SnapshotMeta {
                op: "rm".into(),
                name: format!("recent-{}", i),
                created_at: ts,
            };
            fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
        }
        // max_snapshots=1 would drop 4, but max_days=30 keeps all 5.
        let retention = Retention {
            max_snapshots: 1,
            max_days: 30,
        };
        let pruned = rotate(&paths, &retention, now).unwrap();
        assert!(pruned.is_empty(), "pruned {:?}", pruned);
        assert_eq!(list(&paths).unwrap().len(), 5);
    }
}
