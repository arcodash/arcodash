//! Thin reqwest-based client for the GCS JSON/XML upload API (not the gRPC `google-cloud-storage`
//! SDK).
//!
//! Talks to real GCS (metadata-server token auth) or a local emulator (`STORAGE_EMULATOR_HOST`, no
//! auth) via the same code path.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::connectors::http_connectors::percent_encode;

const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";
/// Refresh this long before expiry so an in-flight request never races the token going stale.
const TOKEN_REFRESH_SKEW: Duration = Duration::from_secs(60);
const ENSURE_BUCKET_RETRIES: u32 = 10;
const ENSURE_BUCKET_BACKOFF: Duration = Duration::from_millis(500);
/// GCS returns up to 1000 objects per page, so this bounds `list_prefix` to a million objects;
/// guards against a server that never ends pagination and would OOM `out`.
const MAX_LIST_PAGES: usize = 1000;

enum StorageBase {
    /// `fake-gcs-server` (or similar) base URL, e.g. `http://fake-gcs:4443`; sends no auth.
    Emulator(String),
    Gcp,
}

pub struct ObjectStore {
    http: reqwest::Client,
    base: StorageBase,
    bucket: String,
    token_cache: RwLock<Option<(String, Instant)>>,
}

#[async_trait::async_trait]
impl crate::org_storage::BlobStore for ObjectStore {
    async fn put(
        &self,
        key: &str,
        bytes: Bytes,
        content_type: &str,
    ) -> Result<(), crate::org_storage::StorageError> {
        Self::put(self, key, bytes, content_type).await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Bytes, crate::org_storage::StorageError> {
        self.get_opt(key)
            .await?
            .ok_or(crate::org_storage::StorageError::NotFound)
    }

    async fn delete(&self, key: &str) -> Result<(), crate::org_storage::StorageError> {
        Self::delete(self, key).await?;
        Ok(())
    }

    async fn list_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, u64)>, crate::org_storage::StorageError> {
        Ok(Self::list_prefix(self, prefix)
            .await?
            .into_iter()
            .map(|o| (o.name, o.size))
            .collect())
    }
}

/// One object returned by `ObjectStore::list_prefix`.
pub struct StoredObject {
    pub name: String,
    pub size: u64,
}

/// GCS's list response: `items` is absent (not `[]`) when nothing matches, and each `size` is a
/// decimal string.
#[derive(serde::Deserialize)]
struct GcsListResponse {
    #[serde(default)]
    items: Option<Vec<GcsListItem>>,
    #[serde(rename = "nextPageToken", default)]
    next_page_token: Option<String>,
}

#[derive(serde::Deserialize)]
struct GcsListItem {
    name: String,
    #[serde(default)]
    size: Option<String>,
}

/// Split out of `list_prefix` so the URL (including percent-encoding of a `/`-containing prefix) is
/// testable offline.
fn list_prefix_url(
    json_base: &str,
    bucket: &str,
    prefix: &str,
    page_token: Option<&str>,
) -> String {
    let mut url = format!(
        "{json_base}/storage/v1/b/{}/o?prefix={}",
        percent_encode(bucket),
        percent_encode(prefix)
    );
    if let Some(token) = page_token {
        url.push_str("&pageToken=");
        url.push_str(&percent_encode(token));
    }
    url
}

/// Decides `list_prefix`'s next step from one page's response: `Ok(Some(token))` continues,
/// `Ok(None)` is the last page, `Err` means the server echoed the same token back. A hard error,
/// not a silent stop, because a truncated list would make the org-deletion GCS purge under-delete.
fn next_page_step(
    prefix: &str,
    previous_token: Option<&str>,
    next_page_token: Option<String>,
) -> Result<Option<String>> {
    match next_page_token {
        Some(token) if !token.is_empty() => {
            if previous_token == Some(token.as_str()) {
                anyhow::bail!(
                    "GCS object list for prefix '{prefix}' returned the same page token twice \
                     ('{token}') — refusing to loop forever"
                );
            }
            Ok(Some(token))
        }
        _ => Ok(None),
    }
}

impl ObjectStore {
    /// Builds its own `reqwest::Client`. Reads `UPLOAD_BUCKET` (required) and
    /// `STORAGE_EMULATOR_HOST` (emulator mode when non-empty).
    pub fn from_env() -> Result<Arc<Self>> {
        let bucket =
            std::env::var("UPLOAD_BUCKET").context("UPLOAD_BUCKET is required but was not set")?;
        let base = match std::env::var("STORAGE_EMULATOR_HOST") {
            Ok(host) if !host.trim().is_empty() => {
                StorageBase::Emulator(host.trim_end_matches('/').to_string())
            }
            _ => StorageBase::Gcp,
        };
        Ok(Arc::new(Self {
            http: build_http_client()?,
            base,
            bucket,
            token_cache: RwLock::new(None),
        }))
    }

    /// Emulator-mode store for an explicit host and bucket, without reading the environment.
    ///
    /// # Errors
    ///
    /// Fails if the HTTP client cannot be built.
    pub fn for_emulator(host: &str, bucket: &str) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            http: build_http_client()?,
            base: StorageBase::Emulator(host.trim_end_matches('/').to_string()),
            bucket: bucket.to_string(),
            token_cache: RwLock::new(None),
        }))
    }

    fn json_base(&self) -> &str {
        match &self.base {
            StorageBase::Emulator(host) => host,
            StorageBase::Gcp => "https://storage.googleapis.com",
        }
    }

    fn object_url(&self, key: &str) -> String {
        format!(
            "{}/storage/v1/b/{}/o/{}",
            self.json_base(),
            percent_encode(&self.bucket),
            percent_encode(key)
        )
    }

    /// Adds the bearer token when talking to real GCS.
    async fn authed(&self, req: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        Ok(match self.access_token().await? {
            Some(token) => req.bearer_auth(token),
            None => req,
        })
    }

    /// `None` in emulator mode; `Some(token)` against real GCS, refreshed from the metadata server
    /// shortly before expiry.
    async fn access_token(&self) -> Result<Option<String>> {
        #[derive(serde::Deserialize)]
        struct TokenResponse {
            access_token: String,
            expires_in: u64,
        }

        if matches!(self.base, StorageBase::Emulator(_)) {
            return Ok(None);
        }

        if let Some(token) = self.cached_token().await {
            return Ok(Some(token));
        }

        // No lock is held across the metadata-server round trip. Two racing cache misses may both
        // fetch a token; accepted, as that costs less than serializing all object-store I/O.

        // Metadata-server ADC only (Cloud Run/GKE); add a GOOGLE_APPLICATION_CREDENTIALS
        // JWT exchange if non-GCP deploys need it.
        let resp: TokenResponse = self
            .http
            .get(METADATA_TOKEN_URL)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .context("failed to reach GCP metadata server for an access token")
            .and_then(|r| {
                r.error_for_status()
                    .context("GCP metadata server returned an error status")
            })
            .inspect_err(
                |e| tracing::warn!(error = %e, "metadata-server access token fetch failed"),
            )?
            .json()
            .await
            .context("failed to parse GCP metadata server token response")?;

        let ttl = Duration::from_secs(resp.expires_in).saturating_sub(TOKEN_REFRESH_SKEW);
        let expiry = Instant::now() + ttl;
        *self.token_cache.write().await = Some((resp.access_token.clone(), expiry));
        Ok(Some(resp.access_token))
    }

    async fn cached_token(&self) -> Option<String> {
        let cache = self.token_cache.read().await;
        cache
            .as_ref()
            .filter(|(_, expiry)| Instant::now() < *expiry)
            .map(|(token, _)| token.clone())
    }

    pub async fn put(&self, key: &str, bytes: Bytes, content_type: &str) -> Result<()> {
        tracing::debug!(object_key = key, bytes = bytes.len(), "storage put");
        // Real GCS documents a separate upload endpoint; fake-gcs-server serves it off the same host.
        let url = format!(
            "{}/upload/storage/v1/b/{}/o?uploadType=media&name={}",
            self.json_base(),
            percent_encode(&self.bucket),
            percent_encode(key)
        );
        let req = self
            .http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(bytes);
        self.authed(req)
            .await?
            .send()
            .await
            .context("GCS object upload request failed")?
            .error_for_status()
            .context("GCS object upload returned an error status")?;
        Ok(())
    }

    pub async fn get(&self, key: &str) -> Result<Bytes> {
        self.get_opt(key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("GCS object download returned status 404 Not Found"))
    }

    /// `None` when the object does not exist.
    async fn get_opt(&self, key: &str) -> Result<Option<Bytes>> {
        tracing::debug!(object_key = key, "storage get");
        let url = format!("{}?alt=media", self.object_url(key));
        let resp = self
            .authed(self.http.get(&url))
            .await?
            .send()
            .await
            .context("GCS object download request failed")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = resp
            .error_for_status()
            .context("GCS object download returned an error status")?;
        let bytes = resp
            .bytes()
            .await
            .context("failed to read GCS object download body")?;
        tracing::debug!(
            object_key = key,
            bytes = bytes.len(),
            "storage get complete"
        );
        Ok(Some(bytes))
    }

    /// A 404 counts as success: the object is already gone.
    pub async fn delete(&self, key: &str) -> Result<()> {
        tracing::info!(object_key = key, "storage delete");
        let resp = self
            .authed(self.http.delete(self.object_url(key)))
            .await?
            .send()
            .await
            .context("GCS object delete request failed")?;
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            tracing::warn!(object_key = key, status = %resp.status(), "storage delete returned a non-success status");
            anyhow::bail!("GCS object delete returned status {}", resp.status())
        }
    }

    /// Every object GCS returns for `prefix`, enough to sum counts/bytes and feed each `name` to
    /// `delete`.
    pub async fn list_prefix(&self, prefix: &str) -> Result<Vec<StoredObject>> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        for page in 1..=MAX_LIST_PAGES {
            let url = list_prefix_url(
                self.json_base(),
                &self.bucket,
                prefix,
                page_token.as_deref(),
            );
            let resp: GcsListResponse = self
                .authed(self.http.get(&url))
                .await?
                .send()
                .await
                .context("GCS object list request failed")?
                .error_for_status()
                .context("GCS object list returned an error status")?
                .json()
                .await
                .context("failed to parse GCS object list response")?;

            out.extend(resp.items.unwrap_or_default().into_iter().map(|item| {
                let size = item.size.and_then(|s| s.parse().ok()).unwrap_or(0);
                StoredObject {
                    name: item.name,
                    size,
                }
            }));

            if let Some(token) =
                next_page_step(prefix, page_token.as_deref(), resp.next_page_token)?
            {
                page_token = Some(token);
            } else {
                tracing::info!(
                    prefix,
                    pages = page,
                    objects = out.len(),
                    "storage list_prefix complete"
                );
                return Ok(out);
            }
        }
        anyhow::bail!(
            "GCS object list for prefix '{prefix}' exceeded {MAX_LIST_PAGES} pages without \
             finishing — refusing to keep paginating"
        )
    }

    /// Creates the configured bucket if missing (409 is success), retrying while the emulator
    /// boots. Call at startup only when `STORAGE_EMULATOR_HOST` is set.
    pub async fn ensure_bucket(&self) -> Result<()> {
        let url = format!("{}/storage/v1/b?project=arcodash", self.json_base());
        let body = serde_json::json!({ "name": self.bucket });

        let mut last_err = None;
        for _ in 0..ENSURE_BUCKET_RETRIES {
            match self.http.post(&url).json(&body).send().await {
                Ok(resp)
                    if resp.status().is_success()
                        || resp.status() == reqwest::StatusCode::CONFLICT =>
                {
                    return Ok(());
                }
                Ok(resp) => {
                    last_err = Some(anyhow::anyhow!(
                        "ensure_bucket: unexpected status {}",
                        resp.status()
                    ));
                }
                Err(e) => last_err = Some(anyhow::anyhow!(e)),
            }
            tokio::time::sleep(ENSURE_BUCKET_BACKOFF).await;
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("ensure_bucket: exhausted retries")))
    }
}

/// Makes an uploaded filename safe as an object key segment: only `[A-Za-z0-9._-]` survive (so no
/// `/` or `\` traversal), leading dots are stripped, capped at 128 bytes; falls back to `"file"`.
#[must_use]
pub fn sanitize_filename(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let capped: String = mapped.trim_start_matches('.').chars().take(128).collect();
    if capped.is_empty() {
        "file".to_string()
    } else {
        capped
    }
}

/// Object key for an uploaded file, including the connector type so uploads never collide on
/// `upload_id` alone.
#[must_use]
pub fn object_key(org_id: i64, connector_type: &str, upload_id: Uuid, filename: &str) -> String {
    format!(
        "orgs/{org_id}/uploads/{connector_type}/{upload_id}/{}",
        sanitize_filename(filename)
    )
}

/// 60s total timeout covers `put` too (the 25MB upload cap needs ~440KB/s). No redirects: GCS's
/// API doesn't 30x normally.
fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("arcodash/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build object-store HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_strips_path_traversal() {
        // No leading dots or path separators survive, which is the traversal risk; a literal ".."
        // elsewhere is inert in GCS keys.
        let s = sanitize_filename("../../etc/passwd");
        assert!(!s.contains('/'));
        assert!(!s.contains('\\'));
        assert!(!s.starts_with('.'));
    }

    #[test]
    fn sanitize_filename_empty_falls_back() {
        assert_eq!(sanitize_filename(""), "file");
    }

    #[test]
    fn sanitize_filename_replaces_unicode_and_spaces() {
        let s = sanitize_filename("héllo world.csv");
        assert_eq!(s, "h_llo_world.csv");
    }

    #[test]
    fn sanitize_filename_strips_leading_dots() {
        assert_eq!(sanitize_filename("...secret"), "secret");
    }

    #[test]
    fn sanitize_filename_caps_length() {
        let long = "a".repeat(200);
        assert_eq!(sanitize_filename(&long).len(), 128);
    }

    #[test]
    fn object_key_includes_connector_type_and_sanitized_name() {
        let id = Uuid::nil();
        let key = object_key(42, "csv", id, "../evil.csv");
        assert_eq!(key, format!("orgs/42/uploads/csv/{id}/_evil.csv"));
    }

    #[test]
    fn object_key_shape_for_sqlite() {
        let id = Uuid::nil();
        let key = object_key(7, "sqlite", id, "data.sqlite");
        assert_eq!(key, format!("orgs/7/uploads/sqlite/{id}/data.sqlite"));
    }

    #[test]
    fn list_prefix_url_percent_encodes_slashes_in_prefix() {
        let url = list_prefix_url(
            "https://storage.googleapis.com",
            "my-bucket",
            "orgs/42/",
            None,
        );
        assert_eq!(
            url,
            "https://storage.googleapis.com/storage/v1/b/my-bucket/o?prefix=orgs%2F42%2F"
        );

        let paged = list_prefix_url(
            "https://storage.googleapis.com",
            "my-bucket",
            "orgs/42/",
            Some("next-page"),
        );
        assert!(paged.ends_with("&pageToken=next-page"));
    }

    #[test]
    fn next_page_step_continues_on_a_fresh_token() {
        assert_eq!(
            next_page_step("orgs/1/", None, Some("page2".to_string())).unwrap(),
            Some("page2".to_string())
        );
        assert_eq!(
            next_page_step("orgs/1/", Some("page2"), Some("page3".to_string())).unwrap(),
            Some("page3".to_string())
        );
    }

    #[test]
    fn next_page_step_stops_on_no_token_or_empty_token() {
        assert_eq!(
            next_page_step("orgs/1/", Some("page2"), None).unwrap(),
            None
        );
        assert_eq!(
            next_page_step("orgs/1/", Some("page2"), Some(String::new())).unwrap(),
            None
        );
    }

    #[test]
    fn next_page_step_errors_when_the_server_echoes_the_same_token_back() {
        let err = next_page_step("orgs/1/", Some("stuck"), Some("stuck".to_string()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("same page token twice"), "got: {err}");
        assert!(
            err.contains("orgs/1/"),
            "error should name the prefix: {err}"
        );
    }

    #[test]
    fn max_list_pages_is_the_documented_bound() {
        assert_eq!(MAX_LIST_PAGES, 1000);
    }

    /// The multi-page happy path, served by a local raw-HTTP server standing in for GCS's list API.
    #[tokio::test]
    async fn list_prefix_aggregates_objects_across_multiple_pages() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local listener");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let pages = [
                r#"{"items":[{"name":"orgs/1/a","size":"10"}],"nextPageToken":"page2"}"#,
                r#"{"items":[{"name":"orgs/1/b","size":"20"}]}"#,
            ];
            for body in pages {
                let (mut socket, _) = listener.accept().await.expect("accept connection");
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        let store = ObjectStore {
            http: reqwest::Client::new(),
            base: StorageBase::Emulator(format!("http://{addr}")),
            bucket: "test-bucket".to_string(),
            token_cache: RwLock::new(None),
        };

        let objects = store
            .list_prefix("orgs/1/")
            .await
            .expect("list_prefix must aggregate every page");
        let names: Vec<&str> = objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, vec!["orgs/1/a", "orgs/1/b"]);
        assert_eq!(objects[0].size, 10);
        assert_eq!(objects[1].size, 20);
    }

    /// `access_token` must drop the cache lock before the metadata-server call, so concurrent cache
    /// misses never serialize behind one another's I/O. The calls fail fast off-GCP; the assertion
    /// is that they complete promptly and independently.
    #[tokio::test(flavor = "multi_thread")]
    async fn access_token_concurrent_calls_do_not_deadlock() {
        let store = Arc::new(ObjectStore {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            base: StorageBase::Gcp,
            bucket: "unused".to_string(),
            token_cache: RwLock::new(None),
        });
        let a = store.clone();
        let b = store.clone();
        let result = tokio::time::timeout(Duration::from_secs(10), async move {
            tokio::join!(a.access_token(), b.access_token())
        })
        .await;
        assert!(
            result.is_ok(),
            "two concurrent cache-miss access_token() calls must not hang"
        );
    }

    #[test]
    fn gcs_list_response_deserializes_string_size_and_absent_items() {
        // `items` absent entirely -- an empty prefix match, not `"items": []`.
        let empty: GcsListResponse = serde_json::from_str(r#"{"kind":"storage#objects"}"#).unwrap();
        assert!(empty.items.is_none());
        assert!(empty.next_page_token.is_none());

        let body = r#"{
            "items": [{"name": "orgs/42/uploads/csv/a/f.csv", "size": "12345"}],
            "nextPageToken": "abc"
        }"#;
        let resp: GcsListResponse = serde_json::from_str(body).unwrap();
        let items = resp.items.expect("items must deserialize");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "orgs/42/uploads/csv/a/f.csv");
        assert_eq!(items[0].size.as_deref(), Some("12345"));
        assert_eq!(resp.next_page_token.as_deref(), Some("abc"));
    }
}
