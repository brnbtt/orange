//! Reading the session written by `orange login`.
//!
//! The tray does not perform the OAuth flow itself; it shells out to the
//! `orange` binary, which owns that logic, and then reads the result. One
//! implementation of login, not two.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    pub name: String,
    #[allow(dead_code)]
    pub id: String,
    #[allow(dead_code)]
    pub avatar_url: Option<String>,
}

fn path() -> Option<PathBuf> {
    let dir = std::env::var("APPDATA").ok()?;
    Some(PathBuf::from(dir).join("orange").join("session.json"))
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
