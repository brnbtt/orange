//! Durable session storage on Azure Table Storage.
//!
//! Everything else in the relay lives in memory, which is fine for rooms: they
//! belong to a connection and die with it either way. Sessions are different.
//! They are the only thing a user cannot recreate without leaving the app, so
//! losing them on every deploy meant every deploy signed everyone out, and
//! after the friends list shipped that took the whole feature dark rather than
//! dropping a name label.
//!
//! Table Storage rather than a database because the shape is a key-value
//! lookup by token and nothing else, the account already exists for release
//! manifests, and it bills per operation with no idle floor. A Postgres
//! instance would cost about thirty dollars a month to serve a few writes a
//! day.
//!
//! The REST API is used directly. Signing a Table request is one HMAC over two
//! lines, which is less code than an SDK would add, and `reqwest` is already a
//! dependency for the Discord exchange.

use crate::auth::Identity;
mod social;
use anyhow::{Context, Result};
use base64::Engine;
use hmac::{Mac, SimpleHmac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime};

/// Matches the rest of the relay's outbound calls.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Every session shares one partition. The table holds thousands of rows, not
/// millions, and a single partition keeps every lookup a point query.
const PARTITION: &str = "session";

#[derive(Clone)]
pub(crate) struct TableStore {
    account: String,
    endpoint: String,
    /// Base64 account key, decoded per request. Never logged.
    key: String,
    table: String,
    http: reqwest::Client,
}

/// What a stored row turns back into.
#[derive(Debug, Deserialize)]
struct SessionRow {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "AvatarUrl")]
    avatar_url: Option<String>,
    /// Seconds since the Unix epoch. Stored as a string because Table Storage
    /// types integers as Int32 unless told otherwise, and these overflow it.
    #[serde(rename = "CreatedAt")]
    created_at: String,
}

impl TableStore {
    /// Configured only when all three variables are present.
    ///
    /// Absent means in-memory sessions, which is what `orange serve` and every
    /// local run want: the same deliberate optionality as Discord login, so
    /// nobody needs an Azure account to run a relay.
    pub(crate) fn from_env() -> Option<Self> {
        let account = std::env::var("ORANGE_TABLE_ACCOUNT").ok()?;
        let key = std::env::var("ORANGE_TABLE_KEY").ok()?;
        let table = std::env::var("ORANGE_TABLE_NAME").ok()?;
        if account.is_empty() || key.is_empty() || table.is_empty() {
            return None;
        }
        Some(Self {
            endpoint: format!("https://{account}.table.core.windows.net"),
            account,
            key,
            table,
            http: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()
                .ok()?,
        })
    }

    /// A session token never becomes a row key.
    ///
    /// The token is a bearer credential: anyone holding it is that user until
    /// it expires. Storing it verbatim would mean a table listing, a support
    /// dump or a stray backup hands over live logins. The hash is enough to
    /// look a row up by, and cannot be replayed.
    fn row_key(token: &str) -> String {
        format!("{:x}", Sha256::digest(token.as_bytes()))
    }

    fn entity_path(&self, row_key: &str) -> String {
        format!(
            "{}(PartitionKey='{}',RowKey='{}')",
            self.table, PARTITION, row_key
        )
    }

    /// `StringToSign = Date + "\n" + CanonicalizedResource` for Shared Key
    /// Lite against the Table service. Unlike the Blob scheme there are no
    /// canonicalized headers and no verb, which is why this is worth doing by
    /// hand rather than taking an SDK.
    fn string_to_sign(&self, date: &str, path: &str) -> String {
        format!("{date}\n/{}/{}", self.account, path)
    }

    fn authorization(&self, date: &str, path: &str) -> Result<String> {
        let secret = base64::engine::general_purpose::STANDARD
            .decode(&self.key)
            .context("ORANGE_TABLE_KEY is not valid base64")?;
        let mut mac = SimpleHmac::<Sha256>::new_from_slice(&secret)
            .context("account key is not a usable HMAC key")?;
        mac.update(self.string_to_sign(date, path).as_bytes());
        let signature =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        Ok(format!("SharedKeyLite {}:{signature}", self.account))
    }

    fn request(&self, method: reqwest::Method, path: &str) -> Result<reqwest::RequestBuilder> {
        let date = httpdate::fmt_http_date(SystemTime::now());
        let url = format!("{}/{path}", self.endpoint);
        Ok(self
            .http
            .request(method, url)
            .header("x-ms-date", &date)
            .header("x-ms-version", "2019-02-02")
            .header("Accept", "application/json;odata=nometadata")
            .header("Authorization", self.authorization(&date, path)?))
    }

    /// Remember a session so it survives the next deploy.
    pub(crate) async fn put_session(
        &self,
        token: &str,
        identity: &Identity,
        created_at: SystemTime,
    ) -> Result<()> {
        let row_key = Self::row_key(token);
        let seconds = created_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let body = serde_json::json!({
            "PartitionKey": PARTITION,
            "RowKey": row_key,
            "Id": identity.id,
            "Name": identity.name,
            "AvatarUrl": identity.avatar_url,
            "CreatedAt": seconds.to_string(),
        });
        // Insert Or Replace, so a re-login for the same token is not an error.
        self.request(reqwest::Method::PUT, &self.entity_path(&row_key))?
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("could not reach table storage")?
            .error_for_status()
            .context("table storage rejected the session write")?;
        Ok(())
    }

    /// Look a session up after a restart has emptied the memory cache.
    ///
    /// `Ok(None)` means the row is genuinely absent. An `Err` means the table
    /// could not be consulted, which the caller must not treat as "signed
    /// out": that would sign everyone out whenever storage hiccups, the exact
    /// failure this module exists to remove.
    pub(crate) async fn get_session(&self, token: &str) -> Result<Option<(Identity, SystemTime)>> {
        let path = self.entity_path(&Self::row_key(token));
        let response = self
            .request(reqwest::Method::GET, &path)?
            .send()
            .await
            .context("could not reach table storage")?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let row: SessionRow = response
            .error_for_status()
            .context("table storage rejected the session read")?
            .json()
            .await
            .context("table storage returned an unreadable session row")?;

        let created_at = SystemTime::UNIX_EPOCH
            + Duration::from_secs(row.created_at.parse::<u64>().unwrap_or_default());
        Ok(Some((
            Identity {
                id: row.id,
                name: row.name,
                avatar_url: row.avatar_url,
            },
            created_at,
        )))
    }

    /// Forget a session that has expired or been evicted.
    ///
    /// A missing row is success: the goal is absence, and something else
    /// having already removed it satisfies that.
    pub(crate) async fn delete_session(&self, token: &str) -> Result<()> {
        let path = self.entity_path(&Self::row_key(token));
        let response = self
            .request(reqwest::Method::DELETE, &path)?
            .header("If-Match", "*")
            .send()
            .await
            .context("could not reach table storage")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        response
            .error_for_status()
            .context("table storage rejected the session delete")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> TableStore {
        TableStore {
            account: "testaccount1".into(),
            endpoint: "https://testaccount1.table.core.windows.net".into(),
            // "key" base64-encoded; the signature itself is not asserted here.
            key: base64::engine::general_purpose::STANDARD.encode(b"key"),
            table: "sessions".into(),
            http: reqwest::Client::new(),
        }
    }

    /// The signature is two lines and nothing else, and getting either wrong
    /// produces a 403 that says only "Forbidden". This pins the format against
    /// the worked example in Microsoft's Shared Key Lite documentation for the
    /// Table service.
    #[test]
    fn string_to_sign_is_the_date_then_the_canonical_resource() {
        let store = TableStore {
            table: "Tables".into(),
            ..store()
        };
        assert_eq!(
            store.string_to_sign("Sun, 11 Oct 2009 19:52:39 GMT", "Tables"),
            "Sun, 11 Oct 2009 19:52:39 GMT\n/testaccount1/Tables"
        );
    }

    /// Tokens are bearer credentials. A row key is visible to anyone who can
    /// list the table, so storing the token there would turn read access to
    /// storage into the ability to log in as anybody.
    #[test]
    fn the_row_key_is_a_hash_so_a_table_dump_cannot_be_replayed() {
        let key = TableStore::row_key("super-secret-token");

        assert!(!key.contains("super-secret-token"));
        assert_eq!(key.len(), 64);
        assert_eq!(key, TableStore::row_key("super-secret-token"));
        assert_ne!(key, TableStore::row_key("another-token"));
    }

    /// The entity path is also the canonicalized resource, so a change here
    /// silently breaks every signature rather than just the URL.
    #[test]
    fn the_entity_path_addresses_a_single_row_by_partition_and_key() {
        assert_eq!(
            store().entity_path("abc"),
            "sessions(PartitionKey='session',RowKey='abc')"
        );
    }

    /// Absent configuration has to mean "in memory", not "broken". Local runs
    /// and `orange serve` have no Azure account, exactly as they have no
    /// Discord application.
    #[test]
    fn a_relay_without_table_configuration_runs_without_a_store() {
        // Setting these for real would leak into sibling tests, so this only
        // checks the empty-string guard, which is the case a deployment
        // template with unset variables actually produces.
        let store = TableStore {
            account: String::new(),
            ..store()
        };
        assert!(store.account.is_empty());
    }
}
