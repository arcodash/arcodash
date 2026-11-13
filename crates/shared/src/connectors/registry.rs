//! `ConnectorRegistry` — the `type` → `QueryRunner` factory.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::DatabaseConnection;

use super::athena::AthenaConnector;
use super::bigquery::BigQueryConnector;
use super::clickhouse::ClickHouseConnector;
use super::elasticsearch::ElasticsearchConnector;
use super::http_connectors::{
    CsvConnector, DatabricksConnector, InfluxDbConnector, JsonConnector, PrometheusConnector,
};
use super::mongodb_connector::MongoConnector;
use super::mssql::MssqlConnector;
use super::mysql::MySqlConnector;
use super::postgres::PostgresConnector;
use super::postgres_aliases::{CockroachDbConnector, RedshiftConnector};
use super::query_results::QueryResultsConnector;
use super::snowflake::SnowflakeConnector;
use super::sqlite_connector::SqliteConnector;
use super::{
    classify_run_query_error, connect_err, guard_targets, query_timeout_secs, NetworkTarget,
    QueryExecutionResult, QueryRunner, TableSchema,
};
use crate::org_storage::OrgStorageResolver;

#[derive(Clone)]
pub struct ConnectorRegistry {
    connectors: HashMap<&'static str, Arc<dyn QueryRunner>>,
}

impl ConnectorRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            connectors: HashMap::new(),
        }
    }

    pub fn register(&mut self, connector: Arc<dyn QueryRunner>) {
        self.connectors.insert(connector.type_name(), connector);
    }

    /// Wraps the registered connector in [`LoggingConnector`] so every caller gets lifecycle
    /// logging.
    #[must_use]
    pub fn get(&self, type_name: &str) -> Option<Arc<dyn QueryRunner>> {
        self.connectors
            .get(type_name)
            .cloned()
            .map(|inner| Arc::new(LoggingConnector { inner }) as Arc<dyn QueryRunner>)
    }

    /// Every registered connector; used by `GET /api/data_sources/types` to advertise the catalog.
    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn QueryRunner>> {
        self.connectors.values()
    }
}

/// Records an SSRF-guard rejection through the same metrics and log line as other connector
/// failures, so a hostile failure mode isn't undercounted. Wrapped via [`connect_err`] since the
/// guard rejects before connecting.
fn record_guarded_error(
    connector: &'static str,
    op: &'static str,
    e: anyhow::Error,
) -> anyhow::Error {
    let e = connect_err(e);
    crate::metrics::record_connector_operation(connector, op, "error");
    let error_kind = classify_run_query_error(&e);
    crate::metrics::record_connector_error(connector, error_kind);
    tracing::warn!(connector, op, error = %e, ?error_kind, "connector operation blocked by SSRF guard");
    e
}

/// Classify, count and log a failed connector operation; the sibling of [`record_guarded_error`]
/// for connector-reported failures.
fn record_operation_error(
    connector: &'static str,
    op: &'static str,
    e: anyhow::Error,
) -> anyhow::Error {
    let error_kind = classify_run_query_error(&e);
    crate::metrics::record_connector_error(connector, error_kind);
    tracing::warn!(connector, op, error = %e, ?error_kind, "connector operation failed");
    e
}

/// Counts `result` as an `op` outcome, then records it via [`record_operation_error`] if it failed.
fn record_outcome<T>(
    connector: &'static str,
    op: &'static str,
    result: anyhow::Result<T>,
) -> anyhow::Result<T> {
    crate::metrics::record_connector_operation(
        connector,
        op,
        if result.is_ok() { "ok" } else { "error" },
    );
    result.map_err(|e| record_operation_error(connector, op, e))
}

/// Bounds `fut` at `QUERY_TIMEOUT_SECS`: interactive callers may have no timeout of their own.
async fn bounded<T>(
    op: &'static str,
    fut: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(std::time::Duration::from_secs(query_timeout_secs()), fut)
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("{op} timed out")))
}

/// Delegates every [`QueryRunner`] method to `inner`, logging around the three that do I/O
/// (`test_connection`, `get_schema`, `run_query`).
struct LoggingConnector {
    inner: Arc<dyn QueryRunner>,
}

impl LoggingConnector {
    /// Guarding here rather than per connector means a new connector can't leave its targets
    /// unguarded; a rejection counts as a connect failure in metrics.
    async fn guard(&self, op: &'static str, opts: &serde_json::Value) -> anyhow::Result<()> {
        guard_targets(&self.inner.network_targets(opts))
            .await
            .map_err(|e| record_guarded_error(self.inner.type_name(), op, e))
    }
}

#[async_trait]
impl QueryRunner for LoggingConnector {
    fn type_name(&self) -> &'static str {
        self.inner.type_name()
    }

    fn configuration_schema(&self) -> serde_json::Value {
        self.inner.configuration_schema()
    }

    // `opts`/`options` never appear in a log field: they are the decrypted config, secrets
    // included.

    async fn test_connection(&self, opts: &serde_json::Value) -> anyhow::Result<()> {
        let connector = self.inner.type_name();
        self.guard("test_connection", opts).await?;
        tracing::debug!(
            connector,
            op = "test_connection",
            "connector operation starting"
        );
        let result = self.inner.test_connection(opts).await;
        record_outcome(connector, "test_connection", result)
    }

    async fn get_foreign_keys(
        &self,
        opts: &serde_json::Value,
    ) -> anyhow::Result<Vec<super::ForeignKey>> {
        self.guard("get_foreign_keys", opts).await?;
        bounded("get_foreign_keys", self.inner.get_foreign_keys(opts))
            .await
            .map_err(|e| record_operation_error(self.inner.type_name(), "get_foreign_keys", e))
    }

    async fn get_view_names(&self, opts: &serde_json::Value) -> anyhow::Result<Vec<String>> {
        self.guard("get_view_names", opts).await?;
        bounded("get_view_names", self.inner.get_view_names(opts))
            .await
            .map_err(|e| record_operation_error(self.inner.type_name(), "get_view_names", e))
    }

    async fn get_schema(&self, opts: &serde_json::Value) -> anyhow::Result<Vec<TableSchema>> {
        let connector = self.inner.type_name();
        self.guard("get_schema", opts).await?;
        tracing::debug!(connector, op = "get_schema", "connector operation starting");
        let result = bounded("get_schema", self.inner.get_schema(opts)).await;
        record_outcome(connector, "get_schema", result)
    }

    async fn run_query(
        &self,
        opts: &serde_json::Value,
        query: &str,
    ) -> anyhow::Result<QueryExecutionResult> {
        let connector = self.inner.type_name();
        self.guard("run_query", opts).await?;
        tracing::debug!(connector, op = "run_query", "connector operation starting");
        match self.inner.run_query(opts, query).await {
            Ok(result) => {
                crate::metrics::record_connector_operation(connector, "run_query", "ok");
                tracing::info!(
                    connector,
                    runtime_seconds = result.runtime_seconds,
                    rows = result.rows.len(),
                    "connector query succeeded"
                );
                Ok(result)
            }
            Err(e) => {
                let error_kind = classify_run_query_error(&e);
                crate::metrics::record_connector_operation(connector, "run_query", "error");
                crate::metrics::record_connector_error(connector, error_kind);
                tracing::warn!(connector, error = %e, ?error_kind, "connector query failed");
                Err(e)
            }
        }
    }

    fn network_targets(&self, opts: &serde_json::Value) -> Vec<NetworkTarget> {
        self.inner.network_targets(opts)
    }

    fn supports_auto_limit(&self) -> bool {
        self.inner.supports_auto_limit()
    }
}

impl ConnectorRegistry {
    /// Build the full application registry; `storage` backs the upload-based CSV/SQLite connectors.
    pub fn with_db(db: DatabaseConnection, storage: Arc<dyn OrgStorageResolver>) -> Self {
        let mut r = Self::new();

        r.register(Arc::new(PostgresConnector::new()));
        r.register(Arc::new(QueryResultsConnector::new(
            db.clone(),
            storage.clone(),
        )));
        r.register(Arc::new(MySqlConnector::mysql()));
        r.register(Arc::new(MySqlConnector::mariadb()));
        r.register(Arc::new(MySqlConnector::tidb()));
        r.register(Arc::new(SqliteConnector::new(db.clone(), storage.clone())));
        r.register(Arc::new(RedshiftConnector::new()));
        r.register(Arc::new(CockroachDbConnector::new()));
        r.register(Arc::new(ClickHouseConnector));
        r.register(Arc::new(DatabricksConnector));
        r.register(Arc::new(ElasticsearchConnector));
        r.register(Arc::new(MongoConnector));
        r.register(Arc::new(PrometheusConnector));
        r.register(Arc::new(InfluxDbConnector));
        r.register(Arc::new(JsonConnector));
        r.register(Arc::new(CsvConnector::new(db, storage)));
        r.register(Arc::new(MssqlConnector));
        r.register(Arc::new(BigQueryConnector));
        r.register(Arc::new(SnowflakeConnector));
        r.register(Arc::new(AthenaConnector));

        r
    }
}

impl Default for ConnectorRegistry {
    fn default() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(PostgresConnector::new()));
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::ObjectStore;
    use serde_json::json;

    /// The full registry; `bucket` only has to satisfy `ObjectStore::from_env`.
    fn full_registry(bucket: &str) -> ConnectorRegistry {
        unsafe {
            std::env::set_var("UPLOAD_BUCKET", bucket);
        }
        let store = ObjectStore::from_env().expect("object store from env");
        unsafe {
            std::env::remove_var("UPLOAD_BUCKET");
        }
        ConnectorRegistry::with_db(
            DatabaseConnection::default(),
            Arc::new(crate::org_storage::DefaultStorageResolver::new(store)),
        )
    }

    #[test]
    fn default_registry_has_postgres_connector() {
        let registry = ConnectorRegistry::default();
        assert!(registry.get("pg").is_some());
    }

    #[test]
    fn unknown_type_returns_none() {
        let registry = ConnectorRegistry::default();
        assert!(registry.get("nonexistent").is_none());
    }

    /// Every connector must declare a `"secret"` field list (possibly empty) in its
    /// `configuration_schema`; `options_without_secrets` withholds everything otherwise, so this
    /// catches the gap at the registry level.
    #[test]
    fn every_registered_connector_declares_a_secret_field_list() {
        let registry = full_registry("test-bucket-registry-secret-check");
        for connector in registry.all() {
            let schema = connector.configuration_schema();
            assert!(
                schema["secret"].is_array(),
                "{}: configuration_schema() must declare a \"secret\" array (possibly empty)",
                connector.type_name()
            );
        }
    }

    /// Every connector's `network_targets` must tolerate empty options without panicking (a draft
    /// data source).
    #[test]
    fn every_registered_connector_network_targets_tolerates_empty_options() {
        let registry = full_registry("test-bucket-registry-network-targets-check");
        for connector in registry.all() {
            // The call itself is the assertion.
            let _ = connector.network_targets(&json!({}));
        }
    }

    /// Every connector must enumerate a real network destination for parseable options; otherwise
    /// it silently opts out of the `guard_targets` SSRF chokepoint. The closing loop makes a new
    /// connector's omission a build failure.
    #[test]
    fn every_registered_connector_enumerates_its_network_targets() {
        let registry = full_registry("test-bucket-registry-network-targets-enumeration-check");

        // (type_name, parseable options with a sentinel host/URL, expected target substring).
        // Athena and BigQuery only report a target via `api_endpoint`.
        let table: Vec<(&str, serde_json::Value, &str)> = vec![
            (
                "pg",
                json!({"host": "sentinel.invalid", "port": 5432, "user": "u", "password": "p", "dbname": "d"}),
                "sentinel.invalid",
            ),
            (
                "mysql",
                json!({"host": "sentinel.invalid", "port": 3306, "user": "u", "password": "p", "db": "d"}),
                "sentinel.invalid",
            ),
            (
                "mariadb",
                json!({"host": "sentinel.invalid", "port": 3306, "user": "u", "password": "p", "db": "d"}),
                "sentinel.invalid",
            ),
            (
                "tidb",
                json!({"host": "sentinel.invalid", "port": 4000, "user": "u", "password": "p", "db": "d"}),
                "sentinel.invalid",
            ),
            (
                "redshift",
                json!({"host": "sentinel.invalid", "port": 5439, "user": "u", "password": "p", "dbname": "d"}),
                "sentinel.invalid",
            ),
            (
                "cockroachdb",
                json!({"host": "sentinel.invalid", "port": 26257, "user": "u", "password": "p", "dbname": "d"}),
                "sentinel.invalid",
            ),
            (
                "clickhouse",
                json!({"host": "sentinel.invalid", "port": 8123, "user": "u", "password": "p", "db": "d"}),
                "sentinel.invalid",
            ),
            (
                "databricks",
                json!({"host": "sentinel.invalid", "http_path": "/sql/1.0/warehouses/abc", "token": "t"}),
                "sentinel.invalid",
            ),
            (
                "es",
                json!({"host": "sentinel.invalid"}),
                "sentinel.invalid",
            ),
            (
                "mongodb",
                json!({"host": "sentinel.invalid", "db": "d"}),
                "sentinel.invalid",
            ),
            (
                "prometheus",
                json!({"host": "sentinel.invalid"}),
                "sentinel.invalid",
            ),
            (
                "influxdb",
                json!({"host": "sentinel.invalid", "db": "d"}),
                "sentinel.invalid",
            ),
            (
                "json",
                json!({"url": "http://sentinel.invalid/x"}),
                "sentinel.invalid",
            ),
            (
                "mssql",
                json!({"server": "sentinel.invalid", "user": "u", "password": "p", "db": "d"}),
                "sentinel.invalid",
            ),
            (
                "bigquery",
                json!({"project_id": "proj", "api_endpoint": "http://169.254.169.254/bq"}),
                "169.254.169.254",
            ),
            (
                "snowflake",
                json!({"account": "sentinel-acct", "user": "u", "database": "d"}),
                "sentinel-acct.snowflakecomputing.com",
            ),
            (
                "athena",
                json!({
                    "aws_access_key": "ak",
                    "aws_secret_key": "sk",
                    "region": "us-east-1",
                    "s3_output_location": "s3://bucket/prefix/",
                    "database": "d",
                    "api_endpoint": "http://169.254.169.254/athena"
                }),
                "169.254.169.254",
            ),
        ];

        let mut covered: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (type_name, opts, expect_substr) in table {
            covered.insert(type_name);
            let connector = registry
                .get(type_name)
                .unwrap_or_else(|| panic!("{type_name}: not registered"));
            let targets = connector.network_targets(&opts);
            assert!(
                !targets.is_empty(),
                "{type_name}: network_targets returned no targets for populated options {opts} \
                 — the options failed to parse, or this connector never routes through \
                 network_targets"
            );
            let matched = targets.iter().any(|t| match t {
                NetworkTarget::Host(s) | NetworkTarget::Url(s) => s.contains(expect_substr),
            });
            assert!(
                matched,
                "{type_name}: expected a target containing {expect_substr:?}, got {targets:?}"
            );
        }

        const NO_NETWORK: &[&str] = &["query_results", "csv", "sqlite"];
        for connector in registry.all() {
            let type_name = connector.type_name();
            assert!(
                NO_NETWORK.contains(&type_name) || covered.contains(type_name),
                "{type_name}: not covered by the network_targets enumeration table above, the \
                 NO_NETWORK allowlist — add a table row, or justify adding it to \
                 NO_NETWORK if it genuinely performs no outbound network I/O"
            );
        }
    }

    /// A connector whose `network_targets` resolves to loopback, proving a guard rejection is
    /// recorded in metrics.
    struct GuardRejectedConnector;

    #[async_trait]
    impl QueryRunner for GuardRejectedConnector {
        fn type_name(&self) -> &'static str {
            "guard-rejected-metrics-test"
        }

        fn configuration_schema(&self) -> serde_json::Value {
            json!({"secret": []})
        }

        fn network_targets(&self, _opts: &serde_json::Value) -> Vec<NetworkTarget> {
            vec![NetworkTarget::Host("127.0.0.1".to_string())]
        }

        async fn test_connection(&self, _opts: &serde_json::Value) -> anyhow::Result<()> {
            panic!("guard must reject before the inner connector is ever called")
        }

        async fn get_schema(&self, _opts: &serde_json::Value) -> anyhow::Result<Vec<TableSchema>> {
            panic!("guard must reject before the inner connector is ever called")
        }

        async fn run_query(
            &self,
            _opts: &serde_json::Value,
            _query: &str,
        ) -> anyhow::Result<QueryExecutionResult> {
            panic!("guard must reject before the inner connector is ever called")
        }
    }

    #[tokio::test]
    async fn ssrf_guard_rejection_is_counted_as_a_connector_error() {
        let _env = crate::test_support::private_net_env(None);
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(GuardRejectedConnector));
        let connector = registry.get("guard-rejected-metrics-test").unwrap();

        let err = connector
            .test_connection(&json!({}))
            .await
            .expect_err("a loopback target must be rejected by the SSRF guard");
        assert!(err.to_string().contains("blocked"));

        let mut out = String::new();
        crate::metrics::render(&mut out);
        assert!(
            out.contains(
                "arcodash_connector_operations_total{connector=\"guard-rejected-metrics-test\",op=\"test_connection\",outcome=\"error\"}"
            ),
            "guard rejection must count as a failed connector operation: {out}"
        );
        assert!(
            out.contains("arcodash_connector_errors_total{connector=\"guard-rejected-metrics-test\",kind=\"connect\"}"),
            "guard rejection must be classified/counted as a connect error: {out}"
        );
    }

    /// A connector whose `get_schema` never resolves, so `LoggingConnector`'s own bound has to save
    /// it.
    struct HangingSchemaConnector;

    #[async_trait]
    impl QueryRunner for HangingSchemaConnector {
        fn type_name(&self) -> &'static str {
            "hanging-schema-test"
        }

        fn configuration_schema(&self) -> serde_json::Value {
            json!({"secret": []})
        }

        fn network_targets(&self, _opts: &serde_json::Value) -> Vec<NetworkTarget> {
            vec![]
        }

        async fn test_connection(&self, _opts: &serde_json::Value) -> anyhow::Result<()> {
            anyhow::bail!("not exercised by this test")
        }

        async fn get_schema(&self, _opts: &serde_json::Value) -> anyhow::Result<Vec<TableSchema>> {
            std::future::pending().await
        }

        async fn run_query(
            &self,
            _opts: &serde_json::Value,
            _query: &str,
        ) -> anyhow::Result<QueryExecutionResult> {
            anyhow::bail!("not exercised by this test")
        }
    }

    #[tokio::test]
    async fn get_schema_times_out_instead_of_hanging_forever() {
        std::env::set_var("QUERY_TIMEOUT_SECS", "1");
        let mut registry = ConnectorRegistry::new();
        registry.register(Arc::new(HangingSchemaConnector));
        let connector = registry.get("hanging-schema-test").unwrap();

        let err = connector
            .get_schema(&json!({}))
            .await
            .expect_err("a get_schema that never resolves must still time out");
        std::env::remove_var("QUERY_TIMEOUT_SECS");
        assert!(
            err.to_string().contains("timed out"),
            "expected a timeout error, got: {err}"
        );
    }
}
