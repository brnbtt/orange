//! Account-owned friend requests and mutual friendships. A canonical pair row
//! is the authority for both users, so acceptance cannot save only one side.

use crate::{auth::Identity, store::TableStore};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Action {
    Request,
    Accept,
    Decline,
    Cancel,
    Remove,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Change {
    pub action: Action,
    pub target_id: String,
    #[serde(default)]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RelationshipState {
    Pending,
    Accepted,
    Declined,
    Cancelled,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Relationship {
    pub sender: Identity,
    pub recipient: Identity,
    pub revision: String,
    pub state: RelationshipState,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Contact {
    #[serde(flatten)]
    pub profile: Identity,
    pub revision: String,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct Snapshot {
    pub friends: Vec<Contact>,
    pub incoming: Vec<Contact>,
    pub outgoing: Vec<Contact>,
}

#[derive(Debug)]
pub(crate) enum Error {
    Invalid(&'static str),
    Forbidden,
    Conflict,
    NotFound,
    Capacity,
    Storage(anyhow::Error),
}

impl From<anyhow::Error> for Error {
    fn from(error: anyhow::Error) -> Self {
        Self::Storage(error)
    }
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.parse::<u64>()
        .is_ok_and(|n| n != 0 && n.to_string() == id)
}

fn pair_key(a: &str, b: &str) -> String {
    if a < b {
        format!("{a}-{b}")
    } else {
        format!("{b}-{a}")
    }
}

impl Relationship {
    fn transition(&mut self, actor: &str, change: &Change) -> Result<bool, Error> {
        if actor != self.sender.id && actor != self.recipient.id {
            return Err(Error::Forbidden);
        }
        if change.action == Action::Request {
            return Ok(false);
        }
        // A stale Accept must not accept a new request sent after cancellation.
        if change.revision.as_deref() != Some(self.revision.as_str()) {
            return Err(Error::Conflict);
        }
        let (owner, from, to) = match change.action {
            Action::Accept => (
                self.recipient.id.as_str(),
                RelationshipState::Pending,
                RelationshipState::Accepted,
            ),
            Action::Decline => (
                self.recipient.id.as_str(),
                RelationshipState::Pending,
                RelationshipState::Declined,
            ),
            Action::Cancel => (
                self.sender.id.as_str(),
                RelationshipState::Pending,
                RelationshipState::Cancelled,
            ),
            Action::Remove => (
                actor,
                RelationshipState::Accepted,
                RelationshipState::Removed,
            ),
            Action::Request => unreachable!(),
        };
        if actor != owner {
            return Err(Error::Forbidden);
        }
        if self.state == to {
            return Ok(false);
        }
        if self.state != from {
            return Err(Error::Conflict);
        }
        self.state = to;
        Ok(true)
    }
}

#[derive(Default)]
struct Memory {
    profiles: HashMap<String, Identity>,
    pairs: HashMap<String, (Relationship, u64)>,
    changes: HashMap<String, (std::time::Instant, u32)>,
    reads: HashMap<String, (std::time::Instant, u32)>,
}

#[derive(Clone)]
pub(crate) struct Social {
    store: Option<TableStore>,
    memory: Arc<Mutex<Memory>>,
    requests: Arc<tokio::sync::Semaphore>,
    mutations: Arc<Mutex<()>>,
}

impl Social {
    pub(crate) fn new(store: Option<TableStore>) -> Self {
        Self {
            store,
            memory: Default::default(),
            requests: Arc::new(tokio::sync::Semaphore::new(32)),
            mutations: Default::default(),
        }
    }

    pub(crate) async fn register(&self, identity: &Identity) -> Result<(), Error> {
        if !valid_id(&identity.id) {
            return Err(Error::Invalid("invalid account id"));
        }
        {
            let memory = self.memory.lock().await;
            if memory
                .profiles
                .get(&identity.id)
                .is_some_and(|p| p.name == identity.name && p.avatar_url == identity.avatar_url)
            {
                return Ok(());
            }
        }
        if let Some(store) = &self.store {
            store.put_profile(identity).await?;
        }
        let mut memory = self.memory.lock().await;
        if memory.profiles.len() >= 4096 && !memory.profiles.contains_key(&identity.id) {
            if self.store.is_none() {
                return Err(Error::Capacity);
            }
            if let Some(key) = memory.profiles.keys().next().cloned() {
                memory.profiles.remove(&key);
            }
        }
        memory
            .profiles
            .insert(identity.id.clone(), identity.clone());
        Ok(())
    }

    pub(crate) fn admit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, Error> {
        self.requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Capacity)
    }

    pub(crate) async fn admit_read(&self, id: &str) -> Result<(), Error> {
        let mut memory = self.memory.lock().await;
        memory
            .reads
            .retain(|_, (since, _)| since.elapsed() < std::time::Duration::from_secs(60));
        if memory.reads.len() >= 4096 && !memory.reads.contains_key(id) {
            return Err(Error::Capacity);
        }
        let (_, count) = memory
            .reads
            .entry(id.into())
            .or_insert_with(|| (std::time::Instant::now(), 0));
        if *count >= 60 {
            return Err(Error::Capacity);
        }
        *count += 1;
        Ok(())
    }

    async fn profile(&self, id: &str) -> Result<Identity, Error> {
        if let Some(store) = &self.store {
            return store.get_profile(id).await?.ok_or(Error::NotFound);
        }
        self.memory
            .lock()
            .await
            .profiles
            .get(id)
            .cloned()
            .ok_or(Error::NotFound)
    }

    pub(crate) async fn relationships(&self, id: &str) -> Result<Vec<Relationship>, Error> {
        if let Some(store) = &self.store {
            return Ok(store.relationships(id).await?);
        }
        Ok(self
            .memory
            .lock()
            .await
            .pairs
            .values()
            .filter(|(r, _)| r.sender.id == id || r.recipient.id == id)
            .map(|(r, _)| r.clone())
            .collect())
    }

    pub(crate) async fn snapshot(&self, identity: &Identity) -> Result<Snapshot, Error> {
        self.register(identity).await?;
        let mut snapshot = Snapshot::default();
        for relationship in self.relationships(&identity.id).await? {
            let outgoing = relationship.sender.id == identity.id;
            let profile = if outgoing {
                relationship.recipient
            } else {
                relationship.sender
            };
            let contact = Contact {
                profile,
                revision: relationship.revision,
            };
            match relationship.state {
                RelationshipState::Accepted => snapshot.friends.push(contact),
                RelationshipState::Pending if outgoing => snapshot.outgoing.push(contact),
                RelationshipState::Pending => snapshot.incoming.push(contact),
                _ => {}
            }
        }
        for list in [
            &mut snapshot.friends,
            &mut snapshot.incoming,
            &mut snapshot.outgoing,
        ] {
            list.sort_by(|a, b| {
                a.profile
                    .name
                    .cmp(&b.profile.name)
                    .then(a.profile.id.cmp(&b.profile.id))
            });
        }
        Ok(snapshot)
    }

    pub(crate) async fn change(&self, identity: &Identity, change: &Change) -> Result<(), Error> {
        if !valid_id(&change.target_id) || change.target_id == identity.id {
            return Err(Error::Invalid("choose another Orange account"));
        }
        {
            let mut memory = self.memory.lock().await;
            memory
                .changes
                .retain(|_, (since, _)| since.elapsed() < std::time::Duration::from_secs(60));
            if memory.changes.len() >= 4096 && !memory.changes.contains_key(&identity.id) {
                return Err(Error::Capacity);
            }
            let (_, count) = memory
                .changes
                .entry(identity.id.clone())
                .or_insert_with(|| (std::time::Instant::now(), 0));
            if *count >= 30 {
                return Err(Error::Capacity);
            }
            *count += 1;
        }
        // The deployed relay is single-replica. Serialize admission as well as
        // pair writes so two different requests cannot both take slot 256.
        // ponytail: a global mutation lock suits this small relay; use atomic
        // account quotas before scaling to multiple writer replicas.
        let _mutation =
            tokio::time::timeout(std::time::Duration::from_secs(2), self.mutations.lock())
                .await
                .map_err(|_| Error::Capacity)?;
        self.register(identity).await?;
        let key = pair_key(&identity.id, &change.target_id);
        for _ in 0..4 {
            let old = if let Some(store) = &self.store {
                store.get_relationship(&key).await?
            } else {
                self.memory
                    .lock()
                    .await
                    .pairs
                    .get(&key)
                    .map(|(r, version)| (r.clone(), version.to_string()))
            };
            let etag = old.as_ref().map(|(_, etag)| etag.clone());
            let fresh = change.action == Action::Request
                && old.as_ref().is_none_or(|(r, _)| {
                    !matches!(
                        r.state,
                        RelationshipState::Pending | RelationshipState::Accepted
                    )
                });
            let relationship = if fresh {
                // Bound each account's active inbox/roster. The canonical row
                // and ETag below remain the consistency boundary for a pair.
                for id in [&identity.id, &change.target_id] {
                    let history = self.relationships(id).await?;
                    if etag.is_none() && history.len() >= 4096 {
                        return Err(Error::Capacity);
                    }
                    if history
                        .iter()
                        .filter(|r| {
                            matches!(
                                r.state,
                                RelationshipState::Pending | RelationshipState::Accepted
                            )
                        })
                        .count()
                        >= 256
                    {
                        return Err(Error::Capacity);
                    }
                }
                Relationship {
                    sender: identity.clone(),
                    recipient: self.profile(&change.target_id).await?,
                    revision: format!("{:032x}", rand::random::<u128>()),
                    state: RelationshipState::Pending,
                }
            } else {
                let Some((mut relationship, _)) = old else {
                    return Err(Error::NotFound);
                };
                if !relationship.transition(&identity.id, change)? {
                    return Ok(());
                }
                // Capture the authenticated recipient's current profile when
                // accepting, never the display name embedded in a pasted code.
                if relationship.recipient.id == identity.id {
                    relationship.recipient = identity.clone();
                }
                relationship
            };
            let saved = if let Some(store) = &self.store {
                store
                    .save_relationship(&key, &relationship, etag.as_deref())
                    .await?
            } else {
                let mut memory = self.memory.lock().await;
                let version = memory.pairs.get(&key).map(|(_, version)| *version);
                if version.map(|v| v.to_string()) != etag {
                    false
                } else {
                    if memory.pairs.len() >= 65536 && version.is_none() {
                        return Err(Error::Capacity);
                    }
                    memory
                        .pairs
                        .insert(key.clone(), (relationship, version.unwrap_or(0) + 1));
                    true
                }
            };
            if saved {
                return Ok(());
            }
        }
        Err(Error::Conflict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(id: &str) -> Identity {
        Identity {
            id: id.into(),
            name: format!("User {id}"),
            avatar_url: None,
        }
    }
    fn change(action: Action, target: &str, revision: Option<&str>) -> Change {
        Change {
            action,
            target_id: target.into(),
            revision: revision.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn one_acceptance_adds_both_accounts_and_retries_do_not_duplicate_them() {
        let social = Social::new(None);
        let a = identity("1");
        let b = identity("2");
        social.register(&b).await.unwrap();
        social
            .change(&a, &change(Action::Request, "2", None))
            .await
            .unwrap();
        let inbox = social.snapshot(&b).await.unwrap();
        assert!(inbox.friends.is_empty());
        assert_eq!(inbox.incoming[0].profile.id, "1");
        let revision = &inbox.incoming[0].revision;
        assert!(matches!(
            social
                .change(&a, &change(Action::Accept, "2", Some(revision)))
                .await,
            Err(Error::Forbidden)
        ));
        for _ in 0..2 {
            social
                .change(&b, &change(Action::Accept, "1", Some(revision)))
                .await
                .unwrap();
        }
        assert_eq!(
            social.snapshot(&a).await.unwrap().friends[0].profile.id,
            "2"
        );
        let b = social.snapshot(&b).await.unwrap();
        assert_eq!(b.friends.len(), 1);
        assert!(b.incoming.is_empty());
        social
            .change(&a, &change(Action::Remove, "2", Some(revision)))
            .await
            .unwrap();
        assert!(social.snapshot(&a).await.unwrap().friends.is_empty());
        assert!(social
            .snapshot(&identity("2"))
            .await
            .unwrap()
            .friends
            .is_empty());
    }

    #[tokio::test]
    async fn cancelled_requests_cannot_be_accepted_by_a_stale_inbox_action() {
        let social = Social::new(None);
        let a = identity("1");
        let b = identity("2");
        social.register(&b).await.unwrap();
        social
            .change(&a, &change(Action::Request, "2", None))
            .await
            .unwrap();
        let revision = social.snapshot(&b).await.unwrap().incoming[0]
            .revision
            .clone();
        social
            .change(&a, &change(Action::Cancel, "2", Some(&revision)))
            .await
            .unwrap();
        social
            .change(&a, &change(Action::Request, "2", None))
            .await
            .unwrap();
        assert!(matches!(
            social
                .change(&b, &change(Action::Accept, "1", Some(&revision)))
                .await,
            Err(Error::Conflict)
        ));
        assert!(social.snapshot(&b).await.unwrap().friends.is_empty());
    }

    #[tokio::test]
    async fn concurrent_requests_cannot_overfill_a_presence_sized_roster() {
        // Different pair ETags do not protect an account quota. Admission must
        // serialize the count and insertion on this single-replica relay.
        let social = Social::new(None);
        let a = identity("1");
        social.register(&identity("2")).await.unwrap();
        social.register(&identity("3")).await.unwrap();
        {
            let mut memory = social.memory.lock().await;
            for id in 1000..1255 {
                let b = identity(&id.to_string());
                memory.pairs.insert(
                    pair_key(&a.id, &b.id),
                    (
                        Relationship {
                            sender: a.clone(),
                            recipient: b,
                            revision: id.to_string(),
                            state: RelationshipState::Accepted,
                        },
                        1,
                    ),
                );
            }
        }
        let one = change(Action::Request, "2", None);
        let two = change(Action::Request, "3", None);
        let (one, two) = tokio::join!(social.change(&a, &one), social.change(&a, &two));
        assert_eq!(usize::from(one.is_ok()) + usize::from(two.is_ok()), 1);
        let snapshot = social.snapshot(&a).await.unwrap();
        assert_eq!(snapshot.friends.len() + snapshot.outgoing.len(), 256);
    }

    #[tokio::test]
    async fn a_full_history_rejects_new_pairs_without_breaking_existing_inboxes() {
        // Tombstones revoke legacy discovery. Keep them, but reject new pairs
        // before the bounded reader would start failing on every request.
        let social = Social::new(None);
        let a = identity("1");
        social.register(&identity("2")).await.unwrap();
        {
            let mut memory = social.memory.lock().await;
            for id in 1000..5096 {
                let b = identity(&id.to_string());
                memory.pairs.insert(
                    pair_key(&a.id, &b.id),
                    (
                        Relationship {
                            sender: a.clone(),
                            recipient: b,
                            revision: id.to_string(),
                            state: RelationshipState::Removed,
                        },
                        1,
                    ),
                );
            }
        }
        assert!(matches!(
            social.change(&a, &change(Action::Request, "2", None)).await,
            Err(Error::Capacity)
        ));
        assert!(social.snapshot(&a).await.unwrap().friends.is_empty());
        assert_eq!(social.relationships("1").await.unwrap().len(), 4096);
    }

    #[tokio::test]
    async fn social_reads_have_bounded_rate_and_concurrency() {
        // HTTP polling must not bypass the resource bound simply because it
        // does not consume one of the relay's WebSocket permits.
        let social = Social::new(None);
        let permits: Vec<_> = (0..32).map(|_| social.admit().unwrap()).collect();
        assert!(matches!(social.admit(), Err(Error::Capacity)));
        drop(permits);
        assert!(social.admit().is_ok());
        for _ in 0..60 {
            social.admit_read("1").await.unwrap();
        }
        assert!(matches!(social.admit_read("1").await, Err(Error::Capacity)));
        assert!(social.admit_read("2").await.is_ok());
    }
}
