//! Elasticsearch connector via REST API.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult, QueryRunner, TableSchema,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EsOptions {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub use_https: bool,
    #[serde(default)]
    pub index: String,
}

const fn default_port() -> u16 {
    9200
}

pub struct ElasticsearchConnector;

fn parse_options(opts: &Value) -> Result<EsOptions> {
    super::parse_opts(opts, "invalid Elasticsearch options")
}

/// Validate an index (or comma-separated index list) before it is spliced verbatim into a
/// request path. The allowed charset has no URL-reserved character (no `/`, `?`, `#`, `%`,
/// whitespace), so nothing here can redirect the request to a different path or query string;
/// rejecting `.`/`..` elements closes the one remaining path-normalization vector once `/`
/// itself is excluded.
fn validate_index(index: &str) -> Result<()> {
    if index.len() > 255 {
        anyhow::bail!("Elasticsearch index name exceeds 255 characters");
    }
    if !index
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | ',' | '*'))
    {
        anyhow::bail!("Elasticsearch index name contains disallowed characters: {index:?}");
    }
    for part in index.split(',') {
        if part.is_empty() || part == "." || part == ".." {
            anyhow::bail!("Elasticsearch index name contains an invalid path segment: {index:?}");
        }
    }
    Ok(())
}

/// `GET`s `path`, or `POST`s it with `body` as JSON, with basic auth when a user is configured.
async fn request(opts: &EsOptions, path: &str, body: Option<Value>) -> Result<Value> {
    let addrs = crate::net_guard::guard_host(&opts.host).await?;
    let client = super::http_connectors::pinned_http_client(&opts.host, &addrs)?;
    let url = format!(
        "{}{}",
        super::http_base_url(opts.use_https, &opts.host, opts.port),
        path
    );
    let mut req = body
        .as_ref()
        .map_or_else(
            || client.get(&url),
            |body| {
                client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .json(body)
            },
        )
        .timeout(Duration::from_secs(30));
    if !opts.user.is_empty() {
        req = req.basic_auth(&opts.user, Some(&opts.password));
    }
    let resp = req
        .send()
        .await
        .map_err(super::http_connectors::classify_reqwest_err)
        .context("ES request failed")?;
    let status = resp.status();
    let resp_body = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("Elasticsearch returned {status}: {resp_body}");
    }
    serde_json::from_str(&resp_body).context("ES response parse failed")
}

/// Appends every typed field under `props` as `prefix.field`, recursing into nested `properties`.
fn flatten_mapping(prefix: &str, props: &Value, out: &mut Vec<ColumnSchema>) {
    let Some(obj) = props.as_object() else {
        return;
    };
    for (k, v) in obj {
        let full_key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        if let Some(t) = v.get("type").and_then(Value::as_str) {
            out.push(ColumnSchema::typed(full_key.clone(), t));
        }
        if let Some(nested) = v.get("properties") {
            flatten_mapping(&full_key, nested, out);
        }
    }
}

#[async_trait]
impl QueryRunner for ElasticsearchConnector {
    fn type_name(&self) -> &'static str {
        "es"
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "title": "Host" },
                "port": { "type": "integer", "title": "Port", "default": 9200 },
                "user": { "type": "string", "title": "Username" },
                "password": { "type": "string", "title": "Password" },
                "use_https": { "type": "boolean", "title": "Use HTTPS", "default": false },
                "index": { "type": "string", "title": "Default Index (optional)" }
            },
            "required": ["host"],
            "secret": ["password"]
        })
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let opts = parse_options(opts)?;
        request(&opts, "/", None).await.map(|_| ())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let opts = parse_options(opts)?;
        let mapping = request(&opts, "/_all/_mapping", None).await?;
        let indices = mapping.as_object().into_iter().flatten();
        Ok(indices
            .map(|(index, val)| {
                let mut columns = Vec::new();
                if let Some(props) = val.pointer("/mappings/properties") {
                    flatten_mapping("", props, &mut columns);
                }
                TableSchema {
                    name: index.clone(),
                    columns,
                }
            })
            .collect())
    }

    fn network_targets(&self, opts: &Value) -> Vec<NetworkTarget> {
        parse_options(opts)
            .map(|opts| vec![NetworkTarget::Host(opts.host)])
            .unwrap_or_default()
    }

    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        fn source(hit: &Value) -> Option<&serde_json::Map<String, Value>> {
            hit.get("_source").and_then(Value::as_object)
        }

        let opts = parse_options(opts)?;
        // Query is expected to be JSON (ES query DSL) optionally prefixed with "index\n".
        let (index, query_body) = match query.split_once('\n') {
            Some((idx, body)) if !idx.trim().is_empty() => (idx.trim(), body),
            Some((_, body)) => (opts.index.as_str(), body),
            None => (opts.index.as_str(), query),
        };

        // Validated before the body is even parsed: an attacker-controlled index must never
        // reach the URL path regardless of whether the body happens to be well-formed JSON.
        if !index.is_empty() {
            validate_index(index)?;
        }

        let body: Value = serde_json::from_str(query_body.trim())
            .context("query must be valid JSON (Elasticsearch Query DSL)")?;

        let start = Instant::now();
        let path = if index.is_empty() {
            "/_search".to_string()
        } else {
            format!("/{index}/_search")
        };
        let result = request(&opts, &path, Some(body)).await?;
        let runtime_seconds = start.elapsed().as_secs_f64();

        if let Some(err) = result.get("error") {
            anyhow::bail!("Elasticsearch error: {err}");
        }

        let hits = result
            .pointer("/hits/hits")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();

        // Unique keys across all hits, in first-seen order. `_id` comes first (whenever there's at
        // least one hit) since every row below carries one, but it lives outside `_source` so the
        // `_source`-keys scan never finds it on its own.
        let source_keys = hits.iter().filter_map(source).flat_map(|src| src.keys());
        let mut seen = std::collections::HashSet::new();
        let col_names: Vec<String> = hits
            .first()
            .map(|_| "_id")
            .into_iter()
            .chain(source_keys.map(String::as_str))
            .filter(|k| seen.insert(*k))
            .map(String::from)
            .collect();

        let json_rows = hits
            .iter()
            .map(|hit| {
                let mut obj = serde_json::Map::new();
                obj.insert("_id".to_string(), hit["_id"].clone());
                if let Some(src) = source(hit) {
                    obj.extend(src.clone());
                }
                Value::Object(obj)
            })
            .collect();

        Ok(QueryExecutionResult {
            columns: ColumnMeta::all_string(&col_names),
            rows: json_rows,
            runtime_seconds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::validate_index;

    #[test]
    fn validate_index_accepts_legitimate_names() {
        for ok in ["conn_test", "logs-2026.01.*", "a,b", ".kibana", "a..b"] {
            assert!(validate_index(ok).is_ok(), "expected {ok:?} to be accepted");
        }
    }

    #[test]
    fn validate_index_rejects_path_and_query_injection() {
        for bad in [
            "x/../_bulk?refresh#",
            "a/b",
            "..",
            ".",
            "a?x",
            "a#x",
            "a%2Fb",
            "a b",
            "a:b",
            "a,,b",
            "a,..,b",
        ] {
            assert!(
                validate_index(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        assert!(validate_index(&"a".repeat(256)).is_err());
    }
}
