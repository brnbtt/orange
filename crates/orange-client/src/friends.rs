//! Server-owned roster and request inbox. One worker serializes polls and
//! mutations, so a pre-mutation snapshot can never overwrite the new result.

use crate::{
    presence::{self, PresenceClient, PresenceError, PresenceJob},
    session::Friend,
};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Contact {
    #[serde(flatten)]
    pub profile: Friend,
    pub revision: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub(crate) struct Snapshot {
    pub friends: Vec<Contact>,
    pub incoming: Vec<Contact>,
    pub outgoing: Vec<Contact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Action {
    Request,
    Accept,
    Decline,
    Cancel,
    Remove,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Change {
    pub action: Action,
    pub target_id: String,
    pub revision: Option<String>,
}

pub(crate) struct Sync {
    pub snapshot: Snapshot,
    pub error: Option<PresenceError>,
    pub synced: bool,
    job: Option<PresenceJob<Option<Snapshot>>>,
    pending: Option<Change>,
    active: Option<Change>,
    due: Instant,
}

impl Default for Sync {
    fn default() -> Self {
        Self {
            snapshot: Snapshot::default(),
            error: None,
            synced: false,
            job: None,
            pending: None,
            active: None,
            due: Instant::now(),
        }
    }
}

pub(crate) enum Event {
    Snapshot,
    Changed(Change),
    Failed { mutation: bool },
}

impl Sync {
    pub fn busy(&self) -> bool {
        self.pending.is_some() || self.active.is_some()
    }

    pub fn request(&mut self, change: Change) -> bool {
        if self.busy() || !self.synced {
            return false;
        }
        self.pending = Some(change);
        true
    }

    pub fn reset(&mut self) {
        if let Some(job) = &mut self.job {
            job.cancel();
        }
        self.pending = None;
        self.active = None;
        self.snapshot = Snapshot::default();
        self.error = None;
        self.synced = false;
        self.due = Instant::now();
    }

    pub fn refresh(&mut self) {
        self.due = Instant::now();
    }

    pub fn poll(&mut self, client: PresenceClient, session: Option<(&str, &str)>) -> Option<Event> {
        if let Some(job) = &self.job {
            if !job.is_finished() {
                return None;
            }
            let result = job.take_result();
            self.job.take().unwrap().join();
            let active = self.active.take();
            match result {
                Some(Ok(Some(snapshot))) => {
                    self.snapshot = snapshot;
                    self.synced = true;
                    self.error = None;
                    return Some(Event::Snapshot);
                }
                Some(Ok(None)) => {
                    self.due = Instant::now();
                    self.error = None;
                    return active.map(Event::Changed);
                }
                Some(Err(error)) => {
                    self.error = Some(error);
                    // A response can be lost after a write committed. Always
                    // reconcile rather than locally guessing the new state.
                    if active.is_some() {
                        self.due = Instant::now();
                    }
                    return Some(Event::Failed {
                        mutation: active.is_some(),
                    });
                }
                None => return None,
            }
        }
        let (server, token) = session?;
        if self.pending.is_none() && Instant::now() < self.due {
            return None;
        }
        let Some(url) = presence::presence_url(server)
            .map(|url| url.trim_end_matches("presence").to_owned() + "friends")
        else {
            self.error = Some(PresenceError::Unreachable("Invalid relay URL".into()));
            self.due = Instant::now() + Duration::from_secs(15);
            return None;
        };
        let token = token.to_string();
        self.active = self.pending.take();
        let change = self.active.clone();
        self.due = Instant::now() + Duration::from_secs(15);
        self.job = Some(presence::start_job(client, move |client| {
            let response = match change {
                Some(ref change) => client.post(&url).bearer_auth(&token).json(change).send(),
                None => client.get(&url).bearer_auth(&token).send(),
            }
            .map_err(|error| PresenceError::Unreachable(error.to_string()))?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                return Err(PresenceError::SignedOut);
            }
            if !response.status().is_success() {
                let status = response.status();
                let mut detail = String::new();
                std::io::Read::read_to_string(
                    &mut std::io::Read::take(response, 4096),
                    &mut detail,
                )
                .map_err(|error| PresenceError::Unreachable(error.to_string()))?;
                return Err(PresenceError::Unreachable(
                    if status == reqwest::StatusCode::NOT_FOUND && change.is_none() {
                        "This relay needs an update to support friend requests.".into()
                    } else {
                        format!(
                            "{status}: {}",
                            detail
                                .split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ")
                                .chars()
                                .take(240)
                                .collect::<String>()
                        )
                    },
                ));
            }
            if change.is_some() {
                return Ok(None);
            }
            serde_json::from_reader(std::io::Read::take(response, 1024 * 1024))
                .map(Some)
                .map_err(|error| PresenceError::Unreachable(error.to_string()))
        }));
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::tests::HttpServer;

    fn finish(sync: &mut Sync, client: &PresenceClient, server: &str) -> Option<Event> {
        sync.poll(client.clone(), Some((server, "token")));
        sync.job.as_mut().unwrap().join();
        sync.poll(client.clone(), Some((server, "token")))
    }

    #[test]
    fn accepting_uses_the_displayed_revision_and_refreshes_the_authoritative_roster() {
        // A mutation must not optimistically invent a friendship or race a
        // preceding poll. The final GET supplies the accepted roster.
        let server = HttpServer::new(vec![
            (200, br#"{"friends":[],"incoming":[{"id":"42","name":"Friend","revision":"r1"}],"outgoing":[]}"#.to_vec()),
            (204, vec![]),
            (200, br#"{"friends":[{"id":"42","name":"Friend","revision":"r1"}],"incoming":[],"outgoing":[]}"#.to_vec()),
        ]);
        let url = server.url("/ws").replacen("http://", "ws://", 1);
        let client = PresenceClient::default();
        let mut sync = Sync::default();
        assert!(matches!(
            finish(&mut sync, &client, &url),
            Some(Event::Snapshot)
        ));
        assert!(sync.request(Change {
            action: Action::Accept,
            target_id: "42".into(),
            revision: Some(sync.snapshot.incoming[0].revision.clone())
        }));
        assert!(matches!(
            finish(&mut sync, &client, &url),
            Some(Event::Changed(_))
        ));
        assert!(sync.snapshot.friends.is_empty());
        assert!(matches!(
            finish(&mut sync, &client, &url),
            Some(Event::Snapshot)
        ));
        assert_eq!(sync.snapshot.friends[0].profile.id, "42");
        assert!(sync.snapshot.incoming.is_empty());
        let requests = server.finish();
        assert!(requests[0].1.starts_with("GET /friends "));
        assert!(requests[1].1.starts_with("POST /friends "));
        let body: serde_json::Value =
            serde_json::from_str(requests[1].1.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"action":"accept","target_id":"42","revision":"r1"})
        );
        assert_eq!(
            requests[0].0, requests[2].0,
            "friends sync discarded the connection pool"
        );
    }

    #[test]
    fn resetting_an_account_discards_a_snapshot_that_has_already_arrived() {
        // Cancellation must be checked by the consumer as well as the worker:
        // a result may have been sent just before signout or reauthentication.
        let server = HttpServer::new(vec![(200, br#"{"friends":[{"id":"42","name":"Old account friend","revision":"r1"}],"incoming":[],"outgoing":[]}"#.to_vec())]);
        let url = server.url("/ws").replacen("http://", "ws://", 1);
        let client = PresenceClient::default();
        let mut sync = Sync::default();
        sync.poll(client.clone(), Some((&url, "old-token")));
        sync.job.as_mut().unwrap().join();
        sync.reset();
        assert!(sync.poll(client, Some((&url, "new-token"))).is_none());
        assert!(!sync.synced);
        assert!(sync.snapshot.friends.is_empty());
        assert!(sync.error.is_none());
        server.finish();
    }

    #[test]
    fn only_a_rejected_session_is_classified_as_signed_out() {
        let server = HttpServer::new(vec![(503, b"storage unavailable".to_vec()), (401, vec![])]);
        let url = server.url("/ws").replacen("http://", "ws://", 1);
        let client = PresenceClient::default();
        let mut sync = Sync::default();
        assert!(matches!(
            finish(&mut sync, &client, &url),
            Some(Event::Failed { mutation: false })
        ));
        assert!(matches!(sync.error, Some(PresenceError::Unreachable(_))));
        sync.refresh();
        assert!(matches!(
            finish(&mut sync, &client, &url),
            Some(Event::Failed { mutation: false })
        ));
        assert_eq!(sync.error, Some(PresenceError::SignedOut));
        server.finish();
    }
}
