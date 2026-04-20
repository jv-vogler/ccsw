//! Profile layout, path resolution, and on-disk registry.
//!
//! The base profile (`~/.claude`) is addressable as the reserved name `default`
//! and is not present in the registry. All other profiles live under
//! `~/.claude-profiles/<name>/` and have an entry in `.ccsw.json`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PROFILE: &str = "default";
pub const REGISTRY_FILE: &str = ".ccsw.json";
pub const BACKUPS_DIR: &str = ".backups";
pub const CREDENTIALS_FILE: &str = ".credentials.json";
pub const CLAUDE_JSON: &str = ".claude.json";

/// Default retention: keep at least this many snapshots even if older than [`DEFAULT_MAX_DAYS`].
pub const DEFAULT_MAX_SNAPSHOTS: usize = 20;
/// Default retention: keep snapshots at least this many days even if more than [`DEFAULT_MAX_SNAPSHOTS`].
pub const DEFAULT_MAX_DAYS: i64 = 30;

/// Resolved filesystem layout. Every path is absolute.
#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub base: PathBuf,
    pub profiles_root: PathBuf,
}

impl Paths {
    /// Resolve from the real home directory (`$HOME`).
    pub fn from_env() -> Result<Self> {
        let home = dirs::home_dir().context("could not resolve home directory")?;
        Ok(Self::from_home(home))
    }

    /// Resolve from an explicit home. Used in tests.
    pub fn from_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let base = home.join(".claude");
        let profiles_root = home.join(".claude-profiles");
        Self {
            home,
            base,
            profiles_root,
        }
    }

    pub fn registry_file(&self) -> PathBuf {
        self.profiles_root.join(REGISTRY_FILE)
    }

    pub fn backups_root(&self) -> PathBuf {
        self.profiles_root.join(BACKUPS_DIR)
    }

    /// Directory for a profile. `default` → base; any other name → `<profiles_root>/<name>`.
    pub fn profile_dir(&self, name: &str) -> PathBuf {
        if name == DEFAULT_PROFILE {
            self.base.clone()
        } else {
            self.profiles_root.join(name)
        }
    }

    pub fn ensure_profiles_root(&self) -> Result<()> {
        fs::create_dir_all(&self.profiles_root)
            .with_context(|| format!("creating {}", self.profiles_root.display()))?;
        Ok(())
    }
}

/// Names must be a single path segment, non-empty, and free of path separators.
pub fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("profile name must not be empty");
    }
    if name == DEFAULT_PROFILE {
        bail!("`default` is a reserved name for the base profile");
    }
    if name.starts_with('.') {
        bail!("profile name must not start with '.'");
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        bail!("profile name must not contain path separators");
    }
    if name.contains(char::is_whitespace) {
        bail!("profile name must not contain whitespace");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileEntry {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Retention {
    pub max_snapshots: usize,
    pub max_days: i64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_snapshots: DEFAULT_MAX_SNAPSHOTS,
            max_days: DEFAULT_MAX_DAYS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Registry {
    pub version: u32,
    pub profiles: Vec<ProfileEntry>,
    #[serde(default)]
    pub retention: Retention,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            version: 1,
            profiles: Vec::new(),
            retention: Retention::default(),
        }
    }
}

impl Registry {
    /// Load the registry. On missing file → default. On parse failure → rebuild from filesystem scan.
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.registry_file();
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Registry>(&bytes) {
                Ok(r) => Ok(r),
                Err(err) => {
                    eprintln!(
                        "ccsw: registry at {} is corrupt ({err}); rebuilding from filesystem",
                        path.display()
                    );
                    Self::rebuild_from_scan(paths)
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Scan `profiles_root` for directories and synthesize a registry. Used only as a recovery path.
    pub fn rebuild_from_scan(paths: &Paths) -> Result<Self> {
        let mut profiles = Vec::new();
        if paths.profiles_root.exists() {
            for entry in fs::read_dir(&paths.profiles_root)
                .with_context(|| format!("reading {}", paths.profiles_root.display()))?
            {
                let entry = entry?;
                let ft = entry.file_type()?;
                if !ft.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue; // skip .backups, hidden
                }
                let created_at = entry
                    .metadata()
                    .and_then(|m| m.created().or_else(|_| m.modified()))
                    .ok()
                    .and_then(|t| DateTime::<Utc>::from(t).into())
                    .unwrap_or_else(Utc::now);
                profiles.push(ProfileEntry {
                    name,
                    created_at,
                    last_used_at: None,
                });
            }
        }
        profiles.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            version: 1,
            profiles,
            retention: Retention::default(),
        })
    }

    /// Atomic save: write-to-tmp in the same directory, then rename.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        paths.ensure_profiles_root()?;
        let dest = paths.registry_file();
        let tmp = dest.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(self)?;
        fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &dest)
            .with_context(|| format!("renaming {} to {}", tmp.display(), dest.display()))?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&ProfileEntry> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut ProfileEntry> {
        self.profiles.iter_mut().find(|p| p.name == name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn upsert(&mut self, entry: ProfileEntry) {
        if let Some(slot) = self.get_mut(&entry.name) {
            *slot = entry;
        } else {
            self.profiles.push(entry);
        }
        self.profiles.sort_by(|a, b| a.name.cmp(&b.name));
    }

    pub fn remove(&mut self, name: &str) -> Option<ProfileEntry> {
        let idx = self.profiles.iter().position(|p| p.name == name)?;
        Some(self.profiles.remove(idx))
    }

    pub fn touch(&mut self, name: &str) {
        if let Some(p) = self.get_mut(name) {
            p.last_used_at = Some(Utc::now());
        }
    }
}

/// Read the OAuth account email out of a profile's `.claude.json`, if present.
///
/// Returns `None` for any failure mode (file missing, unreadable, invalid JSON,
/// or the expected keys absent). Used by `ls` and the picker to annotate rows.
pub fn read_oauth_email(profile_dir: &Path) -> Option<String> {
    let bytes = fs::read(profile_dir.join(CLAUDE_JSON)).ok()?;
    let val: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    val.get("oauthAccount")?
        .get("emailAddress")?
        .as_str()
        .map(str::to_string)
}

/// Copy `~/.claude/.claude.json` into the new profile directory so that UI prefs,
/// tips state, and onboarding flags carry over at create time.
///
/// Missing source is not an error — the profile will simply have no `.claude.json`
/// until `/login` runs and Claude Code writes one.
pub fn copy_forward_claude_json(base: &Path, dest_profile_dir: &Path) -> Result<bool> {
    let src = base.join(CLAUDE_JSON);
    if !src.exists() {
        return Ok(false);
    }
    let dest = dest_profile_dir.join(CLAUDE_JSON);
    fs::copy(&src, &dest)
        .with_context(|| format!("copying {} → {}", src.display(), dest.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Paths) {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::from_home(tmp.path());
        fs::create_dir_all(&paths.base).unwrap();
        paths.ensure_profiles_root().unwrap();
        (tmp, paths)
    }

    #[test]
    fn default_name_maps_to_base() {
        let (_tmp, paths) = setup();
        assert_eq!(paths.profile_dir("default"), paths.base);
    }

    #[test]
    fn named_profile_maps_under_profiles_root() {
        let (_tmp, paths) = setup();
        assert_eq!(paths.profile_dir("work"), paths.profiles_root.join("work"),);
    }

    #[test]
    fn validate_rejects_bad_names() {
        assert!(validate_profile_name("").is_err());
        assert!(validate_profile_name("default").is_err());
        assert!(validate_profile_name(".hidden").is_err());
        assert!(validate_profile_name("a/b").is_err());
        assert!(validate_profile_name("a\\b").is_err());
        assert!(validate_profile_name("with space").is_err());
        assert!(validate_profile_name("ok_name-1").is_ok());
    }

    #[test]
    fn registry_save_load_roundtrip() {
        let (_tmp, paths) = setup();
        let mut reg = Registry::default();
        reg.upsert(ProfileEntry {
            name: "work".to_string(),
            created_at: Utc::now(),
            last_used_at: None,
        });
        reg.save(&paths).unwrap();
        let loaded = Registry::load(&paths).unwrap();
        assert_eq!(loaded, reg);
    }

    #[test]
    fn missing_registry_loads_as_default() {
        let (_tmp, paths) = setup();
        let reg = Registry::load(&paths).unwrap();
        assert!(reg.profiles.is_empty());
        assert_eq!(reg.retention, Retention::default());
    }

    #[test]
    fn corrupt_registry_rebuilds_from_scan() {
        let (_tmp, paths) = setup();
        // Create two profile dirs + a hidden dir that should be ignored.
        fs::create_dir_all(paths.profiles_root.join("alpha")).unwrap();
        fs::create_dir_all(paths.profiles_root.join("beta")).unwrap();
        fs::create_dir_all(paths.backups_root()).unwrap();
        fs::write(paths.registry_file(), b"not json").unwrap();

        let reg = Registry::load(&paths).unwrap();
        let names: Vec<_> = reg.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn remove_and_touch() {
        let mut reg = Registry::default();
        reg.upsert(ProfileEntry {
            name: "a".into(),
            created_at: Utc::now(),
            last_used_at: None,
        });
        assert!(reg.contains("a"));
        reg.touch("a");
        assert!(reg.get("a").unwrap().last_used_at.is_some());
        assert!(reg.remove("a").is_some());
        assert!(!reg.contains("a"));
    }

    #[test]
    fn copy_forward_claude_json_skips_when_missing() {
        let (_tmp, paths) = setup();
        let prof = paths.profiles_root.join("x");
        fs::create_dir_all(&prof).unwrap();
        let copied = copy_forward_claude_json(&paths.base, &prof).unwrap();
        assert!(!copied);
    }

    #[test]
    fn copy_forward_claude_json_copies_when_present() {
        let (_tmp, paths) = setup();
        fs::write(paths.base.join(CLAUDE_JSON), b"{\"k\":1}").unwrap();
        let prof = paths.profiles_root.join("x");
        fs::create_dir_all(&prof).unwrap();
        let copied = copy_forward_claude_json(&paths.base, &prof).unwrap();
        assert!(copied);
        assert_eq!(
            fs::read(prof.join(CLAUDE_JSON)).unwrap(),
            b"{\"k\":1}".to_vec()
        );
    }
}
