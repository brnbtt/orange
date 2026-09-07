//! Reading the session written by `orange login`.
//!
//! The client does not perform the OAuth flow itself; it shells out to the
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
    /// Stable Discord id, shared in personal friend codes.
    pub id: String,
    pub avatar_url: Option<String>,
    /// Relay session token. The client reads it only to authenticate presence
    /// polls; it still never performs or refreshes a login. `orange login`
    /// remains the sole writer of this file.
    pub token: String,
}

/// Someone whose stream this machine wants to be told about.
///
/// Cached profile for rendering while offline. Accepted friendships and
/// pending requests are owned by the relay; this copy never grants access.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Friend {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

impl Session {
    pub fn friend_code(&self) -> Result<String> {
        let code = format!("orange-friend:1:{}:{}", self.id, self.name.trim());
        Friend::from_code(&code)?;
        Ok(code)
    }
}

impl Friend {
    pub fn from_code(code: &str) -> Result<Self> {
        anyhow::ensure!(code.len() <= 512, "friend code is too long");
        let (id, name) = code
            .trim()
            .strip_prefix("orange-friend:1:")
            .and_then(|profile| profile.split_once(':'))
            .context("copy a personal friend code from Orange, then try again")?;
        let number = id
            .parse::<u64>()
            .context("invalid Discord id in friend code")?;
        anyhow::ensure!(
            number != 0 && number.to_string() == id,
            "invalid Discord id in friend code"
        );
        let name = name.trim();
        anyhow::ensure!(
            !name.is_empty() && name.chars().count() <= 128 && !name.chars().any(char::is_control),
            "invalid name in friend code"
        );
        // Codes carry a shared display name, not authenticated credentials or
        // a download URL. Presence refreshes the profile from the relay later.
        Ok(Self {
            id: id.into(),
            name: name.into(),
            avatar_url: None,
        })
    }
}

/// Retain distinct people until the UI can offer them. Bound both the child
/// inbox and the UI backlog so repeated joins cannot grow them indefinitely.
pub(crate) fn remember_friend(offers: &mut Vec<Friend>, friend: Friend) {
    if let Some(existing) = offers.iter_mut().find(|existing| existing.id == friend.id) {
        *existing = friend;
    } else {
        if offers.len() == 100 {
            offers.remove(0);
        }
        offers.push(friend);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub quality: usize,
    /// `None` follows the captured window's display refresh rate.
    /// The rate the picker last wrote. `None` predates the 120 fps cap, when
    /// the default was to follow the captured display's refresh rate;
    /// `supervisor::supported_frame_rate` resolves it to an offered rate.
    pub fps: Option<u32>,
    pub own_codes: Vec<String>,
    pub friends: Vec<Friend>,
    pub friend_accounts: std::collections::HashMap<String, AccountFriends>,
    pub friends_panel_collapsed: bool,
    /// Explicit UI language. Absent on older files; the client then follows
    /// the Windows display language and writes the choice on the next save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<crate::i18n::Locale>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountFriends {
    pub friends: Vec<Friend>,
    pub suggestions: Vec<Friend>,
    pub muted_stream_alert_friend_ids: Vec<String>,
}

impl Preferences {
    pub fn friend_account(&mut self, id: &str) -> AccountFriends {
        let account = self.friend_accounts.entry(id.to_string()).or_default();
        // A local add was never consent from the other account. Preserve it
        // as a suggestion to send a request, not as a mutual cloud friendship.
        for friend in self.friends.drain(..) {
            remember_friend(&mut account.suggestions, friend);
        }
        account.clone()
    }
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            quality: 1,
            fps: None,
            own_codes: Vec::new(),
            friends: Vec::new(),
            friend_accounts: Default::default(),
            friends_panel_collapsed: false,
            locale: None,
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

pub fn load() -> Result<Option<Session>> {
    load_from(&path().context("APPDATA is not set")?)
}

fn load_from(path: &Path) -> Result<Option<Session>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read session {}", path.display()));
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("parse session {}", path.display()))
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
    fn older_preferences_without_locale_still_load() {
        // Installed clients wrote this file before UI language existed. Missing
        // must mean "follow Windows", not a parse error that wipes the roster.
        let preferences: Preferences = serde_json::from_str(r#"{"quality":1}"#).unwrap();
        assert_eq!(preferences.locale, None);
        assert_eq!(preferences.quality, 1);
    }

    #[test]
    fn legacy_friends_become_suggestions_for_one_account_without_granting_friendship() {
        // Device-wide lists used to follow whoever logged in next. Migrate
        // once as suggestions and keep both accounts' caches separate.
        let mut preferences: Preferences =
            serde_json::from_str(r#"{"friends":[{"id":"42","name":"Legacy"}]}"#).unwrap();
        let a = preferences.friend_account("1");
        assert!(a.friends.is_empty());
        assert_eq!(a.suggestions[0].id, "42");
        assert!(preferences.friends.is_empty());
        let b = preferences.friend_account("2");
        assert!(b.friends.is_empty());
        assert!(b.suggestions.is_empty());
        let persisted = serde_json::to_string(&preferences).unwrap();
        let mut loaded: Preferences = serde_json::from_str(&persisted).unwrap();
        assert_eq!(loaded.friend_account("1").suggestions[0].id, "42");
        assert!(loaded.friend_account("2").suggestions.is_empty());
    }

    #[test]
    fn friend_codes_share_a_profile_without_sharing_session_credentials() {
        // A personal code must work offline and must never serialize Session:
        // that would turn a shareable invitation into a leaked bearer token.
        let session = Session {
            id: "123456789012345678".into(),
            name: "Jo / ジョー".into(),
            avatar_url: Some("https://cdn.discordapp.com/avatar.png".into()),
            token: "private-session-token".into(),
        };
        let code = session.friend_code().unwrap();
        assert_eq!(code, "orange-friend:1:123456789012345678:Jo / ジョー");
        let friend = Friend::from_code(&format!(" \n{code}\n")).unwrap();
        assert_eq!(friend.id, "123456789012345678");
        assert_eq!(friend.name, "Jo / ジョー");
        assert_eq!(friend.avatar_url, None);
    }

    #[test]
    fn invalid_friend_codes_cannot_be_saved_as_unusable_roster_entries() {
        // Room codes, malformed ids, and control characters previously had no
        // direct-add boundary. Reject them before they reach presence queries.
        for code in [
            "",
            "ABC-234",
            "orange-friend:2:42:Jo",
            "orange-friend:1::Jo",
            "orange-friend:1:0:Jo",
            "orange-friend:1:0042:Jo",
            "orange-friend:1:42,77:Jo",
            "orange-friend:1:18446744073709551616:Jo",
            "orange-friend:1:42:",
            "orange-friend:1:42:Jo\nSomeone",
        ] {
            assert!(Friend::from_code(code).is_err(), "accepted {code:?}");
        }
        assert!(Friend::from_code(&format!("orange-friend:1:42:{}", "x".repeat(513))).is_err());
    }

    #[test]
    fn repeated_profiles_refresh_one_offer_without_displacing_other_people() {
        // Repeated joins must not fill the backlog with the same person, and
        // a burst of new people must stay bounded while the UI is hidden.
        let mut offers = Vec::new();
        for id in 1..=101 {
            remember_friend(
                &mut offers,
                Friend {
                    id: id.to_string(),
                    name: "Old".into(),
                    avatar_url: None,
                },
            );
        }
        remember_friend(
            &mut offers,
            Friend {
                id: "101".into(),
                name: "New".into(),
                avatar_url: None,
            },
        );
        assert_eq!(offers.len(), 100);
        assert_eq!(offers.first().unwrap().id, "2");
        assert_eq!(offers.last().unwrap().name, "New");
    }

    #[test]
    fn missing_session_is_absent() {
        let dir = tempfile::tempdir().unwrap();

        let loaded = load_from(&dir.path().join("session.json")).unwrap();

        assert!(loaded.is_none());
    }

    #[test]
    fn malformed_session_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(&path, "not json").unwrap();

        let error = load_from(&path).unwrap_err();

        assert!(error.to_string().contains("parse session"));
    }

    #[test]
    fn session_with_cli_token_fields_loads_client_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(
            &path,
            r#"{
                "token": "secret",
                "id": "123",
                "name": "Orange User",
                "avatar_url": "https://example.com/avatar.png"
            }"#,
        )
        .unwrap();

        let loaded = load_from(&path).unwrap().unwrap();

        assert_eq!(loaded.id, "123");
        assert_eq!(loaded.name, "Orange User");
        assert_eq!(
            loaded.avatar_url.as_deref(),
            Some("https://example.com/avatar.png")
        );
    }

    #[test]
    fn missing_preferences_use_defaults() {
        let dir = tempfile::tempdir().unwrap();

        let preferences = load_preferences_from(&dir.path().join("preferences.json")).unwrap();

        assert_eq!(preferences.quality, 1);
        assert_eq!(preferences.fps, None);
        assert!(preferences.own_codes.is_empty());
        assert!(!preferences.friends_panel_collapsed);
    }

    #[test]
    fn friend_mutes_and_panel_state_persist_per_account() {
        let mut preferences = Preferences {
            friends_panel_collapsed: true,
            ..Preferences::default()
        };
        preferences.friend_accounts.insert(
            "1".into(),
            AccountFriends {
                muted_stream_alert_friend_ids: vec!["42".into()],
                ..AccountFriends::default()
            },
        );
        preferences.friend_accounts.insert(
            "2".into(),
            AccountFriends {
                muted_stream_alert_friend_ids: vec!["99".into()],
                ..AccountFriends::default()
            },
        );

        let json = serde_json::to_string(&preferences).unwrap();
        let mut loaded: Preferences = serde_json::from_str(&json).unwrap();

        assert!(loaded.friends_panel_collapsed);
        assert_eq!(
            loaded.friend_account("1").muted_stream_alert_friend_ids,
            vec!["42".to_string()]
        );
        assert_eq!(
            loaded.friend_account("2").muted_stream_alert_friend_ids,
            vec!["99".to_string()]
        );
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
    fn preferences_with_the_retired_bitrate_field_still_load_and_clean_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(
            &path,
            r#"{"quality":2,"fps":120,"bitrate":2,"own_codes":["ORANGE"]}"#,
        )
        .unwrap();

        let loaded = load_preferences_from(&path).unwrap();

        assert_eq!(loaded.quality, 2);
        assert_eq!(loaded.fps, Some(120));
        assert_eq!(loaded.own_codes, vec!["ORANGE".to_string()]);
        save_preferences_to(&path, &loaded).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(saved.get("bitrate").is_none());
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
            ..Preferences::default()
        };

        save_preferences_to(&path, &preferences).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["quality"], 2);
        assert_eq!(json["fps"], 120);
        assert_eq!(json["own_codes"], serde_json::json!(["ORANGE"]));
        assert!(json.get("bitrate").is_none());
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
