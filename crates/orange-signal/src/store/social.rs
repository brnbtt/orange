//! Social entities reuse the configured session table under distinct partitions.
//! One canonical relationship row and conditional writes avoid a two-user
//! transaction that could leave one roster updated and the other unchanged.

use super::*;
use crate::social::Relationship;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};

impl TableStore {
    fn social_path(&self, partition: &str, key: &str) -> String {
        format!("{}(PartitionKey='{partition}',RowKey='{key}')", self.table)
    }

    async fn social_entity(&self, partition: &str, key: &str) -> Result<Option<(Value, String)>> {
        let response = self
            .request(Method::GET, &self.social_path(partition, key))?
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response.error_for_status()?;
        let etag = response
            .headers()
            .get("etag")
            .context("table response has no ETag")?
            .to_str()?
            .to_string();
        Ok(Some((response.json().await?, etag)))
    }

    pub(crate) async fn put_profile(&self, identity: &Identity) -> Result<()> {
        self.request(Method::PUT, &self.social_path("profile", &identity.id))?
            .json(&json!({"PartitionKey":"profile", "RowKey":identity.id, "Data":serde_json::to_string(identity)?}))
            .send().await?.error_for_status()?;
        Ok(())
    }

    pub(crate) async fn get_profile(&self, id: &str) -> Result<Option<Identity>> {
        self.social_entity("profile", id)
            .await?
            .map(|(body, _)| decode(&body))
            .transpose()
    }

    pub(crate) async fn get_relationship(
        &self,
        key: &str,
    ) -> Result<Option<(Relationship, String)>> {
        self.social_entity("friendship", key)
            .await?
            .map(|(body, etag)| Ok((decode(&body)?, etag)))
            .transpose()
    }

    pub(crate) async fn save_relationship(
        &self,
        key: &str,
        relationship: &Relationship,
        etag: Option<&str>,
    ) -> Result<bool> {
        let body = json!({"PartitionKey":"friendship", "RowKey":key,
            "SenderId":relationship.sender.id, "RecipientId":relationship.recipient.id,
            "Data":serde_json::to_string(relationship)?});
        let request = match etag {
            Some(etag) => self
                .request(Method::PUT, &self.social_path("friendship", key))?
                .header("If-Match", etag),
            None => self.request(Method::POST, &self.table)?,
        };
        let response = request.json(&body).send().await?;
        if matches!(
            response.status(),
            StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED
        ) {
            return Ok(false);
        }
        response.error_for_status()?;
        Ok(true)
    }

    pub(crate) async fn relationships(&self, id: &str) -> Result<Vec<Relationship>> {
        // ponytail: filtered scans of one partition suit the current small
        // relay. Add per-account indexes if measured query latency warrants it.
        // Never return a partial inbox at an Azure continuation boundary.
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut result = Vec::new();
            let mut continuation = Vec::<(String, String)>::new();
            for _ in 0..32 {
                let response = self.request(Method::GET, &format!("{}()", self.table))?
                    .query(&[("$filter", format!("PartitionKey eq 'friendship' and (SenderId eq '{id}' or RecipientId eq '{id}')")), ("$top", "256".into())])
                    .query(&continuation).send().await?.error_for_status()?;
                continuation.clear();
                for (header, parameter) in [("x-ms-continuation-NextPartitionKey", "NextPartitionKey"), ("x-ms-continuation-NextRowKey", "NextRowKey")] {
                    if let Some(value) = response.headers().get(header) {
                        continuation.push((parameter.into(), value.to_str()?.to_string()));
                    }
                }
                let body: Value = response.json().await?;
                for row in body["value"].as_array().context("table query has no entity list")? {
                    result.push(decode(row)?);
                }
                anyhow::ensure!(result.len() <= 4096, "friend history exceeds query limit");
                if continuation.is_empty() { return Ok(result); }
            }
            anyhow::bail!("friend query exceeded continuation limit")
        }).await.context("friend query timed out")?
    }
}

fn decode<T: serde::de::DeserializeOwned>(body: &Value) -> Result<T> {
    serde_json::from_str(
        body["Data"]
            .as_str()
            .context("table entity has no social data")?,
    )
    .context("invalid social entity")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        extract::{Query, State},
        http::Request,
        response::{IntoResponse, Response},
        Json, Router,
    };
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };
    use tokio::sync::Mutex;

    #[derive(Clone, Default)]
    struct Table {
        rows: Arc<Mutex<HashMap<String, (Value, u64)>>>,
        fail_writes: Arc<AtomicBool>,
        fail_reads: Arc<AtomicBool>,
        conflict_once: Arc<AtomicBool>,
    }

    async fn table(
        State(table): State<Table>,
        Query(query): Query<HashMap<String, String>>,
        request: Request<Body>,
    ) -> Response {
        let path = request.uri().path().to_string();
        let method = request.method().clone();
        if method == Method::GET && table.fail_reads.load(Ordering::Relaxed) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let mut rows = table.rows.lock().await;
        if method == Method::GET && path.ends_with("()") {
            let filter = &query["$filter"];
            let id = filter
                .split("SenderId eq '")
                .nth(1)
                .unwrap()
                .split('\'')
                .next()
                .unwrap();
            let mut values: Vec<_> = rows
                .values()
                .map(|(row, _)| row.clone())
                .filter(|row| row["SenderId"] == id || row["RecipientId"] == id)
                .collect();
            values.sort_by_key(|row| row["RowKey"].as_str().unwrap().to_string());
            let offset = query
                .get("NextRowKey")
                .map(|value| value.parse::<usize>().unwrap())
                .unwrap_or(0);
            let more = offset + 1 < values.len();
            let mut response =
                Json(json!({"value": values.into_iter().skip(offset).take(1).collect::<Vec<_>>()}))
                    .into_response();
            if more {
                response.headers_mut().insert(
                    "x-ms-continuation-NextPartitionKey",
                    "friendship".parse().unwrap(),
                );
                response.headers_mut().insert(
                    "x-ms-continuation-NextRowKey",
                    (offset + 1).to_string().parse().unwrap(),
                );
            }
            return response;
        }
        if method == Method::GET {
            return match rows.get(&path) {
                Some((row, version)) => {
                    ([("etag", format!("\"{version}\""))], Json(row.clone())).into_response()
                }
                None => StatusCode::NOT_FOUND.into_response(),
            };
        }
        if table.fail_writes.load(Ordering::Relaxed) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let etag = request
            .headers()
            .get("If-Match")
            .map(|v| v.to_str().unwrap().to_string());
        if etag.is_some() && table.conflict_once.swap(false, Ordering::Relaxed) {
            return StatusCode::PRECONDITION_FAILED.into_response();
        }
        let body: Value =
            serde_json::from_slice(&to_bytes(request.into_body(), 16384).await.unwrap()).unwrap();
        let key = format!(
            "/sessions(PartitionKey='{}',RowKey='{}')",
            body["PartitionKey"].as_str().unwrap(),
            body["RowKey"].as_str().unwrap()
        );
        let version = rows.get(&key).map(|(_, version)| *version);
        if method == Method::POST && version.is_some() {
            return StatusCode::CONFLICT.into_response();
        }
        if let Some(etag) = etag {
            if version.map(|version| format!("\"{version}\"")) != Some(etag) {
                return StatusCode::PRECONDITION_FAILED.into_response();
            }
        }
        rows.insert(key, (body, version.unwrap_or(0) + 1));
        StatusCode::NO_CONTENT.into_response()
    }

    #[tokio::test]
    async fn friendships_survive_service_restart_and_table_failures_never_create_half_a_friendship()
    {
        // Exercise the actual REST serializer, continuation headers and ETags
        // against a local HTTP table, rather than just an in-memory friend map.
        let state = Table::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(table).with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let store = TableStore {
            account: "test".into(),
            endpoint,
            key: "a2V5".into(),
            table: "sessions".into(),
            http: reqwest::Client::new(),
        };
        // An existing durable login can register the social profile without
        // another Discord OAuth flow. A cold-cache storage outage is 503, not
        // the 401 that instructs the desktop to delete its session file.
        use tower::ServiceExt;
        let user = Identity {
            id: "9".into(),
            name: "Existing login".into(),
            avatar_url: None,
        };
        store
            .put_session("valid-login", &user, SystemTime::now())
            .await
            .unwrap();
        let auth = crate::auth::Auth::with_store(None, Some(store.clone()));
        let app = crate::server::router(crate::server::AppState {
            rooms: Default::default(),
            auth,
            social: crate::social::Social::new(Some(store.clone())),
        });
        state.fail_reads.store(true, Ordering::Relaxed);
        let response = app
            .clone()
            .oneshot(
                Request::get("/friends")
                    .header("Authorization", "Bearer valid-login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        state.fail_reads.store(false, Ordering::Relaxed);
        let response = app
            .clone()
            .oneshot(
                Request::get("/friends")
                    .header("Authorization", "Bearer valid-login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app
            .oneshot(
                Request::get("/friends")
                    .header("Authorization", "Bearer expired-login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let profile = |id: &str| Identity {
            id: id.into(),
            name: format!("User {id}"),
            avatar_url: None,
        };
        let a = profile("1");
        let b = profile("2");
        let c = profile("3");
        let social = crate::social::Social::new(Some(store.clone()));
        for user in [&a, &b, &c] {
            social.register(user).await.unwrap();
        }
        let change = |action, target: &str, revision: Option<String>| crate::social::Change {
            action,
            target_id: target.into(),
            revision,
        };
        social
            .change(&a, &change(crate::social::Action::Request, "2", None))
            .await
            .unwrap();
        social
            .change(&a, &change(crate::social::Action::Request, "3", None))
            .await
            .unwrap();
        let restarted = crate::social::Social::new(Some(store));
        assert_eq!(
            restarted.snapshot(&a).await.unwrap().outgoing.len(),
            2,
            "lost a continuation page"
        );
        let revision = restarted.snapshot(&b).await.unwrap().incoming[0]
            .revision
            .clone();
        state.fail_writes.store(true, Ordering::Relaxed);
        assert!(restarted
            .change(
                &b,
                &change(crate::social::Action::Accept, "1", Some(revision.clone()))
            )
            .await
            .is_err());
        assert!(restarted.snapshot(&a).await.unwrap().friends.is_empty());
        assert!(restarted.snapshot(&b).await.unwrap().friends.is_empty());
        state.fail_writes.store(false, Ordering::Relaxed);
        state.conflict_once.store(true, Ordering::Relaxed);
        restarted
            .change(
                &b,
                &change(crate::social::Action::Accept, "1", Some(revision)),
            )
            .await
            .unwrap();
        assert_eq!(
            restarted.snapshot(&a).await.unwrap().friends[0].profile.id,
            "2"
        );
        assert_eq!(
            restarted.snapshot(&b).await.unwrap().friends[0].profile.id,
            "1"
        );
        server.abort();
    }
}
