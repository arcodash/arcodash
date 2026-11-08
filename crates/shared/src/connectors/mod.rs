//! Connector framework: the `QueryRunner` trait and types shared by the backend and worker, one
//! module per connector type, looked up by `type_name` via [`registry::ConnectorRegistry`].

pub mod athena;
pub mod bigquery;
pub mod clickhouse;
pub mod elasticsearch;
pub mod http;
pub mod mongodb_connector;
pub mod mssql;
pub mod mysql;
pub mod postgres;
pub mod postgres_aliases;
pub mod query_results;
pub mod registry;
pub mod snowflake;
mod sql_scan;
pub mod sqlite_connector;

/// Flat re-export of the HTTP-family connectors.
pub mod http_connectors {
    pub use super::http::csv_upload::*;
    pub use super::http::databricks::*;
    pub use super::http::influxdb::*;
    pub use super::http::json_url::*;
    pub use super::http::prometheus::*;
    pub use super::http::*;
}

pub(crate) use sql_scan::scan_code_positions;
pub use sql_scan::{apply_auto_limit, sample_table_sql};

use anyhow::Context;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// One table/view's schema for the schema browser: a name plus its columns, in source-reported
/// order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<ColumnSchema>,
}

/// A column within a [`TableSchema`]: its name plus the source's own type spelling (`"UInt32"`).
/// `type_name` is `None` for sources without column types.
///
/// Deserializes from a bare column-name string or the full object, so older cached schemas still
/// load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ColumnSchema {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
}

impl ColumnSchema {
    /// A column whose source declares a type.
    pub fn typed(name: impl Into<String>, type_name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            type_name: Some(type_name.into()),
        }
    }

    /// A column from a source that declares no types.
    pub fn untyped(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            type_name: None,
        }
    }

    /// Like [`Self::typed`], but an empty `type_name` normalizes to `None`.
    pub fn typed_or_blank(name: impl Into<String>, type_name: impl AsRef<str>) -> Self {
        let type_name = type_name.as_ref();
        if type_name.trim().is_empty() {
            Self::untyped(name)
        } else {
            Self::typed(name, type_name)
        }
    }
}

impl<'de> Deserialize<'de> for ColumnSchema {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Object {
                name: String,
                #[serde(default)]
                type_name: Option<String>,
            },
            Name(String),
        }

        Ok(match Repr::deserialize(deserializer)? {
            Repr::Object { name, type_name } => Self { name, type_name },
            Repr::Name(name) => Self::untyped(name),
        })
    }
}

/// Coarse `friendly_type` tag for a result column (`"string"`, `"integer"`, `"float"`, `"boolean"`,
/// `"datetime"`, `"date"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnMeta {
    pub name: String,
    pub friendly_type: String,
}

impl ColumnMeta {
    /// Pairs deduplicated names with friendly types in order; a length mismatch truncates to the
    /// shorter (`Iterator::zip`).
    pub(crate) fn zip_types<S: Into<String>>(
        names: &[String],
        types: impl IntoIterator<Item = S>,
    ) -> Vec<Self> {
        names
            .iter()
            .cloned()
            .zip(types)
            .map(|(name, friendly_type)| Self {
                name,
                friendly_type: friendly_type.into(),
            })
            .collect()
    }

    /// Every column as `string`, for result shapes with no per-column types.
    pub(crate) fn all_string(names: &[String]) -> Vec<Self> {
        Self::zip_types(names, std::iter::repeat("string"))
    }
}

/// `friendly_type` inferred from a decoded sample value, for schemaless JSON sources.
pub(crate) fn friendly_type_from_json_sample(sample: Option<&serde_json::Value>) -> &'static str {
    match sample {
        Some(serde_json::Value::Number(n)) if n.is_i64() || n.is_u64() => "integer",
        Some(serde_json::Value::Number(_)) => "float",
        Some(serde_json::Value::Bool(_)) => "boolean",
        _ => "string",
    }
}

/// Result of running a query: column metadata, rows (JSON objects keyed by column name), and
/// execution time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryExecutionResult {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<serde_json::Value>,
    pub runtime_seconds: f64,
}

/// One outbound network destination: a bare hostname, or a full URL whose host is the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkTarget {
    Host(String),
    Url(String),
}

/// Rejects any target that resolves to a blocked address. Connectors that pin their client
/// (`http_connectors::pinned_http_client`) re-resolve and guard on their own path.
pub async fn guard_targets(targets: &[NetworkTarget]) -> anyhow::Result<()> {
    for target in targets {
        match target {
            NetworkTarget::Host(host) => {
                crate::net_guard::guard_host(host).await?;
            }
            NetworkTarget::Url(url) => {
                http_connectors::guard_url(url).await?;
            }
        }
    }
    Ok(())
}

/// One declared foreign-key column pair: `from_*` references `to_*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    pub from_table: String,
    pub from_column: String,
    pub to_table: String,
    pub to_column: String,
}

/// A data-source connector. `opts` is the data source's decrypted `encrypted_options` as JSON,
/// never raw ciphertext. Backend and worker share one implementation per type.
#[async_trait]
pub trait QueryRunner: Send + Sync {
    /// Short machine name, e.g. `"pg"` — matches `data_sources.type` and the registry key.
    fn type_name(&self) -> &'static str;

    /// JSON Schema-ish description of the connection form fields.
    fn configuration_schema(&self) -> serde_json::Value;

    /// Opens a connection and verifies it works (e.g. `SELECT 1`). Errors are human-readable and
    /// never include the options (they may hold a password).
    async fn test_connection(&self, opts: &serde_json::Value) -> anyhow::Result<()>;

    /// List every table/view visible to the connection's user, with column names.
    async fn get_schema(&self, opts: &serde_json::Value) -> anyhow::Result<Vec<TableSchema>>;

    /// Declared foreign keys, table names as `get_schema` spells them; empty by default.
    async fn get_foreign_keys(&self, _opts: &serde_json::Value) -> anyhow::Result<Vec<ForeignKey>> {
        Ok(vec![])
    }

    /// View names spelled as `get_schema` spells them.
    async fn get_view_names(&self, _opts: &serde_json::Value) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }

    /// Execute `query` verbatim (no rewriting) and return the decoded result.
    async fn run_query(
        &self,
        opts: &serde_json::Value,
        query: &str,
    ) -> anyhow::Result<QueryExecutionResult>;

    /// Every network destination `opts` produces, including assembled hostnames and nested targets.
    /// Unparseable options yield none (the connect path fails first).
    fn network_targets(&self, opts: &serde_json::Value) -> Vec<NetworkTarget>;

    /// Whether the worker should auto-append a `LIMIT` to unbounded `SELECT`s.
    fn supports_auto_limit(&self) -> bool {
        true
    }
}

/// Where a `run_query` failure happened: connecting (data source unusable) vs. after that (bad
/// query, missing table). Only `Connect` should flip health to Failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorErrorKind {
    Connect,
    Statement,
}

/// Tags an error with where it happened; `Display`/`source()` pass through unchanged.
#[derive(Debug)]
struct ConnectorError {
    kind: ConnectorErrorKind,
    inner: anyhow::Error,
}

impl std::fmt::Display for ConnectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.inner, f)
    }
}

impl std::error::Error for ConnectorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.source()
    }
}

/// Wraps a connect-step failure so [`classify_run_query_error`] can find it; later failures stay
/// plain and classify as [`ConnectorErrorKind::Statement`].
#[must_use]
pub fn connect_err(inner: anyhow::Error) -> anyhow::Error {
    ConnectorError {
        kind: ConnectorErrorKind::Connect,
        inner,
    }
    .into()
}

/// Classifies a `run_query` failure. Untagged errors default to `Statement`, the safe direction: a
/// bad query must never mark a data source unhealthy.
#[must_use]
pub fn classify_run_query_error(err: &anyhow::Error) -> ConnectorErrorKind {
    err.chain()
        .find_map(|e| e.downcast_ref::<ConnectorError>())
        .map_or(ConnectorErrorKind::Statement, |e| e.kind)
}

/// Renders an error chain, collapsing links already contained in the emitted text
/// (`sqlx::Error::Database`'s Display inlines its source, so `{:#}` would print it twice).
#[must_use]
pub fn format_error_chain(err: &anyhow::Error) -> String {
    let mut out = String::new();
    for link in err.chain() {
        let text = link.to_string();
        if out.contains(&text) {
            continue;
        }
        if !out.is_empty() {
            out.push_str(": ");
        }
        out.push_str(&text);
    }
    out
}

/// Makes column names unique for JSON row objects (a self-join can return a name twice, and a
/// `serde_json::Map` keeps only the last). Renames probe upward against already-emitted names, so
/// `a`, `a` and a real `a_1` stay distinct.
#[allow(
    clippy::maybe_infinite_iter,
    reason = "`taken` is finite, so `find` over the unbounded suffix range always terminates"
)]
pub(crate) fn dedupe_column_names(names: &mut [String]) {
    let mut taken = std::collections::HashSet::new();
    for name in names.iter_mut() {
        if taken.insert(name.clone()) {
            continue;
        }
        *name = (1..)
            .map(|n| format!("{name}_{n}"))
            .find(|candidate| !taken.contains(candidate))
            .expect("an unbounded range always yields a free suffix");
        taken.insert(name.clone());
    }
}

/// `raw` parsed as `T`, or kept as the raw string when it doesn't parse.
pub(crate) fn parse_or_raw<T: std::str::FromStr + Serialize>(raw: &str) -> serde_json::Value {
    raw.parse::<T>()
        .map_or_else(|_| serde_json::json!(raw), |v| serde_json::json!(v))
}

/// `http[s]://host:port` for the HTTP-API connectors.
pub(crate) fn http_base_url(use_https: bool, host: &str, port: u16) -> String {
    let scheme = if use_https { "https" } else { "http" };
    format!("{scheme}://{host}:{port}")
}

/// Appends `column` to the run for `name`, starting a new table when the name changes
/// (introspection queries order by table then ordinal position).
pub(crate) fn push_schema_column(
    tables: &mut Vec<TableSchema>,
    name: String,
    column: ColumnSchema,
) {
    match tables.last_mut() {
        Some(t) if t.name == name => t.columns.push(column),
        _ => tables.push(TableSchema {
            name,
            columns: vec![column],
        }),
    }
}

/// Per-connector connect budget, well below the query timeout so a dead host surfaces as "can't
/// connect".
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounds `fut` at [`CONNECT_TIMEOUT`]. Failures are tagged with `err_context`; a timeout reports
/// `timeout_message()` verbatim (callers' wording may be matched by tests or alert rules).
pub(crate) async fn connect_within<T, E>(
    err_context: &'static str,
    timeout_message: impl FnOnce() -> String,
    fut: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> anyhow::Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout(CONNECT_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(e).context(err_context),
        Err(_) => Err(anyhow::anyhow!("{}", timeout_message())),
    }
}

/// Reads an internal filter value the caller must inject into `opts`. Fail-closed: a missing one is
/// an error, never a default identity.
pub(crate) fn injected_id(
    opts: &serde_json::Value,
    field: &str,
    connector: &str,
) -> anyhow::Result<i64> {
    opts[field].as_i64().with_context(|| {
        format!(
            "{connector} connector: opts missing internal {field} filter \
             (caller bug — {field} must be injected into opts before invoking this connector)",
        )
    })
}

/// Deserializes a connector's typed options from stored JSON, attributing parse failures to the
/// connector.
pub(crate) fn parse_opts<T: serde::de::DeserializeOwned>(
    opts: &serde_json::Value,
    context: &'static str,
) -> anyhow::Result<T> {
    T::deserialize(opts).context(context)
}

/// Best-effort session-level statement timeout (seconds).
///
/// For connectors that support one (Postgres `statement_timeout`, MySQL `max_execution_time`), a
/// DB-side backstop to the worker's authoritative `QUERY_TIMEOUT_SECS`.
///
/// Reads `DB_STATEMENT_TIMEOUT_SECS` (default 300s).
#[must_use]
pub fn db_statement_timeout_secs() -> u64 {
    crate::config::env_parsed("DB_STATEMENT_TIMEOUT_SECS", 300)
}

/// Lazy env lookup of `QUERY_TIMEOUT_SECS` for connectors with their own poll/paginate loop
/// (Athena), since interactive calls have no external wrapper.
#[must_use]
pub fn query_timeout_secs() -> u64 {
    crate::config::env_parsed("QUERY_TIMEOUT_SECS", 300)
}

/// Lazy env lookup of the row cap for connectors `apply_auto_limit` can't bound. Not wired into any
/// connector yet; each keeps its own ceiling.
#[must_use]
pub fn max_query_rows() -> u32 {
    crate::config::env_parsed("MAX_QUERY_ROWS", 10_000)
}

#[cfg(test)]
mod db_statement_timeout_tests {
    use super::{db_statement_timeout_secs, max_query_rows};
    use std::sync::Mutex;

    // Serializes access to the shared process env var across test threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn defaults_to_300_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("DB_STATEMENT_TIMEOUT_SECS");
        assert_eq!(db_statement_timeout_secs(), 300);
    }

    #[test]
    fn reads_env_override() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("DB_STATEMENT_TIMEOUT_SECS", "7200");
        assert_eq!(db_statement_timeout_secs(), 7200);
        std::env::remove_var("DB_STATEMENT_TIMEOUT_SECS");
    }

    #[test]
    fn max_query_rows_reads_the_env_var() {
        let _rows_guard = crate::test_support::hold_max_query_rows_env();
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("MAX_QUERY_ROWS");
        assert_eq!(max_query_rows(), 10_000);
        std::env::set_var("MAX_QUERY_ROWS", "500");
        assert_eq!(max_query_rows(), 500);
        std::env::remove_var("MAX_QUERY_ROWS");
    }
}

#[cfg(test)]
mod connector_error_kind_tests {
    use super::{classify_run_query_error, connect_err, ConnectorErrorKind};

    #[test]
    fn tagged_connect_failure_classifies_as_connect() {
        let err = connect_err(anyhow::anyhow!("connection refused"));
        assert_eq!(classify_run_query_error(&err), ConnectorErrorKind::Connect);
    }

    #[test]
    fn untagged_failure_defaults_to_statement() {
        let err = anyhow::anyhow!("relation \"foo\" does not exist");
        assert_eq!(
            classify_run_query_error(&err),
            ConnectorErrorKind::Statement
        );
    }

    #[test]
    fn connect_tag_survives_additional_context_wrapping() {
        let err = connect_err(anyhow::anyhow!("connection refused")).context("run_query failed");
        assert_eq!(classify_run_query_error(&err), ConnectorErrorKind::Connect);
    }

    #[test]
    fn display_is_unchanged_by_tagging() {
        let err = connect_err(anyhow::anyhow!("connection refused"));
        assert_eq!(err.to_string(), "connection refused");
    }
}

#[cfg(test)]
mod format_error_chain_tests {
    use super::format_error_chain;

    // Mirrors sqlx's `Error::Database` Display, which inlines its source's text.
    #[derive(Debug)]
    struct DbError(&'static str);
    impl std::fmt::Display for DbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }
    impl std::error::Error for DbError {}

    #[derive(Debug)]
    struct DatabaseWrapper(DbError);
    impl std::fmt::Display for DatabaseWrapper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "error returned from database: {}", self.0)
        }
    }
    impl std::error::Error for DatabaseWrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn collapses_sqlx_style_duplication() {
        let inner = DatabaseWrapper(DbError("column \"nonexistent_column\" does not exist"));
        let err = anyhow::Error::new(inner).context("query execution failed");
        let out = format_error_chain(&err);
        assert_eq!(
            out,
            "query execution failed: error returned from database: column \"nonexistent_column\" does not exist"
        );
        assert_eq!(out.matches("nonexistent_column").count(), 1);
    }

    #[test]
    fn keeps_every_link_when_none_overlap() {
        #[derive(Debug)]
        struct Root;
        impl std::fmt::Display for Root {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "root cause")
            }
        }
        impl std::error::Error for Root {}

        let err = anyhow::Error::new(Root).context("middle").context("outer");
        assert_eq!(format_error_chain(&err), "outer: middle: root cause");
    }

    #[test]
    fn single_link_is_unchanged() {
        let err = anyhow::anyhow!("just one error");
        assert_eq!(format_error_chain(&err), "just one error");
    }
}

#[cfg(test)]
mod column_schema_tests {
    use super::ColumnSchema;

    #[test]
    fn typed_or_blank_treats_whitespace_only_as_untyped() {
        let col = ColumnSchema::typed_or_blank("id", "  ");
        assert_eq!(col.type_name, None);
    }

    #[test]
    fn typed_or_blank_keeps_untrimmed_type_when_non_blank() {
        let col = ColumnSchema::typed_or_blank("id", " int ");
        assert_eq!(col.type_name.as_deref(), Some(" int "));
    }
}

#[cfg(test)]
mod friendly_type_from_json_sample_tests {
    use super::friendly_type_from_json_sample;
    use serde_json::json;

    #[test]
    fn infers_from_the_sample_and_falls_back_to_string() {
        for (sample, want) in [
            (Some(json!(42)), "integer"),
            (Some(json!(-7)), "integer"),
            (Some(json!(1.5)), "float"),
            (Some(json!(true)), "boolean"),
            (Some(json!("hi")), "string"),
            (Some(json!(null)), "string"),
            (None, "string"),
        ] {
            assert_eq!(
                friendly_type_from_json_sample(sample.as_ref()),
                want,
                "{sample:?}"
            );
        }
    }
}

#[cfg(test)]
mod dedupe_column_names_tests {
    use super::dedupe_column_names;

    #[test]
    fn leaves_unique_names_unchanged() {
        let mut names = vec!["id".to_string(), "name".to_string()];
        dedupe_column_names(&mut names);
        assert_eq!(names, vec!["id", "name"]);
    }

    #[test]
    fn suffixes_repeats_in_order_of_appearance() {
        let mut names = vec!["id".to_string(), "id".to_string(), "id".to_string()];
        dedupe_column_names(&mut names);
        assert_eq!(names, vec!["id", "id_1", "id_2"]);
    }

    #[test]
    fn only_suffixes_the_colliding_name_not_others() {
        let mut names = vec!["id".to_string(), "name".to_string(), "id".to_string()];
        dedupe_column_names(&mut names);
        assert_eq!(names, vec!["id", "name", "id_1"]);
    }

    #[test]
    fn a_preexisting_suffix_does_not_collide_with_a_generated_one() {
        let mut names = vec!["a".to_string(), "a".to_string(), "a_1".to_string()];
        dedupe_column_names(&mut names);
        assert_eq!(names, vec!["a", "a_1", "a_1_1"]);
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
    }

    #[test]
    fn three_way_collision_stays_unique() {
        let mut names = vec![
            "a".to_string(),
            "a".to_string(),
            "a".to_string(),
            "a_1".to_string(),
            "a_2".to_string(),
        ];
        dedupe_column_names(&mut names);
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
    }
}

#[cfg(test)]
mod push_schema_column_tests {
    use super::{push_schema_column, ColumnSchema, TableSchema};

    /// A table name reappearing non-consecutively must start a new table, not reopen the earlier
    /// one.
    #[test]
    fn groups_consecutive_runs_and_reopens_a_repeated_name_as_a_new_table() {
        let mut tables: Vec<TableSchema> = Vec::new();
        for (table, col) in [("t1", "a"), ("t1", "b"), ("t2", "c"), ("t1", "d")] {
            push_schema_column(&mut tables, table.to_string(), ColumnSchema::untyped(col));
        }
        assert_eq!(tables.len(), 3);
        assert_eq!(tables[0].name, "t1");
        assert_eq!(
            tables[0]
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(tables[1].name, "t2");
        assert_eq!(tables[1].columns.len(), 1);
        assert_eq!(tables[2].name, "t1");
        assert_eq!(tables[2].columns[0].name, "d");
    }
}

#[cfg(test)]
mod column_meta_tests {
    use super::ColumnMeta;

    #[test]
    fn zip_types_pairs_names_with_types_in_order() {
        let names = vec!["id".to_string(), "id_1".to_string()];
        let types = vec!["integer".to_string(), "string".to_string()];
        let cols = ColumnMeta::zip_types(&names, types);
        assert_eq!(cols[0].name, "id");
        assert_eq!(cols[0].friendly_type, "integer");
        assert_eq!(cols[1].name, "id_1");
        assert_eq!(cols[1].friendly_type, "string");
    }

    #[test]
    fn zip_types_stops_at_the_shorter_input() {
        let names = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let types = vec!["integer".to_string()];
        assert_eq!(ColumnMeta::zip_types(&names, types).len(), 1);
    }

    #[test]
    fn all_string_labels_every_column_string() {
        let names = vec!["a".to_string(), "b".to_string()];
        let cols = ColumnMeta::all_string(&names);
        assert!(cols.iter().all(|c| c.friendly_type == "string"));
        assert_eq!(
            cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }
}

#[cfg(test)]
mod connect_within_tests {
    use super::connect_within;

    #[tokio::test]
    async fn a_future_finishing_within_the_budget_succeeds() {
        let ok = connect_within("unused", || "unused".to_string(), async {
            Ok::<i32, std::io::Error>(42)
        })
        .await
        .expect("a fast future must not time out");
        assert_eq!(ok, 42);
    }

    #[tokio::test]
    async fn a_real_failure_is_wrapped_in_the_caller_supplied_context() {
        let err = connect_within(
            "failed to connect to Postgres data source",
            || "unused".to_string(),
            async { Err::<(), _>(std::io::Error::other("connection refused")) },
        )
        .await
        .expect_err("a failed future must surface as an error");
        assert_eq!(err.to_string(), "failed to connect to Postgres data source");
        assert_eq!(
            format!("{err:#}"),
            "failed to connect to Postgres data source: connection refused"
        );
    }

    /// Pins the helper against the literal Postgres timeout wording.
    #[tokio::test]
    async fn a_timeout_reports_the_caller_supplied_message_verbatim() {
        let err = connect_within(
            "failed to connect to Postgres data source",
            || "timed out connecting to db.example.com:5432".to_string(),
            async {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                Ok::<(), std::io::Error>(())
            },
        )
        .await
        .expect_err("a future slower than CONNECT_TIMEOUT must time out");
        assert_eq!(
            err.to_string(),
            "timed out connecting to db.example.com:5432"
        );
    }
}

#[cfg(test)]
mod injected_id_tests {
    use super::injected_id;
    use serde_json::json;

    #[test]
    fn fails_closed_and_names_the_connector_and_field_when_missing() {
        let err = injected_id(&json!({}), "org_id", "widget").unwrap_err();
        assert_eq!(
            err.to_string(),
            "widget connector: opts missing internal org_id filter \
             (caller bug — org_id must be injected into opts before invoking this connector)"
        );
    }

    #[test]
    fn reads_the_injected_value() {
        assert_eq!(
            injected_id(&json!({"org_id": 42}), "org_id", "widget").unwrap(),
            42
        );
    }
}

#[cfg(test)]
mod parse_opts_tests {
    use super::parse_opts;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize)]
    struct Opts {
        host: String,
    }

    #[test]
    fn attributes_a_parse_failure_to_the_given_context() {
        let err = parse_opts::<Opts>(&json!({}), "invalid Widget options").unwrap_err();
        assert!(err.to_string().contains("invalid Widget options"));
    }

    #[test]
    fn deserializes_a_valid_blob() {
        let opts: Opts = parse_opts(&json!({"host": "h"}), "invalid Widget options").unwrap();
        assert_eq!(opts.host, "h");
    }
}
