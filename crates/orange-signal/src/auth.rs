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
        format!(
            "https://discord.com/oauth2/authorize\
             ?response_type=code&client_id={}&scope=identify&state={}&redirect_uri={}&prompt=none",
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
    Waiting { since: Instant },
    /// Login finished; the app has not collected it yet.
    Ready { session: String, since: Instant },
    Failed { message: String, since: Instant },
}

impl Pending {
    fn since(&self) -> Instant {
        match self {
            Pending::Waiting { since }
            | Pending::Ready { since, .. }
            | Pending::Failed { since, .. } => *since,
        }
    }
}

#[derive(Default)]
struct AuthState {
    /// state nonce -> progress
    pending: HashMap<String, Pending>,
    /// session token -> who it belongs to
    sessions: HashMap<String, Identity>,
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
            let auth = self.state.lock().await;
            if !matches!(auth.pending.get(state), Some(Pending::Waiting { .. })) {
                anyhow::bail!("unknown or expired login attempt");
            }
        }

        let result = self.exchange(config, code).await;

        let mut auth = self.state.lock().await;
        match result {
            Ok(identity) => {
                let session = random_token();
                auth.sessions.insert(session.clone(), identity.clone());
                auth.pending.insert(
                    state.to_string(),
                    Pending::Ready {
                        session,
                        since: Instant::now(),
                    },
                );
                Ok(identity)
            }
            Err(err) => {
                auth.pending.insert(
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
            Some(Pending::Waiting { .. }) => PollResult::Waiting,
            Some(Pending::Failed { message, .. }) => {
                let message = message.clone();
                auth.pending.remove(state);
                PollResult::Failed(message)
            }
            Some(Pending::Ready { session, .. }) => {
                let session = session.clone();
                let identity = auth.sessions.get(&session).cloned();
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
        self.state.lock().await.sessions.get(session).cloned()
    }
}

pub enum PollResult {
    Waiting,
    Ready { session: String, identity: Identity },
    Failed(String),
    Unknown,
}

impl AuthState {
    fn prune(&mut self) {
        self.pending
            .retain(|_, p| p.since().elapsed() < PENDING_TTL);
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
