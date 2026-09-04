//! Asking the relay which friends are streaming right now.
//!
//! This is a poll rather than a pushed subscription. A pushed one would need a
//! held WebSocket per signed-in user, and the relay only has 512 connections in
//! total, shared with every host and viewer; presence would then cost more
//! capacity than streaming does. Polling costs nothing from that budget because
//! the semaphore only guards `/ws`.
//!
//! Latency is the price: a friend who goes live is invisible for up to
//! `INTERVAL`. That is a deliberate trade, not an oversight.

use crate::{background::join_background_worker, session::Friend};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

/// How often the client asks. Chosen as the largest delay that still feels like
/// "my friend just went live" rather than "I refreshed and noticed".
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

#[derive(Debug, Deserialize)]
struct Body {
    friends: Vec<Entry>,
}

pub(crate) struct PresenceJob {
    cancel: Arc<AtomicBool>,
    pub(crate) receiver: mpsc::Receiver<Result<Vec<Entry>, PresenceError>>,
    worker: Option<JoinHandle<()>>,
}

impl PresenceJob {
    pub(crate) fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(crate) fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            join_background_worker(worker, JOIN_TIMEOUT, "presence poll");
        }
    }
}

impl Drop for PresenceJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
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

pub(crate) fn start(job: &mut Option<PresenceJob>, url: String, token: String, friends: &[Friend]) {
    drop(job.take());
    if friends.is_empty() {
        return;
    }
    let ids: Vec<String> = friends.iter().map(|friend| friend.id.clone()).collect();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        if worker_cancel.load(Ordering::Acquire) {
            return;
        }
        let result = fetch(&url, &token, &ids);
        if worker_cancel.load(Ordering::Acquire) {
            return;
        }
        let _ = sender.send(result);
    });
    *job = Some(PresenceJob {
        cancel,
        receiver,
        worker: Some(worker),
    });
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

fn fetch(url: &str, token: &str, ids: &[String]) -> Result<Vec<Entry>, PresenceError> {
    let send = || -> anyhow::Result<reqwest::blocking::Response> {
        Ok(reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("orange/", env!("CARGO_PKG_VERSION")))
            .build()?
            .get(url)
            .query(&[("ids", ids.join(","))])
            .bearer_auth(token)
            .send()?)
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

    let body: Body = serde_json::from_reader(std::io::Read::take(response, RESPONSE_MAX_BYTES))
        .map_err(|error| PresenceError::Unreachable(error.to_string()))?;
    Ok(body.friends)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let body: Body = serde_json::from_str(
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
