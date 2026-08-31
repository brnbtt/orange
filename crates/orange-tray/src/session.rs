//! Reading the session written by `orange login`.
//!
//! The tray does not perform the OAuth flow itself; it shells out to the
//! `orange` binary, which owns that logic, and then reads the result. One
//! implementation of login, not two.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    pub name: String,
    #[allow(dead_code)]
    pub id: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub quality: usize,
    /// `None` follows the captured window's display refresh rate.
    pub fps: Option<u32>,
    pub own_codes: Vec<String>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            quality: 1,
            fps: None,
            own_codes: Vec::new(),
        }
    }
}

fn path() -> Option<PathBuf> {
    let dir = std::env::var("APPDATA").ok()?;
    Some(PathBuf::from(dir).join("orange").join("session.json"))
}

fn preferences_path() -> Result<PathBuf> {
    let dir = std::env::var("APPDATA").context("APPDATA is not set")?;
    Ok(PathBuf::from(dir).join("orange").join("preferences.json"))
}

pub fn load() -> Option<Session> {
    let text = std::fs::read_to_string(path()?).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn clear() -> Result<()> {
    clear_path(&path().context("APPDATA is not set")?)
}

fn clear_path(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove session {}", path.display())),
    }
}

pub fn load_preferences() -> Result<Preferences> {
    load_preferences_from(&preferences_path()?)
}

fn load_preferences_from(path: &Path) -> Result<Preferences> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Preferences::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read preferences {}", path.display()));
        }
    };
    serde_json::from_str(&text).with_context(|| format!("parse preferences {}", path.display()))
}

pub fn save_preferences(preferences: &Preferences) -> Result<()> {
    save_preferences_to(&preferences_path()?, preferences)
}

fn save_preferences_to(path: &Path, preferences: &Preferences) -> Result<()> {
    let parent = path.parent().context("preferences path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create preferences directory {}", parent.display()))?;
    let json = serde_json::to_string_pretty(preferences).context("serialize preferences")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary preferences file in {}", parent.display()))?;
    temporary
        .write_all(json.as_bytes())
        .context("write temporary preferences file")?;
    temporary
        .flush()
        .context("flush temporary preferences file")?;
    temporary
        .as_file()
        .sync_all()
        .context("sync temporary preferences file")?;
    temporary
        .persist(path)
        .with_context(|| format!("replace preferences {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_preferences_use_defaults() {
        let dir = tempfile::tempdir().unwrap();

        let preferences = load_preferences_from(&dir.path().join("preferences.json")).unwrap();

        assert_eq!(preferences.quality, 1);
        assert_eq!(preferences.fps, None);
        assert!(preferences.own_codes.is_empty());
    }

    #[test]
    fn malformed_existing_preferences_are_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, "not json").unwrap();

        let error = load_preferences_from(&path).unwrap_err();

        assert!(error.to_string().contains("parse preferences"));
    }

    #[test]
    fn atomic_save_replaces_existing_json_without_leaving_a_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, "old contents").unwrap();
        let preferences = Preferences {
            quality: 2,
            fps: Some(120),
            own_codes: vec!["ORANGE".into()],
        };

        save_preferences_to(&path, &preferences).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["quality"], 2);
        assert_eq!(json["fps"], 120);
        assert_eq!(json["own_codes"], serde_json::json!(["ORANGE"]));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn clearing_a_missing_session_succeeds() {
        let dir = tempfile::tempdir().unwrap();

        clear_path(&dir.path().join("session.json")).unwrap();
    }

    #[test]
    fn clearing_an_existing_session_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(&path, "session").unwrap();

        clear_path(&path).unwrap();

        assert!(!path.exists());
    }
}
