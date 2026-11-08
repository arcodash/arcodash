//! Databricks SQL connector: the statement-execution REST API
//! (`/api/2.0/sql/statements`), synchronous mode.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::connectors::{
    dedupe_column_names, ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult,
    QueryRunner, TableSchema,
};

use super::{pinned_http_client, validate_identifier};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabricksOptions {
    pub host: String,
    pub http_path: String,
    pub token: String,
    #[serde(default)]
    pub catalog: String,
    #[serde(default)]
    pub schema: String,
}

pub struct DatabricksConnector;

/// Server-side `wait_timeout` above (50s) plus margin — matches Snowflake's identical precedent
/// for a synchronous statement-execution API. Not the generic `HTTP_TIMEOUT`: that would cut off
/// a legitimately slow query before the server's own wait window even elapses.
const DATABRICKS_TIMEOUT: Duration = Duration::from_secs(65);

async fn databricks_exec(opts: &DatabricksOptions, statement: &str) -> Result<Value> {
    let addrs = crate::net_guard::guard_host(&opts.host).await?;
    let url = format!("https://{}/api/2.0/sql/statements/", opts.host);
    let client = pinned_http_client(&opts.host, &addrs)?;
    let body = json!({
        "statement": statement,
        "wait_timeout": "50s",
        "on_wait_timeout": "CANCEL",
        "format": "JSON_ARRAY",
        "warehouse_id": opts.http_path.rsplit('/').next().unwrap_or("")
    });
    let resp = client
        .post(&url)
        .bearer_auth(&opts.token)
        .json(&body)
        .timeout(DATABRICKS_TIMEOUT)
        .send()
        .await
        .context("Databricks API request failed")?;
    if !resp.status().is_success() {
        // A 401/403/5xx body carries the real reason (invalid token, warehouse stopped, …) —
        // read it before it's gone rather than let `.json()` below parse it as a generic value
        // and lose it to the "no error detail" fallback further down.
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Databricks API returned an error status: {body}");
    }
    let resp: Value = resp
        .json()
        .await
        .context("failed to parse Databricks response")?;
    if let Some(err) = resp
        .pointer("/status/error/message")
        .and_then(Value::as_str)
    {
        anyhow::bail!("Databricks error: {err}");
    }
    // `on_wait_timeout: CANCEL` (above) means a query that outran the server-side wait window
    // comes back CANCELED with no error message at all — indistinguishable from a genuine empty
    // result unless the state itself is checked. Every non-SUCCEEDED branch bails, so an
    // unrecognized future state fails safe rather than silently reading as success.
    match resp.pointer("/status/state").and_then(Value::as_str) {
        Some("SUCCEEDED") => {}
        Some(state @ ("CANCELED" | "CLOSED")) => anyhow::bail!(
            "Databricks statement did not complete (state {state}); the query exceeded the \
             server-side wait timeout"
        ),
        Some(state @ ("PENDING" | "RUNNING")) => anyhow::bail!(
            "Databricks statement is still {state} after the wait timeout; \
             the query is too slow for this connector's synchronous execution mode"
        ),
        Some("FAILED") | None => anyhow::bail!("Databricks statement failed with no error detail"),
        Some(other) => anyhow::bail!("Databricks returned unexpected statement state {other}"),
    }
    Ok(resp)
}

/// Row cap for a Databricks statement result — no way to thread the worker's own
/// `max_query_rows` into this crate's connector layer, so this hardcodes the same default value.
const DATABRICKS_MAX_ROWS: usize = 10_000;
/// Chunk-following iteration cap, independent of the row cap, so a server reporting more/smaller
/// chunks than rows present can't spin this loop forever.
const DATABRICKS_MAX_CHUNKS: usize = 2000;

/// Merge every page's `data_array` rows (in order) into one row set, enforcing
/// `DATABRICKS_MAX_ROWS`. A page carrying `external_links` instead of `data_array` — Databricks'
/// transport for results too large to inline — isn't fetched by this connector (a signed-URL GET
/// is a separate implementation this doesn't have), so it errors rather than silently dropping
/// rows. Pure/no-network, so it's exercised directly by unit tests.
fn merge_databricks_pages(pages: &[Value]) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for page in pages {
        if page.get("external_links").is_some() {
            anyhow::bail!(
                "this result is returned via external links, which this connector does not support"
            );
        }
        rows.extend(page["data_array"].as_array().into_iter().flatten().cloned());
        if rows.len() > DATABRICKS_MAX_ROWS {
            anyhow::bail!(
                "Databricks result exceeds this connector's row cap ({DATABRICKS_MAX_ROWS} rows); \
                 narrow the query"
            );
        }
    }
    Ok(rows)
}

/// Follow `result.next_chunk_index` across every page of a Databricks statement result (the
/// first page from `databricks_exec`'s own response, since only the first chunk is inlined
/// there), fetching each subsequent chunk from `/result/chunks/{n}`. Returns the raw pages for
/// [`merge_databricks_pages`] to merge.
async fn fetch_databricks_pages(
    opts: &DatabricksOptions,
    first_resp: &Value,
) -> Result<Vec<Value>> {
    let mut pages = vec![first_resp["result"].clone()];
    let mut next_chunk_index = first_resp
        .pointer("/result/next_chunk_index")
        .and_then(Value::as_u64);
    if next_chunk_index.is_none() {
        return Ok(pages);
    }
    let statement_id = first_resp["statement_id"].as_str().ok_or_else(|| {
        anyhow::anyhow!(
            "Databricks response has more result chunks but no statement_id to fetch them with"
        )
    })?;
    let addrs = crate::net_guard::guard_host(&opts.host).await?;
    let client = pinned_http_client(&opts.host, &addrs)?;
    let mut chunks_fetched = 0;
    while let Some(idx) = next_chunk_index {
        chunks_fetched += 1;
        if chunks_fetched > DATABRICKS_MAX_CHUNKS {
            anyhow::bail!(
                "Databricks result has more chunks than this connector will follow ({DATABRICKS_MAX_CHUNKS})"
            );
        }
        let url = format!(
            "https://{}/api/2.0/sql/statements/{statement_id}/result/chunks/{idx}",
            opts.host
        );
        let page: Value = client
            .get(&url)
            .bearer_auth(&opts.token)
            .timeout(DATABRICKS_TIMEOUT)
            .send()
            .await
            .context("Databricks result chunk fetch failed")?
            .json()
            .await?;
        next_chunk_index = page["next_chunk_index"].as_u64();
        pages.push(page);
    }
    Ok(pages)
}

/// Databricks reports its own vocabulary (`TINYINT`/`SMALLINT`/`INT`/`BIGINT`,
/// `FLOAT`/`DOUBLE`/`DECIMAL`, `BOOLEAN`, `DATE`/`TIMESTAMP`, `STRING`, `BINARY`, `INTERVAL`,
/// `ARRAY`/`MAP`/`STRUCT`, ...) — matched exactly rather than by substring, since a substring
/// match on `"INT"` also (wrongly) catches `INTERVAL` and `POINT`.
fn db_friendly_type(t: &str) -> &'static str {
    match t.to_uppercase().as_str() {
        "TINYINT" | "SMALLINT" | "INT" | "BIGINT" => "integer",
        "FLOAT" | "DOUBLE" | "DECIMAL" => "float",
        "BOOLEAN" => "boolean",
        "DATE" | "TIMESTAMP" => "datetime",
        _ => "string",
    }
}

/// Full column-level schema via Unity Catalog's `information_schema.columns`. Requires a
/// configured `catalog` — Unity Catalog exposes `information_schema` per-catalog, and there's no
/// safe default catalog name to guess when none is configured — so this returns `Ok(None)` (not
/// an error) rather than fail the whole schema fetch, letting the caller fall back to a coarser
/// SHOW TABLES-based listing (also the fallback for a legacy `hive_metastore` catalog, which has no
/// `information_schema` at all).
async fn databricks_information_schema(
    opts: &DatabricksOptions,
) -> Result<Option<Vec<TableSchema>>> {
    if opts.catalog.is_empty() {
        return Ok(None);
    }
    validate_identifier(&opts.catalog, "catalog")?;
    let where_clause = if opts.schema.is_empty() {
        String::new()
    } else {
        validate_identifier(&opts.schema, "schema")?;
        format!("WHERE table_schema = '{}'", opts.schema)
    };
    let sql = format!(
        "SELECT table_name, column_name, data_type FROM {}.information_schema.columns {where_clause} \
         ORDER BY table_name, ordinal_position",
        opts.catalog
    );
    let result = databricks_exec(opts, &sql).await?;
    let pages = fetch_databricks_pages(opts, &result).await?;
    let rows = merge_databricks_pages(&pages)?;

    let mut tables: Vec<TableSchema> = Vec::new();
    for row in rows {
        let cell = |i: usize| row[i].as_str().unwrap_or_default();
        let column = ColumnSchema::typed_or_blank(cell(1), cell(2));
        crate::connectors::push_schema_column(&mut tables, cell(0).to_string(), column);
    }
    Ok(Some(tables))
}

#[async_trait]
impl QueryRunner for DatabricksConnector {
    fn type_name(&self) -> &'static str {
        "databricks"
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "title": "Server Hostname" },
                "http_path": { "type": "string", "title": "HTTP Path (warehouse)" },
                "token": { "type": "string", "title": "Personal Access Token" },
                "catalog": { "type": "string", "title": "Catalog (optional)" },
                "schema": { "type": "string", "title": "Schema (optional)" }
            },
            "required": ["host", "http_path", "token"],
            "secret": ["token"]
        })
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let opts: DatabricksOptions = serde_json::from_value(opts.clone())?;
        databricks_exec(&opts, "SELECT 1").await.map(|_| ())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let opts: DatabricksOptions = serde_json::from_value(opts.clone())?;

        match databricks_information_schema(&opts).await {
            Ok(Some(tables)) => return Ok(tables),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Databricks information_schema.columns query failed; falling back to SHOW TABLES"
                );
            }
        }

        let schema_filter = if opts.schema.is_empty() {
            String::new()
        } else {
            validate_identifier(&opts.schema, "schema")?;
            format!("IN SCHEMA {}", opts.schema)
        };
        let result = databricks_exec(&opts, &format!("SHOW TABLES {schema_filter}")).await?;
        let tables = result["result"]["data_array"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                Some(TableSchema {
                    name: r.as_array()?.get(1)?.as_str()?.to_string(),
                    columns: vec![],
                })
            })
            .collect();
        Ok(tables)
    }

    fn network_targets(&self, opts: &Value) -> Vec<NetworkTarget> {
        serde_json::from_value::<DatabricksOptions>(opts.clone())
            .map(|opts| vec![NetworkTarget::Host(opts.host)])
            .unwrap_or_default()
    }

    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        let opts: DatabricksOptions = serde_json::from_value(opts.clone())?;
        let start = Instant::now();
        let result = databricks_exec(&opts, query).await?;

        let schema = result["manifest"]["schema"]["columns"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut col_names: Vec<String> = schema
            .iter()
            .map(|c| c["name"].as_str().unwrap_or("").to_string())
            .collect();
        dedupe_column_names(&mut col_names);
        let types = schema
            .iter()
            .map(|c| db_friendly_type(c["type_name"].as_str().unwrap_or("")));
        let columns = ColumnMeta::zip_types(&col_names, types);

        let pages = fetch_databricks_pages(&opts, &result).await?;
        let data = merge_databricks_pages(&pages)?;
        let runtime_seconds = start.elapsed().as_secs_f64();

        let json_rows = data
            .iter()
            .map(|row| {
                let cells = columns.iter().enumerate();
                Value::Object(
                    cells
                        .map(|(i, c)| (c.name.clone(), row[i].clone()))
                        .collect(),
                )
            })
            .collect();

        Ok(QueryExecutionResult {
            columns,
            rows: json_rows,
            runtime_seconds,
        })
    }
}

#[cfg(test)]
mod databricks_tests {
    use super::{db_friendly_type, merge_databricks_pages};
    use serde_json::json;

    #[test]
    fn friendly_type_matches_exact_names_not_substrings() {
        assert_eq!(db_friendly_type("INTERVAL"), "string");
        assert_eq!(db_friendly_type("INT"), "integer");
        assert_eq!(db_friendly_type("BIGINT"), "integer");
        assert_eq!(db_friendly_type("POINT"), "string");
        assert_eq!(db_friendly_type("TIMESTAMP"), "datetime");
        assert_eq!(db_friendly_type("DECIMAL"), "float");
        assert_eq!(db_friendly_type("BOOLEAN"), "boolean");
        assert_eq!(db_friendly_type("STRUCT"), "string");
    }

    #[test]
    fn merge_pages_concatenates_data_array_rows_in_order() {
        let pages = vec![
            json!({"data_array": [["1", "a"]]}),
            json!({"data_array": [["2", "b"]]}),
        ];
        let rows = merge_databricks_pages(&pages).expect("merge failed");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], json!(["1", "a"]));
        assert_eq!(rows[1], json!(["2", "b"]));
    }

    #[test]
    fn merge_pages_errors_clearly_on_external_links() {
        let pages = vec![json!({"external_links": [{"external_link": "https://example.com"}]})];
        let err = merge_databricks_pages(&pages).unwrap_err();
        assert!(err.to_string().contains("external links"));
    }

    #[test]
    fn merge_pages_errors_past_the_row_cap_instead_of_truncating_silently() {
        let big_page = json!({
            "data_array": (0..(super::DATABRICKS_MAX_ROWS + 1)).map(|i| json!([i.to_string()])).collect::<Vec<_>>()
        });
        let err = merge_databricks_pages(&[big_page]).unwrap_err();
        assert!(err.to_string().contains("row cap"));
    }
}
