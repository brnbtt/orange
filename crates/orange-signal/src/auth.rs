//! Discord identity.
//!
//! The desktop app never sees Discord's `client_secret`. Anything shipped to a
//! user's machine can be extracted from it, so the code-for-token exchange
//! happens here on the relay, and the app receives only our own opaque session
//! token.
//!
//! The flow avoids a loopback redirect on purpose. Discord requires redirect
//! URIs to match exactly, which would force the desktop app onto a hardcoded
//! port that may already be in use. Instead the browser lands back on the
//! relay, and the app polls for the result using a nonce it generated.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// How long an unclaimed login attempt stays valid.
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    pub id: String,
    /// Display name, falling back to username when no global name is set.
    pub name: String,
    pub avatar_url: Option<String>,
}

#[derive(Clone)]
pub struct DiscordConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Must match a redirect URI registered on the Discord application.
    pub redirect_uri: String,
}

impl DiscordConfig {
    /// Read from the environment. Absent configuration is not an error: the
    /// relay still works with anonymous peers, it just cannot log anyone in.
    pub fn from_env() -> Option<Self> {
        let client_id = std::env::var("DISCORD_CLIENT_ID").ok()?;
        let client_secret = std::env::var("DISCORD_CLIENT_SECRET").ok()?;
        let redirect_uri = std::env::var("DISCORD_REDIRECT_URI").ok()?;
        if client_id.is_empty() || client_secret.is_empty() {
            return None;
        }
        Some(Self {
            client_id,
            client_secret,
            redirect_uri,
        })
    }

    pub fn authorize_url(&self, state: &str) -> String {
        // No `prompt` parameter: Discord's default shows the consent screen on
        // first authorisation and skips it thereafter. `prompt=none` would
        // fail outright for a user who has never authorised the app.
        format!(
            "https://discord.com/oauth2/authorize\
             ?response_type=code&client_id={}&scope=identify&state={}&redirect_uri={}",
            urlencode(&self.client_id),
            urlencode(state),
            urlencode(&self.redirect_uri),
        )
    }
}

/// Minimal percent-encoding for the few values we put in URLs.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

enum Pending {
    /// Waiting for the user to finish in the browser.
    Waiting {
        since: Instant,
    },
    /// A callback claimed this attempt and is exchanging its code.
    Completing {
        since: Instant,
    },
    /// Login finished; the app has not collected it yet.
    Ready {
        session: String,
        since: Instant,
    },
    Failed {
        message: String,
        since: Instant,
    },
}

impl Pending {
    fn since(&self) -> Instant {
        match self {
            Pending::Waiting { since }
            | Pending::Completing { since }
            | Pending::Ready { since, .. }
            | Pending::Failed { since, .. } => *since,
        }
    }
}

struct StoredSession {
    identity: Identity,
    created_at: Instant,
}

#[derive(Default)]
struct AuthState {
    /// state nonce -> progress
    pending: HashMap<String, Pending>,
    /// session token -> who it belongs to
    sessions: HashMap<String, StoredSession>,
}

#[derive(Clone)]
pub struct Auth {
    config: Option<DiscordConfig>,
    state: Arc<Mutex<AuthState>>,
    http: reqwest::Client,
}

impl Auth {
    pub fn new(config: Option<DiscordConfig>) -> Self {
        Self {
            config,
            state: Arc::new(Mutex::new(AuthState::default())),
            http: reqwest::Client::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.is_some()
    }

    /// Begin a login. Returns the URL to open and the nonce to poll with.
    pub async fn start(&self) -> Result<(String, String)> {
        let config = self
            .config
            .as_ref()
            .context("Discord login is not configured on this relay")?;

        let state = random_token();
        let url = config.authorize_url(&state);

        let mut auth = self.state.lock().await;
        auth.prune();
        auth.pending.insert(
            state.clone(),
            Pending::Waiting {
                since: Instant::now(),
            },
        );
        Ok((url, state))
    }

    /// Handle Discord's redirect: exchange the code and record the session.
    pub async fn complete(&self, state: &str, code: &str) -> Result<Identity> {
        let config = self
            .config
            .as_ref()
            .context("Discord login is not configured")?;

        {
            let mut auth = self.state.lock().await;
            auth.prune();
            auth.claim(state)?;
        }

        let result = self.exchange(config, code).await;

        let mut auth = self.state.lock().await;
        auth.prune();
        auth.finish_completion(state, result)
    }

    /// Terminalize a browser cancellation so the desktop poller does not wait
    /// until the pending-attempt timeout.
    pub async fn fail(&self, state: &str, message: String) {
        let mut auth = self.state.lock().await;
        if matches!(auth.pending.get(state), Some(Pending::Waiting { .. })) {
            auth.pending.insert(
                state.to_string(),
                Pending::Failed {
                    message,
                    since: Instant::now(),
                },
            );
        }
    }

    async fn exchange(&self, config: &DiscordConfig, code: &str) -> Result<Identity> {
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
        }

        let token: TokenResponse = self
            .http
            .post("https://discord.com/api/v10/oauth2/token")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", config.redirect_uri.as_str()),
            ])
            .basic_auth(&config.client_id, Some(&config.client_secret))
            .send()
            .await
            .context("token request failed")?
            .error_for_status()
            .context("Discord rejected the token exchange")?
            .json()
            .await
            .context("malformed token response")?;

        #[derive(Deserialize)]
        struct DiscordUser {
            id: String,
            username: String,
            global_name: Option<String>,
            avatar: Option<String>,
        }

        let user: DiscordUser = self
            .http
            .get("https://discord.com/api/v10/users/@me")
            .bearer_auth(&token.access_token)
            .send()
            .await
            .context("user request failed")?
            .error_for_status()
            .context("Discord rejected the user request")?
            .json()
            .await
            .context("malformed user response")?;

        let avatar_url = user.avatar.as_ref().map(|hash| {
            format!(
                "https://cdn.discordapp.com/avatars/{}/{}.png?size=64",
                user.id, hash
            )
        });

        Ok(Identity {
            name: user.global_name.unwrap_or(user.username),
            id: user.id,
            avatar_url,
        })
    }

    /// Poll for the outcome of a login attempt.
    pub async fn poll(&self, state: &str) -> PollResult {
        let mut auth = self.state.lock().await;
        auth.prune();
        match auth.pending.get(state) {
            None => PollResult::Unknown,
            Some(Pending::Waiting { .. } | Pending::Completing { .. }) => PollResult::Waiting,
            Some(Pending::Failed { message, .. }) => {
                let message = message.clone();
                auth.pending.remove(state);
                PollResult::Failed(message)
            }
            Some(Pending::Ready { session, .. }) => {
                let session = session.clone();
                let identity = auth
                    .sessions
                    .get(&session)
                    .map(|stored| stored.identity.clone());
                // One-shot: collecting the result consumes it.
                auth.pending.remove(state);
                match identity {
                    Some(identity) => PollResult::Ready { session, identity },
                    None => PollResult::Unknown,
                }
            }
        }
    }

    /// Who a session belongs to, if it is still valid.
    pub async fn identify(&self, session: &str) -> Option<Identity> {
        let mut auth = self.state.lock().await;
        auth.prune();
        auth.sessions
            .get(session)
            .map(|stored| stored.identity.clone())
    }

    #[cfg(test)]
    pub(crate) async fn insert_test_session(&self, session: &str, identity: Identity) {
        self.state.lock().await.sessions.insert(
            session.to_string(),
            StoredSession {
                identity,
                created_at: Instant::now(),
            },
        );
    }
}

pub enum PollResult {
    Waiting,
    Ready { session: String, identity: Identity },
    Failed(String),
    Unknown,
}

impl AuthState {
    fn claim(&mut self, state: &str) -> Result<()> {
        let Some(pending) = self.pending.get_mut(state) else {
            anyhow::bail!("unknown or expired login attempt");
        };
        if !matches!(pending, Pending::Waiting { .. }) {
            anyhow::bail!("unknown or expired login attempt");
        }
        *pending = Pending::Completing {
            since: Instant::now(),
        };
        Ok(())
    }

    fn finish_completion(&mut self, state: &str, result: Result<Identity>) -> Result<Identity> {
        if !matches!(self.pending.get(state), Some(Pending::Completing { .. })) {
            anyhow::bail!("login attempt expired");
        }

        match result {
            Ok(identity) => {
                let session = random_token();
                let now = Instant::now();
                self.sessions.insert(
                    session.clone(),
                    StoredSession {
                        identity: identity.clone(),
                        created_at: now,
                    },
                );
                self.pending.insert(
                    state.to_string(),
                    Pending::Ready {
                        session,
                        since: now,
                    },
                );
                Ok(identity)
            }
            Err(err) => {
                self.pending.insert(
                    state.to_string(),
                    Pending::Failed {
                        message: err.to_string(),
                        since: Instant::now(),
                    },
                );
                Err(err)
            }
        }
    }

    fn prune(&mut self) {
        self.pending
            .retain(|_, p| p.since().elapsed() < PENDING_TTL);
        self.sessions
            .retain(|_, session| session.created_at.elapsed() < SESSION_TTL);
    }
}

/// Opaque, unguessable token. Not a JWT: the relay is the only thing that
/// validates these, so there is nothing to gain from a signed format.
fn random_token() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..48)
        .map(|_| {
            const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
            CHARS[rng.gen_range(0..CHARS.len())] as char
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(id: &str) -> Identity {
        Identity {
            id: id.into(),
            name: id.into(),
            avatar_url: None,
        }
    }

    fn waiting_state() -> AuthState {
        let mut auth = AuthState::default();
        auth.pending.insert(
            "state".into(),
            Pending::Waiting {
                since: Instant::now(),
            },
        );
        auth
    }

    #[test]
    fn callback_state_can_only_be_claimed_once() {
        let mut auth = waiting_state();

        assert!(auth.claim("state").is_ok());
        assert!(auth.claim("state").is_err());
    }

    #[test]
    fn expired_waiting_state_is_pruned_before_claim() {
        let mut auth = AuthState::default();
        auth.pending.insert(
            "state".into(),
            Pending::Waiting {
                since: Instant::now() - PENDING_TTL,
            },
        );

        auth.prune();

        assert!(auth.claim("state").is_err());
        assert!(!auth.pending.contains_key("state"));
    }

    #[test]
    fn pruned_completion_cannot_create_session_or_ready_state() {
        let mut auth = AuthState::default();
        auth.pending.insert(
            "state".into(),
            Pending::Completing {
                since: Instant::now() - PENDING_TTL,
            },
        );
        auth.prune();

        let error = auth
            .finish_completion("state", Ok(identity("user")))
            .unwrap_err();

        assert_eq!(error.to_string(), "login attempt expired");
        assert!(!auth.pending.contains_key("state"));
        assert!(auth.sessions.is_empty());
    }

    #[tokio::test]
    async fn cancellation_terminalizes_a_waiting_attempt() {
        let auth = Auth::new(None);
        *auth.state.lock().await = waiting_state();

        auth.fail("state", "access_denied".into()).await;

        assert!(matches!(
            auth.poll("state").await,
            PollResult::Failed(message) if message == "access_denied"
        ));
    }

    #[tokio::test]
    async fn identify_prunes_expired_sessions_and_retains_current_sessions() {
        let auth = Auth::new(None);
        {
            let mut state = auth.state.lock().await;
            state.sessions.insert(
                "current".into(),
                StoredSession {
                    identity: identity("current-id"),
                    created_at: Instant::now(),
                },
            );
            state.sessions.insert(
                "expired".into(),
                StoredSession {
                    identity: identity("expired-id"),
                    created_at: Instant::now() - SESSION_TTL,
                },
            );
        }

        assert!(auth.identify("expired").await.is_none());
        assert_eq!(
            auth.identify("current").await.map(|identity| identity.id),
            Some("current-id".into())
        );
        let state = auth.state.lock().await;
        assert!(!state.sessions.contains_key("expired"));
        assert!(state.sessions.contains_key("current"));
    }
}
