//! Reading the session written by `orange login`.
//!
//! The tray does not perform the OAuth flow itself; it shells out to the
//! `orange` binary, which owns that logic, and then reads the result. One
//! implementation of login, not two.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

fn preferences_path() -> Option<PathBuf> {
    let dir = std::env::var("APPDATA").ok()?;
    Some(PathBuf::from(dir).join("orange").join("preferences.json"))
}

pub fn load() -> Option<Session> {
    let text = std::fs::read_to_string(path()?).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn clear() {
    if let Some(path) = path() {
        let _ = std::fs::remove_file(path);
    }
}

pub fn load_preferences() -> Preferences {
    let Some(path) = preferences_path() else {
        return Preferences::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save_preferences(preferences: Preferences) {
    let Some(path) = preferences_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&preferences) {
        let _ = std::fs::write(path, json);
    }
}
