//! Asking the relay which friends are streaming right now.
//!
//! A bounded HTTP long-poll returns when the visible snapshot changes without
//! consuming one of the relay's streaming WebSocket permits. Older relays omit
//! the revision and keep the original timed polling behavior.

use crate::{background::join_background_worker, session::Friend};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Retry and legacy fallback delay. Revision-bearing replies renew immediately.
pub(crate) const INTERVAL: Duration = Duration::from_secs(15);

const JOIN_TIMEOUT: Duration = Duration::from_secs(35);
const RESPONSE_MAX_BYTES: u64 = 256 * 1024;

/// Mirrors the relay's presence encoding. Kept as its own type rather than
/// shared with `orange-signal` because the client has no dependency on that
/// crate and gaining one would drag tokio into a process with no async runtime.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub(crate) enum Presence {
    Offline,
    Live { code: String },
    Full,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Entry {
    pub(crate) id: String,
    /// Present only while the friend is live: the relay holds a profile just
    /// for the length of a host connection. The client keeps the last one it
    /// saw in `preferences.json`, so an offline friend still has a face.
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) avatar_url: Option<String>,
    #[serde(flatten)]
    pub(crate) presence: Presence,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Snapshot {
    pub(crate) friends: Vec<Entry>,
    #[serde(default)]
    pub(crate) revision: Option<String>,
}

pub(crate) struct PresenceJob<T = Snapshot> {
    cancel: Arc<AtomicBool>,
    cancelled_at: Option<Instant>,
    receiver: mpsc::Receiver<Result<T, PresenceError>>,
    worker: Option<JoinHandle<()>>,
}

impl<T> PresenceJob<T> {
    pub(crate) fn is_finished(&self) -> bool {
        let finished = self.worker.as_ref().is_none_or(JoinHandle::is_finished);
        if !finished {
            crate::background::check_cancel_deadline(
                self.cancelled_at,
                JOIN_TIMEOUT,
                "presence poll",
            );
        }
        finished
    }

    pub(crate) fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.cancelled_at.get_or_insert_with(Instant::now);
    }

    pub(crate) fn take_result(&self) -> Option<Result<T, PresenceError>> {
        // Signout can race a completed send. Cancellation is checked by the
        // consumer too, not just immediately before the worker sends.
        if self.cancel.load(Ordering::Acquire) {
            return None;
        }
        self.receiver.try_recv().ok()
    }

    pub(crate) fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            join_background_worker(worker, JOIN_TIMEOUT, "presence poll");
        }
    }
}

impl<T> Drop for PresenceJob<T> {
    fn drop(&mut self) {
        self.cancel();
        self.join();
    }
}

/// The relay speaks WebSocket on `/ws` and HTTP on the same origin and port,
/// because Azure Container Apps exposes one ingress. Deriving the presence URL
/// from the configured signalling URL keeps `ORANGE_SERVER` the single place a
/// developer points the app somewhere else.
pub(crate) fn presence_url(server: &str) -> Option<String> {
    let (scheme, rest) = match server.split_once("://") {
        Some(("wss", rest)) => ("https", rest),
        Some(("ws", rest)) => ("http", rest),
        _ => return None,
    };
    let origin = rest.split('/').next().filter(|origin| !origin.is_empty())?;
    Some(format!("{scheme}://{origin}/presence"))
}

/// Lazily built on the worker, then shared across polls without keeping an
/// idle polling thread. The mutex only protects construction, not HTTP I/O.
#[derive(Clone, Default)]
pub(crate) struct PresenceClient(Arc<Mutex<Option<reqwest::blocking::Client>>>);

impl PresenceClient {
    fn client(&self) -> Result<reqwest::blocking::Client, PresenceError> {
        let mut client = self
            .0
            .lock()
            .map_err(|error| PresenceError::Unreachable(error.to_string()))?;
        if client.is_none() {
            *client = Some(
                reqwest::blocking::Client::builder()
                    .connect_timeout(Duration::from_secs(10))
                    .timeout(Duration::from_secs(30))
                    .user_agent(concat!("orange/", env!("CARGO_PKG_VERSION")))
                    .build()
                    .map_err(|error| PresenceError::Unreachable(error.to_string()))?,
            );
        }
        Ok(client.as_ref().expect("client was initialized").clone())
    }
}

pub(crate) fn start(
    job: &mut Option<PresenceJob>,
    client: PresenceClient,
    url: String,
    token: String,
    friends: &[Friend],
    since: Option<&str>,
) {
    // A cancelled request still owns its worker until polling reaps it.
    if job.is_some() || friends.is_empty() {
        return;
    }
    let ids: Vec<String> = friends.iter().map(|friend| friend.id.clone()).collect();
    let since = since.map(str::to_owned);
    *job = Some(start_job(client, move |client| {
        fetch(&client, &url, &token, &ids, since.as_deref())
    }));
}

/// Friend snapshots and mutations use the same cancellable HTTP worker as
/// presence, including the consumer-side check for results queued at signout.
pub(crate) fn start_job<T: Send + 'static>(
    client: PresenceClient,
    request: impl FnOnce(reqwest::blocking::Client) -> Result<T, PresenceError> + Send + 'static,
) -> PresenceJob<T> {
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        if worker_cancel.load(Ordering::Acquire) {
            return;
        }
        let result = client.client().and_then(request);
        if worker_cancel.load(Ordering::Acquire) {
            return;
        }
        let _ = sender.send(result);
    });
    PresenceJob {
        cancel,
        cancelled_at: None,
        receiver,
        worker: Some(worker),
    }
}

/// Why a poll failed, when the difference changes what the user should do.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PresenceError {
    /// The relay answered, and does not know this session.
    ///
    /// Its sessions are in memory, so every relay restart produces this for
    /// everyone at once. Worth its own variant because the fix is a specific
    /// action -- sign in again -- and reporting it as a network failure sends
    /// the user looking at their connection instead.
    SignedOut,
    /// Anything else: no route, TLS, timeout, a 500, malformed JSON.
    Unreachable(String),
}

impl std::fmt::Display for PresenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresenceError::SignedOut => write!(f, "the relay no longer knows this session"),
            PresenceError::Unreachable(detail) => write!(f, "{detail}"),
        }
    }
}

fn fetch(
    client: &reqwest::blocking::Client,
    url: &str,
    token: &str,
    ids: &[String],
    since: Option<&str>,
) -> Result<Snapshot, PresenceError> {
    let send = || -> anyhow::Result<reqwest::blocking::Response> {
        let mut request = client
            .get(url)
            .query(&[("ids", ids.join(","))])
            .query(&[("wait", "20")]);
        if let Some(since) = since.filter(|revision| !revision.is_empty()) {
            request = request.query(&[("since", since)]);
        }
        Ok(request.bearer_auth(token).send()?)
    };
    let response = send().map_err(|error| PresenceError::Unreachable(error.to_string()))?;

    // Checked before `error_for_status`, which flattens every HTTP failure into
    // one message and loses the only distinction the user can act on.
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(PresenceError::SignedOut);
    }
    let response = response
        .error_for_status()
        .map_err(|error| PresenceError::Unreachable(error.to_string()))?;

    serde_json::from_reader(std::io::Read::take(response, RESPONSE_MAX_BYTES))
        .map_err(|error| PresenceError::Unreachable(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successive_presence_polls_reuse_their_http_connection() {
        // Creating a Client inside fetch discarded the connection pool after
        // every 15-second poll, despite the relay origin being unchanged.
        let server = crate::background::tests::HttpServer::new(vec![
            (200, br#"{"friends":[]}"#.to_vec()),
            (200, br#"{"friends":[]}"#.to_vec()),
        ]);
        let friends = vec![Friend {
            id: "42".into(),
            name: "Friend".into(),
            avatar_url: None,
        }];
        let mut job = None;
        let client = PresenceClient::default();
        for token in ["first-session", "second-session"] {
            start(
                &mut job,
                client.clone(),
                server.url("/presence"),
                token.into(),
                &friends,
                None,
            );
            job.as_ref()
                .unwrap()
                .receiver
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            job.take().unwrap().join();
        }
        let requests = server.finish();
        assert_eq!(
            requests[0].0, requests[1].0,
            "each poll created a fresh connection"
        );
        assert!(requests[0].1.starts_with("GET /presence?ids=42&wait=20 "));
        assert!(requests[0]
            .1
            .to_ascii_lowercase()
            .contains("authorization: bearer first-session"));
        assert!(requests[1]
            .1
            .to_ascii_lowercase()
            .contains("authorization: bearer second-session"));
    }

    #[test]
    fn signout_rejects_presence_that_was_already_queued() {
        // A result sent just before cancellation must not repopulate the UI
        // with another account's presence after signout.
        let server = crate::background::tests::HttpServer::new(vec![(
            200,
            br#"{"friends":[{"id":"42","state":"live","code":"ABC-234"}]}"#.to_vec(),
        )]);
        let friends = vec![Friend {
            id: "42".into(),
            name: "Friend".into(),
            avatar_url: None,
        }];
        let mut job = None;
        start(
            &mut job,
            PresenceClient::default(),
            server.url("/presence"),
            "secret".into(),
            &friends,
            None,
        );
        let job = job.as_mut().unwrap();
        job.join();
        job.cancel();
        assert!(job.take_result().is_none());
        assert_eq!(server.finish().len(), 1);
    }

    #[test]
    fn the_presence_scheduler_retains_a_cancelled_worker_until_it_is_reaped() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (release_tx, release_rx) = mpsc::channel();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
            let _ = sender.send(Ok(Snapshot {
                friends: Vec::new(),
                revision: None,
            }));
        });
        let mut job = Some(PresenceJob {
            cancel: Arc::clone(&cancel),
            cancelled_at: None,
            receiver,
            worker: Some(worker),
        });
        job.as_mut().unwrap().cancel();
        start(
            &mut job,
            PresenceClient::default(),
            "http://127.0.0.1:1/presence".into(),
            "new".into(),
            &[Friend {
                id: "42".into(),
                name: "Friend".into(),
                avatar_url: None,
            }],
            None,
        );
        assert!(Arc::ptr_eq(&job.as_ref().unwrap().cancel, &cancel));
        assert!(!job.as_ref().unwrap().is_finished());
        release_tx.send(()).unwrap();
        job.as_mut().unwrap().join();
        assert!(job.as_ref().unwrap().take_result().is_none());
    }

    #[test]
    fn presence_http_errors_and_response_limits_survive_client_reuse() {
        // Reusing transport must not flatten the actionable 401 distinction,
        // or remove the existing bounded JSON reader.
        let server = crate::background::tests::HttpServer::new(vec![
            (401, Vec::new()),
            (503, Vec::new()),
            (200, vec![b'x'; RESPONSE_MAX_BYTES as usize + 1]),
        ]);
        let client = PresenceClient::default();
        let http = client.client().unwrap();
        assert_eq!(
            fetch(&http, &server.url("/presence"), "secret", &[], None),
            Err(PresenceError::SignedOut)
        );
        assert!(matches!(
            fetch(&http, &server.url("/presence"), "secret", &[], None),
            Err(PresenceError::Unreachable(_))
        ));
        assert!(matches!(
            fetch(&http, &server.url("/presence"), "secret", &[], None),
            Err(PresenceError::Unreachable(_))
        ));
        assert_eq!(server.finish().len(), 3);
    }

    #[test]
    fn revision_aware_polls_send_since_but_legacy_omits_it() {
        let server = crate::background::tests::HttpServer::new(vec![
            (200, br#"{"friends":[],"revision":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}"#.to_vec()),
            (200, br#"{"friends":[]}"#.to_vec()),
        ]);
        let friends = vec![Friend {
            id: "42".into(),
            name: "Friend".into(),
            avatar_url: None,
        }];
        let mut job = None;
        start(
            &mut job,
            PresenceClient::default(),
            server.url("/presence"),
            "secret".into(),
            &friends,
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
        );
        job.take().unwrap().join();
        start(
            &mut job,
            PresenceClient::default(),
            server.url("/presence"),
            "secret".into(),
            &friends,
            None,
        );
        job.take().unwrap().join();

        let requests = server.finish();
        assert!(requests[0]
            .1
            .starts_with("GET /presence?ids=42&wait=20&since=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef "));
        assert!(requests[1].1.starts_with("GET /presence?ids=42&wait=20 "));
    }

    /// `ORANGE_SERVER` and the shipped default are both WebSocket URLs ending
    /// in `/ws`. Building the HTTP origin by hand in two places is how they
    /// drift, so this pins the one derivation.
    #[test]
    fn presence_url_follows_the_configured_signalling_origin() {
        assert_eq!(
            presence_url("wss://relay.example.com/ws").as_deref(),
            Some("https://relay.example.com/presence")
        );
        assert_eq!(
            presence_url("ws://127.0.0.1:9000/ws").as_deref(),
            Some("http://127.0.0.1:9000/presence")
        );
        assert_eq!(presence_url("https://relay.example.com/ws"), None);
        assert_eq!(presence_url("relay.example.com"), None);
        assert_eq!(presence_url("ws:///ws"), None);
    }

    /// The client pairs answers to rows by id. Decoding has to survive the relay
    /// returning them in an order the client did not ask for, has to keep `full`
    /// distinct from `live` or a full room renders as joinable, and has to
    /// tolerate a profile being absent for an offline friend.
    #[test]
    fn presence_body_decodes_every_state_and_an_optional_profile() {
        let body: Snapshot = serde_json::from_str(
            r#"{"friends":[
                {"id":"a","state":"offline"},
                {"id":"b","name":"Bee","avatar_url":"https://cdn/b.png","state":"live","code":"ABC-234"},
                {"id":"c","state":"full"}
            ]}"#,
        )
        .unwrap();

        assert_eq!(
            body.friends,
            vec![
                Entry {
                    id: "a".into(),
                    name: None,
                    avatar_url: None,
                    presence: Presence::Offline
                },
                Entry {
                    id: "b".into(),
                    name: Some("Bee".into()),
                    avatar_url: Some("https://cdn/b.png".into()),
                    presence: Presence::Live {
                        code: "ABC-234".into()
                    }
                },
                Entry {
                    id: "c".into(),
                    name: None,
                    avatar_url: None,
                    presence: Presence::Full
                },
            ]
        );
        assert_eq!(body.revision, None);
        let revision = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let with_revision: Snapshot =
            serde_json::from_str(&format!("{{\"friends\":[],\"revision\":\"{revision}\"}}"))
                .unwrap();
        assert_eq!(with_revision.revision.as_deref(), Some(revision));
    }

    /// A relay that has forgotten this session answers 401, and the fix is a
    /// specific action the user can take. Folding it into the generic HTTP
    /// error produced "Could not reach the relay" over a raw URL: it described
    /// a network fault that had not happened and named no remedy. Every relay
    /// deploy shows this to everyone at once, because sessions are in memory.
    #[test]
    fn a_forgotten_session_is_reported_as_signed_out_not_as_a_network_fault() {
        assert_eq!(
            PresenceError::SignedOut.to_string(),
            "the relay no longer knows this session"
        );
        // Anything else keeps its detail, which is the only useful thing to
        // show when the cause really is the network.
        assert_eq!(
            PresenceError::Unreachable("dns error".into()).to_string(),
            "dns error"
        );
        assert_ne!(
            PresenceError::SignedOut,
            PresenceError::Unreachable("401 Unauthorized".into())
        );
    }
}
