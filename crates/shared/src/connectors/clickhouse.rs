//! `ClickHouse` connector via HTTP API (port 8123).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::http_connectors::{percent_encode, pinned_http_client, validate_identifier};
use super::{
    ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult, QueryRunner, TableSchema,
};

/// A `ClickHouse` query runs to completion in one HTTP request (no polling), so this must cover
/// the slowest legitimate query, not just a connect/response-header wait — the worker's own
/// `QUERY_TIMEOUT_SECS` cap (300s default) plus margin. Deliberately not the generic connector
/// `HTTP_TIMEOUT` (30s), which would abort any query still running past a normal connect.
const HTTP_TIMEOUT: Duration = Duration::from_secs(310);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClickHouseOptions {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
    pub db: String,
    #[serde(default)]
    pub use_https: bool,
}

const fn default_port() -> u16 {
    8123
}

/// SQL for `get_view_names`; `db` must already be `validate_identifier`-checked.
fn build_view_names_sql(db: &str) -> String {
    format!(
        "SELECT name FROM system.tables WHERE database = '{db}' \
         AND engine IN ('View', 'MaterializedView') ORDER BY name"
    )
}

pub struct ClickHouseConnector;

fn parse_options(opts: &Value) -> Result<ClickHouseOptions> {
    super::parse_opts(opts, "invalid ClickHouse options")
}

/// Whether `sql` (already trimmed of a trailing `;`/whitespace) ends in a `FORMAT <name>`
/// clause, optionally followed by a trailing `SETTINGS <assignments>` run (`ClickHouse` allows
/// both after a query). Only inspects the trailing token run rather than searching the whole
/// string for the word "format", so a column or alias literally named `format` (e.g.
/// `SELECT format FROM t`) isn't mistaken for the clause.
const fn ends_in_format(tokens: &[&str]) -> bool {
    tokens.len() >= 2 && tokens[tokens.len() - 2].eq_ignore_ascii_case("format")
}

fn has_trailing_format(sql: &str) -> bool {
    let tokens: Vec<&str> = sql.split_whitespace().collect();
    // FORMAT can also precede a trailing `SETTINGS <assignments>` run (`FORMAT x SETTINGS a=1`), so
    // if the plain tail check fails, retry with that run stripped. Requiring at least one token
    // after `SETTINGS` keeps a trailing column/alias literally named `settings` from being mistaken
    // for the clause. Checking the unstripped tail first means a query with SETTINGS *before*
    // FORMAT (ClickHouse's own canonical order) matches directly, so its real trailing FORMAT is
    // never mistaken for the end of a SETTINGS run.
    ends_in_format(&tokens)
        || tokens
            .iter()
            .rposition(|t| t.eq_ignore_ascii_case("settings"))
            .is_some_and(|i| i + 1 < tokens.len() && ends_in_format(&tokens[..i]))
}

async fn run_http_query(opts: &ClickHouseOptions, query: &str) -> Result<String> {
    let addrs = crate::net_guard::guard_host(&opts.host).await?;
    let client = pinned_http_client(&opts.host, &addrs)?;
    let url = format!(
        "{}/?database={}",
        super::http_base_url(opts.use_https, &opts.host, opts.port),
        percent_encode(&opts.db)
    );

    // A trailing `;` makes `<query>\nFORMAT JSONCompact` a syntax error, and a query that
    // already names its own FORMAT would otherwise get a second, conflicting one appended.
    let trimmed = query.trim_end_matches(|c: char| c == ';' || c.is_whitespace());
    if has_trailing_format(trimmed) {
        anyhow::bail!(
            "this connector requires ClickHouse's JSONCompact output format and cannot parse a \
             query with its own FORMAT clause — remove the trailing FORMAT and let the \
             connector add it"
        );
    }
    let body = format!("{trimmed}\nFORMAT JSONCompact");

    let mut req = client.post(&url).body(body).timeout(HTTP_TIMEOUT);
    if !opts.user.is_empty() {
        req = req.basic_auth(&opts.user, Some(&opts.password));
    }
    let resp = req
        .send()
        .await
        .map_err(super::http_connectors::classify_reqwest_err)
        .context("ClickHouse HTTP request failed")?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("ClickHouse error: {body}");
    }
    resp.text().await.context("read response failed")
}

fn friendly_type(ch_type: &str) -> &'static str {
    let t = ch_type
        .trim_start_matches("Nullable(")
        .trim_end_matches(')');
    let t = t.split('(').next().unwrap_or(t);
    if t.starts_with("Int") || t.starts_with("UInt") {
        return "integer";
    }
    if t.starts_with("Float") || t.starts_with("Decimal") {
        return "float";
    }
    if t == "Bool" {
        return "boolean";
    }
    if t.contains("DateTime") || t.contains("Timestamp") {
        return "datetime";
    }
    if t == "Date" || t == "Date32" {
        return "date";
    }
    "string"
}

#[async_trait]
impl QueryRunner for ClickHouseConnector {
    fn type_name(&self) -> &'static str {
        "clickhouse"
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "title": "Host" },
                "port": { "type": "integer", "title": "HTTP Port", "default": 8123 },
                "user": { "type": "string", "title": "User" },
                "password": { "type": "string", "title": "Password" },
                "db": { "type": "string", "title": "Database" },
                "use_https": { "type": "boolean", "title": "Use HTTPS", "default": false }
            },
            "required": ["host", "db"],
            "secret": ["password"]
        })
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let opts = parse_options(opts)?;
        run_http_query(&opts, "SELECT 1").await.map(|_| ())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let opts = parse_options(opts)?;
        validate_identifier(&opts.db, "database")?;
        let sql = format!(
            "SELECT table, name, type FROM system.columns WHERE database = '{}' ORDER BY table, position",
            opts.db
        );
        let raw = run_http_query(&opts, &sql).await?;
        let parsed: Value = serde_json::from_str(&raw).context("parse schema failed")?;
        let mut tables: Vec<TableSchema> = Vec::new();
        for row in parsed["data"].as_array().into_iter().flatten() {
            let cell = |i: usize| row[i].as_str().unwrap_or("");
            let column = ColumnSchema::typed_or_blank(cell(1), cell(2));
            super::push_schema_column(&mut tables, cell(0).to_string(), column);
        }
        Ok(tables)
    }

    async fn get_view_names(&self, opts: &Value) -> Result<Vec<String>> {
        let opts = parse_options(opts)?;
        validate_identifier(&opts.db, "database")?;
        let raw = run_http_query(&opts, &build_view_names_sql(&opts.db)).await?;
        let parsed: Value = serde_json::from_str(&raw).context("parse view names failed")?;
        Ok(parsed["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| row.as_array()?.first()?.as_str().map(String::from))
            .collect())
    }

    fn network_targets(&self, opts: &Value) -> Vec<NetworkTarget> {
        parse_options(opts)
            .map(|opts| vec![NetworkTarget::Host(opts.host)])
            .unwrap_or_default()
    }

    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        let opts = parse_options(opts)?;
        let start = Instant::now();
        let raw = run_http_query(&opts, query).await?;
        let runtime_seconds = start.elapsed().as_secs_f64();
        let parsed: Value = serde_json::from_str(&raw).context("parse result failed")?;

        let meta = parsed["meta"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let data = parsed["data"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();

        // A self-join can return the same column name twice; de-duplicate before it's used as a
        // JSON object key below, or the second value silently overwrites the first.
        let mut names: Vec<String> = meta
            .iter()
            .map(|m| m["name"].as_str().unwrap_or("").to_string())
            .collect();
        super::dedupe_column_names(&mut names);
        let types = meta
            .iter()
            .map(|m| friendly_type(m["type"].as_str().unwrap_or("")));
        let columns = ColumnMeta::zip_types(&names, types);

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
mod tests {
    use super::*;

    #[test]
    fn build_view_names_sql_covers_views_and_materialized_views() {
        let sql = build_view_names_sql("analytics");
        assert!(sql.contains("database = 'analytics'"));
        assert!(sql.contains("engine IN ('View', 'MaterializedView')"));
    }

    /// A real (not stubbed) connect-level failure — nothing at 127.0.0.1:1 answers as
    /// ClickHouse, so `run_query` fails inside `run_http_query`'s `.send()` — must classify as
    /// `Connect`, never `Statement`, matching `postgres.rs`'s equivalent live test. Loopback
    /// (not a public IP like `postgres.rs` uses) so the OS returns ECONNREFUSED immediately
    /// instead of waiting out `pinned_http_client()`'s 10s connect timeout.
    ///
    /// Needs `ARCODASH_ALLOW_PRIVATE_CONNECTORS=1` (same opt-out `scripts/test-connectors.sh` sets)
    /// already present in the environment to reach loopback past the SSRF guard — skips otherwise
    /// rather than setting it itself: this crate's tests run in parallel by default, and
    /// `net_guard`'s own tests assert that guard is *on*, so flipping the env var mid-run would
    /// race and intermittently break them. Run manually with the opt-out set; skipped by default.
    #[tokio::test]
    async fn live_run_query_against_unreachable_host_classifies_as_connect_error() {
        let _env = crate::test_support::hold_private_net_env();
        if std::env::var(crate::net_guard::ALLOW_PRIVATE_ENV).as_deref() != Ok("1") {
            eprintln!(
                "[SKIP] {}=1 not set — skipping live loopback connect-failure test",
                crate::net_guard::ALLOW_PRIVATE_ENV
            );
            return;
        }
        let connector = ClickHouseConnector;
        let opts = json!({
            "host": "127.0.0.1",
            "port": 1,
            "db": "nope",
            "use_https": false
        });
        let err = connector
            .run_query(&opts, "SELECT 1")
            .await
            .expect_err("127.0.0.1:1 must not be a live ClickHouse — connection must fail");
        assert_eq!(
            crate::connectors::classify_run_query_error(&err),
            crate::connectors::ConnectorErrorKind::Connect,
            "a real connect-level failure must classify as Connect, got: {err:#}"
        );
    }

    #[test]
    fn friendly_type_maps_known_clickhouse_types() {
        assert_eq!(friendly_type("UInt32"), "integer");
        assert_eq!(friendly_type("Int64"), "integer");
        assert_eq!(friendly_type("Float64"), "float");
        assert_eq!(friendly_type("Decimal(10,2)"), "float");
        assert_eq!(friendly_type("Bool"), "boolean");
        assert_eq!(friendly_type("DateTime"), "datetime");
        assert_eq!(friendly_type("Date"), "date");
        assert_eq!(friendly_type("String"), "string");
        assert_eq!(friendly_type("Nullable(UInt32)"), "integer");
    }

    #[test]
    fn friendly_type_maps_date32_to_date() {
        assert_eq!(friendly_type("Date32"), "date");
    }

    #[test]
    fn has_trailing_format_is_false_for_a_column_literally_named_format() {
        assert!(!has_trailing_format("SELECT format FROM t"));
    }

    #[test]
    fn has_trailing_format_detects_a_real_format_clause() {
        assert!(has_trailing_format("SELECT 1 FORMAT TSV"));
    }

    #[test]
    fn has_trailing_format_is_case_insensitive() {
        assert!(has_trailing_format("select 1 format jsoneachrow"));
    }

    #[test]
    fn has_trailing_format_is_false_with_no_format_clause() {
        assert!(!has_trailing_format("SELECT 1"));
    }

    #[test]
    fn has_trailing_format_detects_a_format_followed_by_settings() {
        assert!(has_trailing_format(
            "SELECT 1 FORMAT JSONEachRow SETTINGS max_threads=1"
        ));
    }

    #[test]
    fn has_trailing_format_detects_a_format_followed_by_multiple_settings() {
        assert!(has_trailing_format("SELECT 1 FORMAT TSV SETTINGS a=1, b=2"));
    }

    #[test]
    fn has_trailing_format_is_false_for_settings_with_no_format() {
        assert!(!has_trailing_format("SELECT 1 SETTINGS max_threads=1"));
    }

    #[test]
    fn has_trailing_format_detects_settings_before_format() {
        assert!(has_trailing_format(
            "SELECT 1 SETTINGS max_threads=1 FORMAT JSONEachRow"
        ));
    }

    #[test]
    fn has_trailing_format_detects_format_after_a_column_named_settings() {
        assert!(has_trailing_format(
            "SELECT 1 WHERE settings = 1 FORMAT JSON"
        ));
    }
}
