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
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Mutex;

/// How long an unclaimed login attempt stays valid.
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const PENDING_CAPACITY: usize = 1024;
const SESSION_CAPACITY: usize = 4096;
const PENDING_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
const DISCORD_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DISCORD_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CANCELLED_MESSAGE: &str = "Login was cancelled";

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
    /// Wall clock, not `Instant`: a session outlives the process now, and a
    /// monotonic clock restarts with it -- every 30-day session would have had
    /// its timer reset by each deploy.
    created_at: SystemTime,
}

#[derive(Default)]
struct AuthState {
    /// state nonce -> progress
    pending: HashMap<String, Pending>,
    /// session token -> who it belongs to
    sessions: HashMap<String, StoredSession>,
    last_pending_sweep: Option<Instant>,
}

#[derive(Clone)]
pub struct Auth {
    config: Option<DiscordConfig>,
    state: Arc<Mutex<AuthState>>,
    http: reqwest::Client,
    /// Durable backing for `sessions`, when configured.
    ///
    /// Memory stays the read path; this is only consulted on a miss, so the
    /// steady-state cost is nothing and a relay restart costs one lookup per
    /// returning user rather than a forced re-login for all of them.
    store: Option<crate::store::TableStore>,
}

impl Auth {
    pub fn new(config: Option<DiscordConfig>) -> Self {
        Self::with_store(config, crate::store::TableStore::from_env())
    }

    pub(crate) fn with_store(
        config: Option<DiscordConfig>,
        store: Option<crate::store::TableStore>,
    ) -> Self {
        Self {
            config,
            state: Arc::new(Mutex::new(AuthState::default())),
            http: reqwest::Client::builder()
                .connect_timeout(DISCORD_CONNECT_TIMEOUT)
                .timeout(DISCORD_REQUEST_TIMEOUT)
                .build()
                .expect("constant Discord HTTP client configuration is valid"),
            store,
        }
    }

    /// Whether sessions survive a restart. Reported at startup so a deploy
    /// that silently lost its storage configuration is visible immediately,
    /// rather than the next time everyone is signed out.
    pub fn durable(&self) -> bool {
        self.store.is_some()
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

        let mut auth = self.state.lock().await;
        let now = Instant::now();
        if !auth.has_pending_capacity(now) {
            anyhow::bail!("too many login attempts; try again later");
        }
        let state = random_token();
        let url = config.authorize_url(&state);
        auth.pending
            .insert(state.clone(), Pending::Waiting { since: now });
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
            auth.expire_pending(state, Instant::now());
            auth.claim(state)?;
        }

        let result = self.exchange(config, code).await;

        let (identity, minted) = {
            let mut auth = self.state.lock().await;
            auth.expire_pending(state, Instant::now());
            let identity = auth.finish_completion(state, result)?;
            let minted = match auth.pending.get(state) {
                Some(Pending::Ready { session, .. }) => Some(session.clone()),
                _ => None,
            };
            (identity, minted)
        };

        // Persisted after the lock is released, and only best-effort: a login
        // that works but is not written down still works until the next
        // restart, which is strictly better than refusing the login outright.
        if let (Some(store), Some(session)) = (self.store.as_ref(), minted) {
            if let Err(error) = store
                .put_session(&session, &identity, SystemTime::now())
                .await
            {
                eprintln!("[auth] session was not persisted: {error:#}");
            }
        }
        Ok(identity)
    }

    /// Terminalize a browser cancellation so the desktop poller does not wait
    /// until the pending-attempt timeout.
    pub async fn fail(&self, state: &str) {
        let mut auth = self.state.lock().await;
        let now = Instant::now();
        auth.expire_pending(state, now);
        if matches!(auth.pending.get(state), Some(Pending::Waiting { .. })) {
            auth.pending.insert(
                state.to_string(),
                Pending::Failed {
                    message: CANCELLED_MESSAGE.into(),
                    since: now,
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
        auth.expire_pending(state, Instant::now());
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
    ///
    /// Memory first, then the durable store. The store is only reached on a
    /// miss, so a warm relay never touches it and a cold one pays a single
    /// lookup per returning user.
    pub async fn identify(&self, session: &str) -> Option<Identity> {
        match self.identify_checked(session).await {
            Ok(identity) => identity,
            Err(error) => {
                eprintln!("[auth] session lookup failed: {error:#}");
                None
            }
        }
    }

    /// HTTP clients clear rejected credentials. Preserve storage failures so
    /// a cold relay cannot turn an Azure outage into a mass signout.
    pub(crate) async fn identify_checked(&self, session: &str) -> Result<Option<Identity>> {
        {
            let mut auth = self.state.lock().await;
            auth.expire_session(session, SystemTime::now());
            if let Some(stored) = auth.sessions.get(session) {
                return Ok(Some(stored.identity.clone()));
            }
        }

        let Some(store) = self.store.as_ref() else {
            return Ok(None);
        };
        // The lock is released across this await on purpose: a table round
        // trip while holding it would serialise every other authentication.
        let Some((identity, created_at)) = store.get_session(session).await? else {
            return Ok(None);
        };
        if SystemTime::now()
            .duration_since(created_at)
            .unwrap_or_default()
            >= SESSION_TTL
        {
            let _ = store.delete_session(session).await;
            return Ok(None);
        }

        self.state.lock().await.cache_session(
            session.to_string(),
            StoredSession {
                identity: identity.clone(),
                created_at,
            },
        );
        Ok(Some(identity))
    }

    /// Mint a session without a round trip to Discord, so tests of things that
    /// merely require an authenticated caller do not have to fake the OAuth
    /// exchange as well.
    #[cfg(test)]
    pub(crate) async fn insert_session_for_test(&self, session: &str, identity: Identity) {
        self.state.lock().await.sessions.insert(
            session.to_string(),
            StoredSession {
                identity,
                created_at: SystemTime::now(),
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
    fn cache_session(&mut self, token: String, session: StoredSession) {
        // Restoration can race another lookup of the same token. Replacing it
        // must not evict an unrelated session or reset its durable lifetime.
        self.sessions.remove(&token);
        while self.sessions.len() >= SESSION_CAPACITY {
            let oldest = self
                .sessions
                .iter()
                .min_by_key(|(_, stored)| stored.created_at)
                .map(|(token, _)| token.clone())
                .expect("a full session cache is nonempty");
            // This is only the read-through cache; the durable row survives.
            self.sessions.remove(&oldest);
        }
        self.sessions.insert(token, session);
    }

    fn expire_pending(&mut self, state: &str, now: Instant) {
        let expired = self
            .pending
            .get(state)
            .is_some_and(|pending| now.saturating_duration_since(pending.since()) >= PENDING_TTL);
        if expired {
            self.pending.remove(state);
        }
    }

    fn expire_session(&mut self, session: &str, now: SystemTime) {
        let expired = self.sessions.get(session).is_some_and(|stored| {
            now.duration_since(stored.created_at).unwrap_or_default() >= SESSION_TTL
        });
        if expired {
            self.sessions.remove(session);
        }
    }

    fn has_pending_capacity(&mut self, now: Instant) -> bool {
        if self.pending.len() < PENDING_CAPACITY {
            return true;
        }
        let may_sweep = self
            .last_pending_sweep
            .is_none_or(|last| now.saturating_duration_since(last) >= PENDING_SWEEP_INTERVAL);
        if may_sweep {
            self.pending
                .retain(|_, pending| now.saturating_duration_since(pending.since()) < PENDING_TTL);
            self.last_pending_sweep = Some(now);
        }
        self.pending.len() < PENDING_CAPACITY
    }

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
                self.cache_session(
                    session.clone(),
                    StoredSession {
                        identity: identity.clone(),
                        created_at: SystemTime::now(),
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

    fn configured_auth() -> Auth {
        Auth::new(Some(DiscordConfig {
            client_id: "client".into(),
            client_secret: "secret".into(),
            redirect_uri: "https://relay.invalid/auth/callback".into(),
        }))
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

        auth.expire_pending("state", Instant::now());

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
        auth.expire_pending("state", Instant::now());

        let error = auth
            .finish_completion("state", Ok(identity("user")))
            .unwrap_err();

        assert_eq!(error.to_string(), "login attempt expired");
        assert!(!auth.pending.contains_key("state"));
        assert!(auth.sessions.is_empty());
    }

    #[tokio::test]
    async fn start_prunes_then_rejects_more_than_1024_pending_attempts() {
        let auth = configured_auth();
        let now = Instant::now();
        {
            let mut state = auth.state.lock().await;
            state.pending.insert(
                "expired".into(),
                Pending::Waiting {
                    since: now - PENDING_TTL,
                },
            );
            for attempt in 0..PENDING_CAPACITY - 1 {
                state
                    .pending
                    .insert(format!("state-{attempt}"), Pending::Waiting { since: now });
            }
        }

        auth.start()
            .await
            .expect("attempt at capacity was rejected");
        let error = auth.start().await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "too many login attempts; try again later"
        );
        let state = auth.state.lock().await;
        assert_eq!(state.pending.len(), PENDING_CAPACITY);
        assert!(!state.pending.contains_key("expired"));
    }

    #[test]
    fn pending_capacity_sweeps_are_full_only_when_throttled_admission_needs_them() {
        let now = Instant::now();
        let mut below_capacity = AuthState::default();
        below_capacity.pending.insert(
            "expired".into(),
            Pending::Waiting {
                since: now - PENDING_TTL,
            },
        );
        assert!(below_capacity.has_pending_capacity(now));
        assert!(below_capacity.pending.contains_key("expired"));
        assert_eq!(below_capacity.last_pending_sweep, None);

        let mut full = AuthState::default();
        for attempt in 0..PENDING_CAPACITY {
            full.pending
                .insert(format!("state-{attempt}"), Pending::Waiting { since: now });
        }
        assert!(!full.has_pending_capacity(now));
        assert_eq!(full.last_pending_sweep, Some(now));

        full.pending.insert(
            "state-0".into(),
            Pending::Waiting {
                since: now - PENDING_TTL,
            },
        );
        assert!(!full.has_pending_capacity(now + Duration::from_millis(999)));
        assert!(full.pending.contains_key("state-0"));
        assert!(full.has_pending_capacity(now + Duration::from_secs(1)));
        assert!(!full.pending.contains_key("state-0"));
    }

    #[test]
    fn successful_completion_at_4096_sessions_evicts_only_the_oldest() {
        let now = SystemTime::now();
        let mut auth = AuthState::default();
        auth.pending.insert(
            "state".into(),
            Pending::Completing {
                since: Instant::now(),
            },
        );
        auth.sessions.insert(
            "oldest".into(),
            StoredSession {
                identity: identity("oldest"),
                created_at: now - Duration::from_secs(2),
            },
        );
        for session in 1..SESSION_CAPACITY {
            auth.sessions.insert(
                format!("session-{session}"),
                StoredSession {
                    identity: identity(&format!("user-{session}")),
                    created_at: now - Duration::from_secs(1),
                },
            );
        }

        auth.finish_completion("state", Ok(identity("new")))
            .unwrap();

        assert_eq!(auth.sessions.len(), SESSION_CAPACITY);
        assert!(!auth.sessions.contains_key("oldest"));
        for session in 1..SESSION_CAPACITY {
            assert!(auth.sessions.contains_key(&format!("session-{session}")));
        }
        assert_eq!(
            auth.sessions
                .values()
                .filter(|stored| stored.identity.id == "new")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cancellation_stores_only_the_canonical_bounded_message() {
        let auth = Auth::new(None);
        *auth.state.lock().await = waiting_state();

        auth.fail("state").await;

        assert!(matches!(
            auth.poll("state").await,
            PollResult::Failed(message) if message == "Login was cancelled" && message.len() == 19
        ));
    }

    #[test]
    fn login_restores_the_cache_bound_after_durable_reads_overfilled_it() {
        // Durable restoration bypassed eviction, then the equality-only login
        // check stopped evicting too. Even an overfull cache must recover.
        let now = SystemTime::now();
        let mut auth = waiting_state();
        auth.claim("state").unwrap();
        for index in 0..SESSION_CAPACITY + 2 {
            auth.sessions.insert(
                format!("session-{index}"),
                StoredSession {
                    identity: identity("returning-user"),
                    created_at: now - Duration::from_secs((SESSION_CAPACITY + 2 - index) as u64),
                },
            );
        }

        auth.finish_completion("state", Ok(identity("new-user")))
            .unwrap();

        assert_eq!(auth.sessions.len(), SESSION_CAPACITY);
        for index in 0..3 {
            assert!(!auth.sessions.contains_key(&format!("session-{index}")));
        }
        assert!(auth.sessions.contains_key("session-3"));
        assert!(auth
            .sessions
            .values()
            .any(|stored| stored.identity.id == "new-user"));
    }

    #[test]
    fn restored_sessions_keep_their_original_expiration_and_bound_the_cache() {
        // A cold relay must not cache every durable row forever. Restoration
        // uses the same insertion path as login, without renewing the token.
        let created_at = SystemTime::now() - SESSION_TTL + Duration::from_secs(1);
        let mut auth = AuthState::default();
        for index in 0..SESSION_CAPACITY + 2 {
            auth.cache_session(
                format!("restored-{index}"),
                StoredSession {
                    identity: identity("user"),
                    created_at,
                },
            );
            assert!(auth.sessions.len() <= SESSION_CAPACITY);
        }
        assert_eq!(auth.sessions.len(), SESSION_CAPACITY);
        let last = format!("restored-{}", SESSION_CAPACITY + 1);
        assert_eq!(auth.sessions[&last].created_at, created_at);
        auth.expire_session(&last, created_at + SESSION_TTL);
        assert!(!auth.sessions.contains_key(&last));
    }

    #[tokio::test]
    async fn concurrent_restoration_of_one_token_does_not_evict_extra_sessions() {
        // Both cold lookups can finish before either inserts. Their serialized
        // cache writes must replace one token, not evict for each response.
        let auth = Arc::new(Mutex::new(AuthState::default()));
        let created_at = SystemTime::now();
        {
            let mut state = auth.lock().await;
            for index in 0..SESSION_CAPACITY {
                state.cache_session(
                    format!("existing-{index}"),
                    StoredSession {
                        identity: identity("existing"),
                        created_at: created_at
                            - Duration::from_secs((SESSION_CAPACITY - index) as u64),
                    },
                );
            }
        }
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let auth = auth.clone();
            tasks.push(tokio::spawn(async move {
                auth.lock().await.cache_session(
                    "restored".into(),
                    StoredSession {
                        identity: identity("returning"),
                        created_at,
                    },
                );
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let state = auth.lock().await;
        assert_eq!(state.sessions.len(), SESSION_CAPACITY);
        assert!(!state.sessions.contains_key("existing-0"));
        for index in 1..SESSION_CAPACITY {
            assert!(state.sessions.contains_key(&format!("existing-{index}")));
        }
        assert_eq!(state.sessions["restored"].identity.id, "returning");
    }

    #[tokio::test]
    async fn poll_expires_only_the_requested_pending_attempt() {
        let auth = Auth::new(None);
        let expired = Instant::now() - PENDING_TTL;
        {
            let mut state = auth.state.lock().await;
            state
                .pending
                .insert("requested".into(), Pending::Waiting { since: expired });
            state
                .pending
                .insert("unrelated".into(), Pending::Waiting { since: expired });
        }

        assert!(matches!(auth.poll("requested").await, PollResult::Unknown));

        let state = auth.state.lock().await;
        assert!(!state.pending.contains_key("requested"));
        assert!(state.pending.contains_key("unrelated"));
    }

    #[tokio::test]
    async fn identify_expires_only_the_requested_session() {
        let auth = Auth::new(None);
        let expired = SystemTime::now() - SESSION_TTL;
        {
            let mut state = auth.state.lock().await;
            state.sessions.insert(
                "requested".into(),
                StoredSession {
                    identity: identity("requested-id"),
                    created_at: expired,
                },
            );
            state.sessions.insert(
                "unrelated".into(),
                StoredSession {
                    identity: identity("unrelated-id"),
                    created_at: expired,
                },
            );
        }

        assert!(auth.identify("requested").await.is_none());

        let state = auth.state.lock().await;
        assert!(!state.sessions.contains_key("requested"));
        assert!(state.sessions.contains_key("unrelated"));
    }

    #[tokio::test]
    async fn complete_expires_only_the_requested_pending_attempt() {
        let auth = configured_auth();
        let expired = Instant::now() - PENDING_TTL;
        {
            let mut state = auth.state.lock().await;
            state
                .pending
                .insert("requested".into(), Pending::Waiting { since: expired });
            state
                .pending
                .insert("unrelated".into(), Pending::Waiting { since: expired });
        }

        let error = auth.complete("requested", "unused").await.unwrap_err();

        assert_eq!(error.to_string(), "unknown or expired login attempt");
        let state = auth.state.lock().await;
        assert!(!state.pending.contains_key("requested"));
        assert!(state.pending.contains_key("unrelated"));
    }
}
