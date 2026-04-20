//! TUI profile picker — arrow-key select with ESC to cancel.
//!
//! Not used when stdin/stdout aren't a TTY (CI, piped invocation); in that case
//! we print a hint and return `None` rather than erroring out.

use anyhow::Result;
use inquire::{InquireError, Select};

use crate::profile::{read_oauth_email, Paths, Registry, DEFAULT_PROFILE};

struct Item {
    name: String,
    label: String,
}

impl std::fmt::Display for Item {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// Show the picker; return the chosen profile name or `None` if cancelled / no TTY.
pub fn pick(paths: &Paths, reg: &Registry) -> Result<Option<String>> {
    let items = build_items(paths, reg);

    match Select::new("Pick a profile", items)
        .with_help_message("↑/↓ to navigate · Enter to launch claude · Esc to cancel")
        .prompt()
    {
        Ok(item) => Ok(Some(item.name)),
        Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => Ok(None),
        Err(InquireError::NotTTY) => {
            eprintln!(
                "ccsw: interactive picker needs a TTY — pass a profile name instead \
                 (e.g. `ccsw default`)."
            );
            Ok(None)
        }
        Err(err) => Err(err.into()),
    }
}

fn build_items(paths: &Paths, reg: &Registry) -> Vec<Item> {
    let mut items = Vec::with_capacity(reg.profiles.len() + 1);
    items.push(make_item(DEFAULT_PROFILE, &paths.base, None));
    for p in &reg.profiles {
        let dir = paths.profile_dir(&p.name);
        items.push(make_item(
            &p.name,
            &dir,
            p.last_used_at.map(|t| t.format("%Y-%m-%d").to_string()),
        ));
    }
    items
}

fn make_item(name: &str, dir: &std::path::Path, last_used: Option<String>) -> Item {
    let email = read_oauth_email(dir).unwrap_or_else(|| "no account".to_string());
    let last = last_used.unwrap_or_else(|| "—".to_string());
    let label = format!("{name:<14} · {email:<36} · last-used {last}");
    Item {
        name: name.to_string(),
        label,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn build_items_includes_default_first() {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::from_home(tmp.path());
        fs::create_dir_all(&paths.base).unwrap();
        paths.ensure_profiles_root().unwrap();

        let mut reg = Registry::default();
        reg.upsert(crate::profile::ProfileEntry {
            name: "work".to_string(),
            created_at: chrono::Utc::now(),
            last_used_at: None,
        });
        let items = build_items(&paths, &reg);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "default");
        assert_eq!(items[1].name, "work");
    }
}
