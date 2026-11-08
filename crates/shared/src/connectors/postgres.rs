//! Postgres connector for an arbitrary user-supplied target database, distinct from the app's own
//! metadata DB; raw `sqlx` gives the dynamic column decoding sea-orm can't.

use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgConnection, PgRow};
use sqlx::{Column, Connection, Row, TypeInfo, ValueRef};

use super::{
    ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult, QueryRunner, TableSchema,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresOptions {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub password: String,
    pub dbname: String,
    #[serde(default = "default_sslmode")]
    pub sslmode: String,
}

const fn default_port() -> u16 {
    5432
}

fn default_sslmode() -> String {
    "prefer".to_string()
}

pub struct PostgresConnector;

impl PostgresConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for PostgresConnector {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_options(opts: &Value) -> Result<PostgresOptions> {
    super::parse_opts(opts, "invalid Postgres connection options")
}

fn ssl_mode(raw: &str) -> sqlx::postgres::PgSslMode {
    use sqlx::postgres::PgSslMode;
    match raw {
        "disable" => PgSslMode::Disable,
        "require" => PgSslMode::Require,
        _ => PgSslMode::Prefer,
    }
}

fn connect_options(opts: &PostgresOptions) -> PgConnectOptions {
    PgConnectOptions::new()
        .host(&opts.host)
        .port(opts.port)
        .username(&opts.user)
        .password(&opts.password)
        .database(&opts.dbname)
        .ssl_mode(ssl_mode(&opts.sslmode))
        // DB-side backstop; the worker's `tokio::time::timeout` is the authoritative enforcement.
        .options([(
            "statement_timeout",
            (super::db_statement_timeout_secs() * 1000).to_string(),
        )])
}

/// Cross-schema names are qualified as "schema.name"; the "public" schema is left bare.
#[allow(
    clippy::needless_pass_by_value,
    reason = "owned params drive the sqlx `try_get` decode type at every call site"
)]
fn qualify(schema: String, name: String) -> String {
    if schema == "public" {
        name
    } else {
        format!("{schema}.{name}")
    }
}

async fn connect(opts: &PostgresOptions) -> Result<PgConnection> {
    crate::net_guard::guard_host(&opts.host).await?;
    let connect_opts = connect_options(opts);
    super::connect_within(
        "failed to connect to Postgres data source",
        || format!("timed out connecting to {}:{}", opts.host, opts.port),
        PgConnection::connect_with(&connect_opts),
    )
    .await
}

#[async_trait]
impl QueryRunner for PostgresConnector {
    fn type_name(&self) -> &'static str {
        "pg"
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "title": "Host" },
                "port": { "type": "integer", "title": "Port", "default": 5432 },
                "user": { "type": "string", "title": "User" },
                "password": { "type": "string", "title": "Password" },
                "dbname": { "type": "string", "title": "Database Name" },
                "sslmode": {
                    "type": "string",
                    "title": "SSL Mode",
                    "enum": ["disable", "prefer", "require"],
                    "default": "prefer"
                }
            },
            "required": ["host", "user", "dbname"],
            "secret": ["password"]
        })
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let opts = parse_options(opts)?;
        let mut conn = connect(&opts).await?;
        sqlx::query("SELECT 1")
            .execute(&mut conn)
            .await
            .context("test query (SELECT 1) failed")?;
        Ok(())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let opts = parse_options(opts)?;
        let mut conn = connect(&opts).await?;

        let rows = sqlx::query(
            "SELECT table_schema, table_name, column_name, data_type \
             FROM information_schema.columns \
             WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
             ORDER BY table_schema, table_name, ordinal_position",
        )
        .fetch_all(&mut conn)
        .await
        .context("failed to introspect schema")?;

        let mut tables: Vec<TableSchema> = Vec::new();
        for row in rows {
            let name = qualify(row.try_get("table_schema")?, row.try_get("table_name")?);
            let column_name: String = row.try_get("column_name")?;
            let data_type: String = row.try_get("data_type")?;
            super::push_schema_column(
                &mut tables,
                name,
                ColumnSchema::typed(column_name, data_type),
            );
        }
        Ok(tables)
    }

    async fn get_foreign_keys(&self, opts: &Value) -> Result<Vec<super::ForeignKey>> {
        let opts = parse_options(opts)?;
        let mut conn = connect(&opts).await?;
        // pg_constraint pairs composite-key FK columns by position, which information_schema can't
        // do reliably.
        let rows = sqlx::query(
            "SELECT fn.nspname AS from_schema, fc.relname AS from_table, fa.attname AS from_column, \
                    tn.nspname AS to_schema, tc.relname AS to_table, ta.attname AS to_column \
             FROM pg_constraint c \
             JOIN pg_class fc ON fc.oid = c.conrelid \
             JOIN pg_namespace fn ON fn.oid = fc.relnamespace \
             JOIN pg_class tc ON tc.oid = c.confrelid \
             JOIN pg_namespace tn ON tn.oid = tc.relnamespace \
             CROSS JOIN LATERAL unnest(c.conkey, c.confkey) AS k(fk_attnum, tk_attnum) \
             JOIN pg_attribute fa ON fa.attrelid = c.conrelid AND fa.attnum = k.fk_attnum \
             JOIN pg_attribute ta ON ta.attrelid = c.confrelid AND ta.attnum = k.tk_attnum \
             WHERE c.contype = 'f' AND fn.nspname NOT IN ('pg_catalog', 'information_schema') \
             ORDER BY 1, 2, 3",
        )
        .fetch_all(&mut conn)
        .await
        .context("failed to introspect foreign keys")?;
        rows.iter()
            .map(|r| {
                Ok(super::ForeignKey {
                    from_table: qualify(r.try_get("from_schema")?, r.try_get("from_table")?),
                    from_column: r.try_get("from_column")?,
                    to_table: qualify(r.try_get("to_schema")?, r.try_get("to_table")?),
                    to_column: r.try_get("to_column")?,
                })
            })
            .collect()
    }

    async fn get_view_names(&self, opts: &Value) -> Result<Vec<String>> {
        let opts = parse_options(opts)?;
        let mut conn = connect(&opts).await?;
        let rows = sqlx::query(
            "SELECT table_schema AS schema_name, table_name AS name \
             FROM information_schema.tables \
             WHERE table_type = 'VIEW' AND table_schema NOT IN ('pg_catalog', 'information_schema') \
             UNION \
             SELECT schemaname::text, matviewname::text FROM pg_matviews \
             ORDER BY 1, 2",
        )
        .fetch_all(&mut conn)
        .await
        .context("failed to introspect views")?;
        rows.iter()
            .map(|r| Ok(qualify(r.try_get("schema_name")?, r.try_get("name")?)))
            .collect()
    }

    fn network_targets(&self, opts: &Value) -> Vec<NetworkTarget> {
        parse_options(opts)
            .map(|opts| vec![NetworkTarget::Host(opts.host)])
            .unwrap_or_default()
    }

    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        let opts = parse_options(opts)?;
        let mut conn = connect(&opts).await.map_err(super::connect_err)?;

        let started = Instant::now();
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .fetch_all(&mut conn)
            .await
            .context("query execution failed")?;
        let runtime_seconds = started.elapsed().as_secs_f64();

        let columns = column_metadata(&rows);
        // Rows are keyed by the de-duplicated names, so a self-join's repeated column doesn't
        // collapse into one JSON key.
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        let json_rows = rows
            .iter()
            .map(|r| row_to_json(r, &names))
            .collect::<Result<Vec<_>>>()?;

        Ok(QueryExecutionResult {
            columns,
            rows: json_rows,
            runtime_seconds,
        })
    }
}

/// Extract column name + `friendly_type` from the first row (representative; an empty result yields
/// no columns). Names are de-duplicated (`id`, `id_1`, ...).
fn column_metadata(rows: &[PgRow]) -> Vec<ColumnMeta> {
    let Some(first) = rows.first() else {
        return Vec::new();
    };

    let mut names: Vec<String> = first
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    super::dedupe_column_names(&mut names);

    let types = first
        .columns()
        .iter()
        .map(|c| friendly_type_for_pg_type(c.type_info().name()));
    ColumnMeta::zip_types(&names, types)
}

/// Map a Postgres type name to the coarse `friendly_type` tags; arrays map to their element type.
fn friendly_type_for_pg_type(pg_type: &str) -> String {
    // sqlx reports arrays as "INT4[]", not "_int4"; strip the "[]" to get the element type.
    let upper = pg_type.to_uppercase();
    let base = upper.strip_suffix("[]").unwrap_or(&upper);
    match base {
        "BOOL" => "boolean",
        "INT2" | "INT4" | "INT8" => "integer",
        "FLOAT4" | "FLOAT8" | "NUMERIC" => "float",
        "TIMESTAMP" | "TIMESTAMPTZ" => "datetime",
        "DATE" => "date",
        // Everything else is decoded as text (see `decode_cell`'s fallback) and reported as
        // "string".
        _ => "string",
    }
    .to_string()
}

/// Decode one row into a JSON object keyed by column name. Covers text, ints, floats, numeric (as a
/// string to avoid f64 loss), bool, date, timestamps, json/jsonb, uuid and 1-D arrays of those.
/// Other types (ranges, composites, enums, geometric) degrade to a string representation rather
/// than erroring the query.
fn row_to_json(row: &PgRow, names: &[String]) -> Result<Value> {
    let mut obj = serde_json::Map::with_capacity(row.columns().len());

    for (idx, col) in row.columns().iter().enumerate() {
        let value = decode_cell(row, idx, col.name(), col.type_info().name())
            .with_context(|| format!("failed to decode column '{}'", col.name()))?;
        obj.insert(names[idx].clone(), value);
    }

    Ok(Value::Object(obj))
}

fn decode_cell(row: &PgRow, idx: usize, column_name: &str, pg_type: &str) -> Result<Value> {
    let raw = row.try_get_raw(idx)?;
    if raw.is_null() {
        return Ok(Value::Null);
    }

    // Normalize to uppercase; array arms match the "[]"-suffixed form.
    let pg_type_upper = pg_type.to_uppercase();
    let value = match pg_type_upper.as_str() {
        "BOOL" => json!(row.try_get::<bool, _>(idx)?),
        "INT2" => json!(row.try_get::<i16, _>(idx)?),
        "INT4" => json!(row.try_get::<i32, _>(idx)?),
        "INT8" => json!(row.try_get::<i64, _>(idx)?),
        "FLOAT4" => json!(row.try_get::<f32, _>(idx)?),
        "FLOAT8" => json!(row.try_get::<f64, _>(idx)?),
        "NUMERIC" => json!(row
            .try_get::<sqlx::types::Decimal, _>(idx)
            .map(|d| d.to_string())?),
        "DATE" => json!(row.try_get::<chrono::NaiveDate, _>(idx)?.to_string()),
        // `timestamp without time zone` carries no zone, so emit the wall-clock value as-is rather
        // than assert an offset (mysql.rs matches this shape).
        "TIMESTAMP" => json!(row
            .try_get::<chrono::NaiveDateTime, _>(idx)?
            .format("%Y-%m-%dT%H:%M:%S%.f")
            .to_string()),
        "TIMESTAMPTZ" => json!(row
            .try_get::<chrono::DateTime<chrono::Utc>, _>(idx)?
            .to_rfc3339()),
        "JSON" | "JSONB" => row.try_get::<Value, _>(idx)?,
        "UUID" => json!(row.try_get::<uuid::Uuid, _>(idx)?.to_string()),
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CHAR" => {
            json!(row.try_get::<String, _>(idx)?)
        }
        "BOOL[]" => json!(row.try_get::<Vec<bool>, _>(idx)?),
        "INT2[]" => json!(row.try_get::<Vec<i16>, _>(idx)?),
        "INT4[]" => json!(row.try_get::<Vec<i32>, _>(idx)?),
        "INT8[]" => json!(row.try_get::<Vec<i64>, _>(idx)?),
        "FLOAT4[]" => json!(row.try_get::<Vec<f32>, _>(idx)?),
        "FLOAT8[]" => json!(row.try_get::<Vec<f64>, _>(idx)?),
        "NUMERIC[]" => json!(row
            .try_get::<Vec<sqlx::types::Decimal>, _>(idx)?
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()),
        "TEXT[]" | "VARCHAR[]" => json!(row.try_get::<Vec<String>, _>(idx)?),
        "UUID[]" => json!(row
            .try_get::<Vec<uuid::Uuid>, _>(idx)?
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()),
        // Fallback for unsupported types: decode as text rather than fail the request.
        _ => row.try_get::<String, _>(idx).map_or_else(
            |_| {
                tracing::debug!(
                    column = column_name,
                    pg_type,
                    "undecodable cell degraded to null"
                );
                Value::Null
            },
            |s| json!(s),
        ),
    };

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SSRF guard: a data source pointing at the cloud metadata IP is rejected before any
    /// connection is attempted.
    #[tokio::test]
    async fn test_connection_rejects_metadata_ip() {
        let _env = crate::test_support::private_net_env(None);
        let connector = PostgresConnector::new();
        let opts = json!({
            "host": "169.254.169.254",
            "port": 5432,
            "user": "u",
            "password": "p",
            "dbname": "d",
            "sslmode": "disable"
        });
        let err = connector
            .test_connection(&opts)
            .await
            .expect_err("connection to the metadata IP must be blocked, not attempted");
        assert!(err.to_string().contains("blocked"), "got: {err}");
    }

    // Live connector tests; skipped when TEST_TARGET_DATABASE_URL is unset.

    /// [`target_db_opts`], logging the skip when it is unavailable.
    fn live_target_opts() -> Option<Value> {
        let opts = target_db_opts();
        if opts.is_none() {
            eprintln!("[SKIP] TEST_TARGET_DATABASE_URL not set — skipping live connector test");
        }
        opts
    }

    fn target_db_opts() -> Option<Value> {
        let url = std::env::var("TEST_TARGET_DATABASE_URL").ok()?;
        // Manual parse of the URL avoids adding the `url` crate for test code.
        let rest = url
            .strip_prefix("postgres://")
            .or_else(|| url.strip_prefix("postgresql://"))?;
        let (userinfo, hostpath) = rest.split_once('@')?;
        let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        let (hostport, dbname) = hostpath.split_once('/')?;
        let (host, port_str) = hostport.split_once(':').unwrap_or((hostport, "5432"));
        let port: u16 = port_str.parse().unwrap_or(5432);
        Some(json!({
            "host": host,
            "port": port,
            "user": user,
            "password": password,
            "dbname": dbname,
            "sslmode": "disable"
        }))
    }

    /// A real connect-level failure (nothing at 8.8.8.8:5433 answers as Postgres) must classify as
    /// `Connect`, never `Statement`.
    #[tokio::test]
    async fn live_run_query_against_unreachable_host_classifies_as_connect_error() {
        let _env = crate::test_support::private_net_env(None);
        let connector = PostgresConnector::new();
        let opts = json!({
            "host": "8.8.8.8",
            "port": 5433,
            "user": "nope",
            "password": "nope",
            "dbname": "nope",
            "sslmode": "disable"
        });
        let err = connector
            .run_query(&opts, "SELECT 1")
            .await
            .expect_err("8.8.8.8:5433 must not be a live Postgres — connection must fail");
        assert_eq!(
            crate::connectors::classify_run_query_error(&err),
            crate::connectors::ConnectorErrorKind::Connect,
            "a real connect-level failure must classify as Connect, got: {err:#}"
        );
    }

    /// A refused connect must surface the real cause, not sqlx's generic "pool timed out". Binds
    /// then drops a listener to get a dead port.
    #[tokio::test]
    async fn live_run_query_against_closed_port_surfaces_real_connect_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let _env = crate::test_support::private_net_env(Some("1"));
        let connector = PostgresConnector::new();
        let opts = json!({
            "host": "127.0.0.1",
            "port": port,
            "user": "nope",
            "password": "nope",
            "dbname": "nope",
            "sslmode": "disable"
        });
        let err = connector
            .run_query(&opts, "SELECT 1")
            .await
            .expect_err("closed local port must not be a live Postgres — connection must fail");

        let msg = format!("{err:#}").to_lowercase();
        assert!(
            !msg.contains("pool timed out"),
            "a direct connection must not mask the failure behind a pool timeout, got: {err:#}"
        );
        assert!(
            msg.contains("refused") || msg.contains("timed out connecting"),
            "expected the real underlying connect failure to surface, got: {err:#}"
        );
        assert_eq!(
            crate::connectors::classify_run_query_error(&err),
            crate::connectors::ConnectorErrorKind::Connect,
            "a real connect-level failure must classify as Connect, got: {err:#}"
        );
    }

    #[tokio::test]
    async fn live_get_foreign_keys_reports_declared_pairs() {
        let _env = crate::test_support::private_net_env(Some("1"));
        let Some(opts) = live_target_opts() else {
            return;
        };
        let connector = PostgresConnector::new();
        for sql in [
            "DROP SCHEMA IF EXISTS fk_probe CASCADE",
            "CREATE SCHEMA fk_probe",
            "CREATE TABLE fk_probe.parent (a int, b int, PRIMARY KEY (a, b))",
            "CREATE TABLE fk_probe.child (id int PRIMARY KEY, pa int, pb int, \
             FOREIGN KEY (pa, pb) REFERENCES fk_probe.parent (a, b))",
        ] {
            connector.run_query(&opts, sql).await.expect(sql);
        }
        let fks = connector.get_foreign_keys(&opts).await;
        connector
            .run_query(&opts, "DROP SCHEMA fk_probe CASCADE")
            .await
            .unwrap();
        let fk = |fc: &str, tc: &str| super::super::ForeignKey {
            from_table: "fk_probe.child".into(),
            from_column: fc.into(),
            to_table: "fk_probe.parent".into(),
            to_column: tc.into(),
        };
        let fks = fks.unwrap();
        assert!(
            fks.contains(&fk("pa", "a")) && fks.contains(&fk("pb", "b")),
            "{fks:?}"
        );
    }

    #[tokio::test]
    async fn live_view_names_lists_views_not_tables() {
        let _env = crate::test_support::private_net_env(Some("1"));
        let Some(opts) = live_target_opts() else {
            return;
        };
        let connector = PostgresConnector::new();
        for sql in [
            "DROP SCHEMA IF EXISTS view_probe CASCADE",
            "CREATE SCHEMA view_probe",
            "CREATE TABLE view_probe.t (a int)",
            "CREATE VIEW view_probe.v AS SELECT a FROM view_probe.t",
            "CREATE MATERIALIZED VIEW view_probe.mv AS SELECT a FROM view_probe.t",
        ] {
            connector.run_query(&opts, sql).await.expect(sql);
        }
        let names = connector.get_view_names(&opts).await;
        connector
            .run_query(&opts, "DROP SCHEMA view_probe CASCADE")
            .await
            .unwrap();
        let names = names.unwrap();
        assert!(
            names.contains(&"view_probe.v".to_string())
                && names.contains(&"view_probe.mv".to_string())
                && !names.contains(&"view_probe.t".to_string()),
            "{names:?}"
        );
    }

    #[tokio::test]
    async fn live_test_connection_select_1() {
        let _env = crate::test_support::private_net_env(Some("1"));
        let Some(opts) = live_target_opts() else {
            return;
        };
        let connector = PostgresConnector::new();
        connector
            .test_connection(&opts)
            .await
            .expect("test_connection (SELECT 1) failed against target DB");
    }

    #[tokio::test]
    async fn live_run_query_typed_table() {
        let _env = crate::test_support::private_net_env(Some("1"));
        let Some(opts) = live_target_opts() else {
            return;
        };
        let connector = PostgresConnector::new();
        let result = connector
            .run_query(&opts, "SELECT * FROM typed_table ORDER BY id")
            .await
            .expect("run_query against typed_table failed");

        assert_eq!(result.rows.len(), 2, "expected 2 seeded rows");

        let col_type = |name: &str| -> &str {
            result
                .columns
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.friendly_type.as_str())
                .unwrap_or("MISSING")
        };
        assert_eq!(col_type("id"), "integer");
        assert_eq!(col_type("bool_col"), "boolean");
        assert_eq!(col_type("int_col"), "integer");
        assert_eq!(col_type("bigint_col"), "integer");
        assert_eq!(col_type("float_col"), "float");
        assert_eq!(col_type("numeric_col"), "float");
        assert_eq!(col_type("text_col"), "string");
        assert_eq!(col_type("date_col"), "date");
        assert_eq!(col_type("ts_col"), "datetime");
        assert_eq!(col_type("json_col"), "string"); // JSONB → string friendly_type
        assert_eq!(col_type("uuid_col"), "string");
        assert_eq!(col_type("arr_col"), "integer"); // _INT4 → integer

        let row0 = &result.rows[0];
        assert_eq!(row0["bool_col"], json!(true));
        assert_eq!(row0["int_col"], json!(42));
        assert_eq!(row0["text_col"], json!("hello"));
        // Exercises the INT4[] decode path.
        assert_eq!(row0["arr_col"], json!([1, 2, 3]));
    }

    /// `DB_STATEMENT_TIMEOUT_SECS` must reach a real Postgres session: the target DB, not the
    /// worker's timeout, cuts the connection. Owns the env var for the test and restores it.
    #[tokio::test]
    async fn live_statement_timeout_honors_env_override() {
        let _env = crate::test_support::private_net_env(Some("1"));
        let Some(opts) = live_target_opts() else {
            return;
        };
        let previous = std::env::var("DB_STATEMENT_TIMEOUT_SECS").ok();
        std::env::set_var("DB_STATEMENT_TIMEOUT_SECS", "1");

        let connector = PostgresConnector::new();
        let started = std::time::Instant::now();
        let result = connector.run_query(&opts, "SELECT pg_sleep(5)").await;
        let elapsed = started.elapsed();

        match previous {
            Some(v) => std::env::set_var("DB_STATEMENT_TIMEOUT_SECS", v),
            None => std::env::remove_var("DB_STATEMENT_TIMEOUT_SECS"),
        }

        let err = result.expect_err("a 5s query under a 1s DB_STATEMENT_TIMEOUT_SECS must fail");
        assert!(
            elapsed < std::time::Duration::from_secs(4),
            "target DB must cut the query at ~1s (env override), not run it to completion \
             (took {elapsed:?}) — DB_STATEMENT_TIMEOUT_SECS is not reaching the session"
        );
        assert!(
            format!("{err:#}").to_lowercase().contains("canceling")
                || format!("{err:#}").to_lowercase().contains("timeout"),
            "expected a statement-timeout style error from Postgres, got: {err:#}"
        );
    }

    #[test]
    fn friendly_type_maps_known_pg_types() {
        for (pg, want) in [
            ("BOOL", "boolean"),
            ("INT2", "integer"),
            ("INT4", "integer"),
            ("INT8", "integer"),
            ("FLOAT4", "float"),
            ("FLOAT8", "float"),
            ("NUMERIC", "float"),
            ("TIMESTAMP", "datetime"),
            ("TIMESTAMPTZ", "datetime"),
            ("DATE", "date"),
            ("TEXT", "string"),
            ("VARCHAR", "string"),
            ("JSONB", "string"),
            ("UUID", "string"),
        ] {
            assert_eq!(friendly_type_for_pg_type(pg), want, "{pg}");
        }
    }

    #[test]
    fn friendly_type_falls_back_to_string_for_unknown_types() {
        for pg in ["CIDR", "POINT", "MY_CUSTOM_ENUM"] {
            assert_eq!(friendly_type_for_pg_type(pg), "string", "{pg}");
        }
    }

    #[test]
    fn friendly_type_array_maps_to_element_type() {
        for (pg, want) in [
            ("INT4[]", "integer"),
            ("TEXT[]", "string"),
            ("BOOL[]", "boolean"),
            ("FLOAT8[]", "float"),
            ("TIMESTAMPTZ[]", "datetime"),
        ] {
            assert_eq!(friendly_type_for_pg_type(pg), want, "{pg}");
        }
    }

    #[test]
    fn parse_options_applies_defaults() {
        let opts = json!({
            "host": "localhost",
            "user": "postgres",
            "dbname": "mydb"
        });
        let parsed = parse_options(&opts).unwrap();
        assert_eq!(parsed.port, 5432);
        assert_eq!(parsed.sslmode, "prefer");
        assert_eq!(parsed.password, "");
    }

    #[test]
    fn parse_options_respects_overrides() {
        let opts = json!({
            "host": "db.example.com",
            "port": 5433,
            "user": "admin",
            "password": "secret",
            "dbname": "prod",
            "sslmode": "require"
        });
        let parsed = parse_options(&opts).unwrap();
        assert_eq!(parsed.port, 5433);
        assert_eq!(parsed.sslmode, "require");
        assert_eq!(parsed.password, "secret");
    }

    #[test]
    fn parse_options_rejects_missing_required_fields() {
        let opts = json!({ "host": "localhost" });
        assert!(parse_options(&opts).is_err());
    }

    #[test]
    fn connector_type_name_and_schema() {
        let connector = PostgresConnector::new();
        assert_eq!(connector.type_name(), "pg");
        let schema = connector.configuration_schema();
        assert_eq!(schema["required"], json!(["host", "user", "dbname"]));
    }
}
