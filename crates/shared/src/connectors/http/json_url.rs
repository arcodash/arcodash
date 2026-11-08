//! JSON URL connector: fetches an admin-configured URL and treats its body as row data.

use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::connectors::query_results::{
    json_to_sqlite_value, open_sqlite_conn, prepare_single, InterruptOnDrop,
};
use crate::connectors::sqlite_connector::sqlite_valueref_to_json;
use crate::connectors::{
    friendly_type_from_json_sample, ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult,
    QueryRunner, TableSchema,
};

use super::{guard_url, pinned_http_client, HTTP_TIMEOUT};

pub struct JsonConnector;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonOptions {
    pub url: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
}

/// Row cap for the JSON connector's response body, matching the value the MongoDB connector
/// (`mongodb_connector::MAX_ROWS`) uses — a byte cap on the response would be a tighter bound but
/// needs a streaming reqwest body; this row cap at least stops an unbounded array from being held
/// in memory in full.
const JSON_MAX_ROWS: usize = 10_000;

/// Strip credentials embedded in `opts.url` (`https://user:pass@host/...`) out of an error's
/// display text before it reaches `queries.last_execution_error`/the UI — reqwest's error
/// `Display` embeds the request URL verbatim, and this connector's URL is free-form
/// operator/user config, unlike the fixed vendor hosts other connectors talk to.
fn redact_json_error(err: impl std::fmt::Display, opts: &JsonOptions) -> anyhow::Error {
    let mut text = err.to_string();
    if let Ok(mut redacted) = reqwest::Url::parse(&opts.url) {
        if redacted.password().is_some() || !redacted.username().is_empty() {
            let _ = redacted.set_username("");
            let _ = redacted.set_password(Some("***"));
            text = text.replace(&opts.url, redacted.as_str());
        }
    }
    if !opts.password.is_empty() {
        text = text.replace(&opts.password, "***");
    }
    anyhow::anyhow!(text)
}

/// `GET` request for the admin-configured URL (never the query text, which would be SSRF), pinned
/// to the guard-approved addresses, with basic auth when a user is configured.
async fn fetch_request(opts: &JsonOptions) -> Result<reqwest::RequestBuilder> {
    let (host, addrs) = guard_url(&opts.url).await?;
    let req = pinned_http_client(&host, &addrs)?
        .get(&opts.url)
        .timeout(HTTP_TIMEOUT);
    Ok(if opts.user.is_empty() {
        req
    } else {
        req.basic_auth(&opts.user, Some(&opts.password))
    })
}

/// Sends `req`, failing on a transport error or non-2xx status with credentials redacted.
async fn send(req: reqwest::RequestBuilder, opts: &JsonOptions) -> Result<reqwest::Response> {
    req.send()
        .await
        .map_err(|e| redact_json_error(e, opts))
        .context("JSON URL request failed")?
        .error_for_status()
        .map_err(|e| redact_json_error(e, opts))
        .context("JSON URL returned error status")
}

#[async_trait]
impl QueryRunner for JsonConnector {
    fn type_name(&self) -> &'static str {
        "json"
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "title": "URL" },
                "user": { "type": "string", "title": "Username (optional)" },
                "password": { "type": "string", "title": "Password (optional)" }
            },
            "required": ["url"],
            "secret": ["password"]
        })
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let opts: JsonOptions = serde_json::from_value(opts.clone())?;
        send(fetch_request(&opts).await?, &opts).await?;
        Ok(())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let opts: JsonOptions = serde_json::from_value(opts.clone())?;
        let resp = send(fetch_request(&opts).await?, &opts).await?;
        let data: Value = resp.json().await.context("JSON parse failed")?;
        let first = match &data {
            Value::Array(arr) => arr.first(),
            obj => Some(obj),
        };
        let cols: Vec<ColumnSchema> = first
            .and_then(Value::as_object)
            .map(|m| {
                m.keys()
                    .map(|k| ColumnSchema::untyped(k.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let url = opts.url.rsplit('/').next().unwrap_or("json").to_string();
        Ok(vec![TableSchema {
            name: url,
            columns: cols,
        }])
    }

    fn network_targets(&self, opts: &Value) -> Vec<NetworkTarget> {
        serde_json::from_value::<JsonOptions>(opts.clone())
            .map(|opts| vec![NetworkTarget::Url(opts.url)])
            .unwrap_or_default()
    }

    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        let opts: JsonOptions = serde_json::from_value(opts.clone())?;
        // The query text never picks the URL; it runs below against the fetched data only, to
        // select/filter/project.
        let req = fetch_request(&opts).await?;
        let start = Instant::now();
        let resp = send(req, &opts).await?;
        let data: Value = resp.json().await.context("JSON parse failed")?;

        let rows: Vec<Value> = match data {
            Value::Array(arr) => arr,
            single => vec![single],
        };
        if rows.len() > JSON_MAX_ROWS {
            anyhow::bail!(
                "JSON response has more than {JSON_MAX_ROWS} elements; this connector caps \
                 result size to avoid unbounded memory use"
            );
        }

        let mut seen = std::collections::HashSet::new();
        let col_names: Vec<String> = rows
            .iter()
            .filter_map(Value::as_object)
            .flat_map(|obj| obj.keys())
            .filter(|k| seen.insert(*k))
            .cloned()
            .collect();

        let conn = open_sqlite_conn()?;
        // A dropped `spawn_blocking` handle only detaches its thread, so without this an abandoned
        // pathological query pins that thread forever.
        let _interrupt = InterruptOnDrop {
            handle: conn.get_interrupt_handle(),
            cancelled: std::sync::Arc::default(),
        };
        let query = query.to_string();
        let result =
            tokio::task::spawn_blocking(move || run_json_sql(conn, rows, col_names, &query))
                .await
                .context("JSON query task panicked")??;
        let runtime_seconds = start.elapsed().as_secs_f64();

        Ok(QueryExecutionResult {
            runtime_seconds,
            ..result
        })
    }
}

/// Loads `rows` into a single-table in-memory SQLite database (on the already-open `conn`, see
/// [`open_sqlite_conn`]) and runs `query` against it, so a JSON data source's SELECT actually
/// projects/filters/limits like any SQL connector instead of always returning every fetched row
/// and column. A row that isn't a JSON object (or carries none of `col_names`) contributes only
/// nulls for those columns — it still occupies a row so `COUNT(*)` stays accurate against what was
/// fetched.
#[allow(
    clippy::needless_pass_by_value,
    reason = "takes ownership of what the caller moves into its spawn_blocking task"
)]
fn run_json_sql(
    conn: rusqlite::Connection,
    rows: Vec<Value>,
    col_names: Vec<String>,
    query: &str,
) -> Result<QueryExecutionResult> {
    // A row shaped as a bare value (no object keys at all, e.g. a JSON array of scalars) has
    // nowhere to land among `col_names` — fall back to one `_value` column holding it verbatim so
    // such a source is still queryable, rather than refusing to create a table at all.
    let use_value_fallback = col_names.is_empty();
    let fallback = ["_value".to_string()];
    let table_col_names: &[String] = if use_value_fallback {
        &fallback
    } else {
        &col_names
    };
    let col_defs: Vec<String> = table_col_names
        .iter()
        .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
        .collect();
    conn.execute(&format!("CREATE TABLE data ({})", col_defs.join(", ")), [])
        .context("failed to create JSON data table")?;

    if !rows.is_empty() {
        let placeholders = vec!["?"; table_col_names.len()].join(", ");
        let insert_sql = format!("INSERT INTO data VALUES ({placeholders})");
        let mut stmt = conn
            .prepare(&insert_sql)
            .context("failed to prepare JSON insert")?;
        for row in &rows {
            let values: Vec<rusqlite::types::Value> = if use_value_fallback {
                vec![json_to_sqlite_value(row)]
            } else {
                col_names
                    .iter()
                    .map(|name| {
                        row.as_object()
                            .and_then(|obj| obj.get(name))
                            .map_or(rusqlite::types::Value::Null, json_to_sqlite_value)
                    })
                    .collect()
            };
            stmt.execute(rusqlite::params_from_iter(&values))
                .context("failed to insert JSON row")?;
        }
    }

    let mut stmt =
        prepare_single(&conn, query).map_err(|e| anyhow::anyhow!("invalid query: {e}"))?;
    let out_col_names: Vec<String> = stmt
        .column_names()
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    let row_iter = stmt
        .query_map([], |row| {
            let cells = out_col_names.iter().enumerate();
            cells
                .map(|(i, name)| Ok((name.clone(), sqlite_valueref_to_json(row.get_ref(i)?))))
                .collect::<rusqlite::Result<_>>()
                .map(Value::Object)
        })
        .map_err(|e| anyhow::anyhow!("invalid query: {e}"))?;

    let mut result_rows: Vec<Value> = Vec::new();
    for row in row_iter {
        let row = row.map_err(|e| anyhow::anyhow!("invalid query: {e}"))?;
        if result_rows.len() >= JSON_MAX_ROWS {
            anyhow::bail!(
                "JSON query result exceeds this connector's row cap ({JSON_MAX_ROWS} rows); \
                 narrow the query"
            );
        }
        result_rows.push(row);
    }

    let types = out_col_names.iter().map(|n| {
        let sample = result_rows.iter().find_map(|row| {
            row.as_object()
                .and_then(|obj| obj.get(n))
                .filter(|v| !v.is_null())
        });
        friendly_type_from_json_sample(sample)
    });
    let columns = ColumnMeta::zip_types(&out_col_names, types);

    Ok(QueryExecutionResult {
        columns,
        rows: result_rows,
        runtime_seconds: 0.0,
    })
}

#[cfg(test)]
mod json_redaction_tests {
    use super::{redact_json_error, JsonOptions};

    fn opts(url: &str, user: &str, password: &str) -> JsonOptions {
        JsonOptions {
            url: url.to_string(),
            user: user.to_string(),
            password: password.to_string(),
        }
    }

    #[test]
    fn strips_credentials_embedded_in_the_configured_url() {
        let opts = opts("https://user:secret@example.com/data.json", "", "");
        let err = redact_json_error(
            "error fetching https://user:secret@example.com/data.json: connection refused",
            &opts,
        );
        let text = err.to_string();
        assert!(
            !text.contains("secret"),
            "password leaked into error text: {text}"
        );
    }

    #[test]
    fn strips_a_separately_configured_password_too() {
        let opts = opts("https://example.com/data.json", "svc", "topsecret");
        let err = redact_json_error("401 unauthorized for user svc / topsecret", &opts);
        assert!(!err.to_string().contains("topsecret"));
    }

    #[test]
    fn leaves_a_credential_free_error_unchanged() {
        let opts = opts("https://example.com/data.json", "", "");
        let err = redact_json_error("connection refused", &opts);
        assert_eq!(err.to_string(), "connection refused");
    }
}

#[cfg(test)]
mod run_json_sql_tests {
    use super::{open_sqlite_conn, run_json_sql, InterruptOnDrop};
    use serde_json::json;
    use std::time::Duration;

    fn sample_rows(n: usize) -> Vec<serde_json::Value> {
        (0..n)
            .map(|i| json!({"id": i as i64, "name": format!("row-{i}")}))
            .collect()
    }

    #[test]
    fn limit_actually_narrows_the_row_count() {
        let rows = sample_rows(10);
        let col_names = vec!["id".to_string(), "name".to_string()];
        let conn = open_sqlite_conn().unwrap();
        let result = run_json_sql(conn, rows, col_names, "select id from data limit 3").unwrap();
        assert_eq!(result.rows.len(), 3);
    }

    #[test]
    fn column_projection_narrows_returned_columns() {
        let rows = sample_rows(5);
        let col_names = vec!["id".to_string(), "name".to_string()];
        let conn = open_sqlite_conn().unwrap();
        let result = run_json_sql(conn, rows, col_names, "select id from data").unwrap();
        assert_eq!(result.columns.len(), 1);
        assert_eq!(result.columns[0].name, "id");
        for row in &result.rows {
            let obj = row.as_object().unwrap();
            assert_eq!(obj.len(), 1);
            assert!(obj.contains_key("id"));
        }
    }

    #[test]
    fn friendly_type_is_inferred_for_the_projected_result_shape() {
        let rows = sample_rows(3);
        let col_names = vec!["id".to_string(), "name".to_string()];
        let conn = open_sqlite_conn().unwrap();
        let result = run_json_sql(conn, rows, col_names, "select id, name from data").unwrap();
        let by_name = |n: &str| {
            result
                .columns
                .iter()
                .find(|c| c.name == n)
                .unwrap()
                .friendly_type
                .clone()
        };
        assert_eq!(by_name("id"), "integer");
        assert_eq!(by_name("name"), "string");
    }

    #[test]
    fn malformed_user_sql_is_a_real_error_not_a_panic() {
        let rows = sample_rows(2);
        let col_names = vec!["id".to_string(), "name".to_string()];
        let conn = open_sqlite_conn().unwrap();
        let err = run_json_sql(conn, rows, col_names, "select this is not sql").unwrap_err();
        assert!(err.to_string().contains("invalid query"));
    }

    #[test]
    fn trailing_semicolon_and_comment_still_run() {
        for q in ["select id from data;", "select id from data; -- done"] {
            let conn = open_sqlite_conn().unwrap();
            let result = run_json_sql(conn, sample_rows(2), vec!["id".to_string()], q)
                .unwrap_or_else(|e| panic!("{q}: {e:#}"));
            assert_eq!(result.rows.len(), 2);
        }
    }

    #[test]
    fn a_second_statement_is_a_clean_error() {
        let conn = open_sqlite_conn().unwrap();
        let err = run_json_sql(
            conn,
            sample_rows(2),
            vec!["id".to_string()],
            "select id from data; select 2",
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("Only one SQL statement is allowed"),
            "{err}"
        );
    }

    /// Proves dropping the `InterruptOnDrop` guard actually stops a runaway query on the blocking
    /// thread it's running on, rather than just leaving `run_query`'s own timeout to fire while the
    /// thread keeps spinning. Emptying `InterruptOnDrop::drop`'s body makes this test hang until
    /// its 5s `timeout` panic instead of passing.
    #[tokio::test]
    async fn dropping_the_interrupt_guard_stops_a_runaway_query() {
        let conn = open_sqlite_conn().unwrap();
        let guard = InterruptOnDrop {
            handle: conn.get_interrupt_handle(),
            cancelled: std::sync::Arc::default(),
        };
        let task = tokio::task::spawn_blocking(move || {
            run_json_sql(
                conn,
                vec![],
                vec!["id".to_string()],
                "WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM r) \
                 SELECT count(*) FROM (SELECT x FROM r)",
            )
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(guard);
        let joined = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the blocking thread must stop once interrupted");
        assert!(joined.unwrap().is_err());
    }
}
