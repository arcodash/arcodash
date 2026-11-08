//! MySQL / MariaDB / `TiDB` connector via sqlx.

use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::mysql::{MySqlConnectOptions, MySqlConnection, MySqlRow};
use sqlx::{Column, Connection, Row, TypeInfo, ValueRef};

use super::{
    ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult, QueryRunner, TableSchema,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MySqlOptions {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub password: String,
    pub db: String,
}

const fn default_port() -> u16 {
    3306
}

pub struct MySqlConnector {
    type_name: &'static str,
}

impl MySqlConnector {
    #[must_use]
    pub const fn mysql() -> Self {
        Self { type_name: "mysql" }
    }

    #[must_use]
    pub const fn mariadb() -> Self {
        Self {
            type_name: "mariadb",
        }
    }

    #[must_use]
    pub const fn tidb() -> Self {
        Self { type_name: "tidb" }
    }
}

fn parse_options(opts: &Value) -> Result<MySqlOptions> {
    super::parse_opts(opts, "invalid MySQL connection options")
}

async fn connect(opts: &MySqlOptions) -> Result<MySqlConnection> {
    crate::net_guard::guard_host(&opts.host).await?;
    let connect_opts = MySqlConnectOptions::new()
        .host(&opts.host)
        .port(opts.port)
        .username(&opts.user)
        .password(&opts.password)
        .database(&opts.db);
    let mut conn = super::connect_within(
        "failed to connect to MySQL",
        || format!("timed out connecting to {}:{}", opts.host, opts.port),
        MySqlConnection::connect_with(&connect_opts),
    )
    .await?;

    // DB-side backstop (milliseconds); see `connectors::db_statement_timeout_secs` docs — the
    // worker's `tokio::time::timeout` is the authoritative enforcement. Best-effort and
    // non-fatal: MariaDB uses `max_statement_time` instead of MySQL/TiDB's
    // `max_execution_time`, and this same `connect()` helper serves all three flavors, so a
    // failure here (unknown system variable) is swallowed rather than breaking MariaDB connects.
    let timeout_ms = super::db_statement_timeout_secs() * 1000;
    if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET SESSION max_execution_time = {timeout_ms}"
    )))
    .execute(&mut conn)
    .await
    {
        tracing::debug!(error = %e, "failed to set MySQL max_execution_time (ignored, best-effort)");
    }

    Ok(conn)
}

fn friendly_type(mysql_type: &str) -> &'static str {
    let t = mysql_type.to_uppercase();
    let t = t.trim();
    // MySQL's idiomatic boolean is `TINYINT(1)`, indistinguishable from a real one-byte integer
    // column once the `(1)` precision is stripped below — so check for it before truncating.
    if t.starts_with("TINYINT(1)") {
        return "boolean";
    }
    let base = t.split('(').next().unwrap_or(t).trim();
    match base {
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "BIGINT"
        | "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED"
        | "BIGINT UNSIGNED" | "YEAR" | "BIT" => "integer",
        "FLOAT" | "DOUBLE" | "DECIMAL" | "NUMERIC" | "REAL" => "float",
        "BOOL" | "BOOLEAN" => "boolean",
        "DATE" => "date",
        "DATETIME" | "TIMESTAMP" => "datetime",
        _ => "string",
    }
}

/// `<blob N bytes>` placeholder for binary column types this connector doesn't decode further —
/// matches `query_results.rs::sqlite_value_to_json`'s shape for SQLite `Blob` values so both
/// connectors render binary cells the same way.
fn blob_placeholder(label: &str, bytes: &[u8]) -> Value {
    json!(format!("<{label} {} bytes>", bytes.len()))
}

fn row_to_json(row: &MySqlRow, names: &[String]) -> Result<Value> {
    use sqlx::types::chrono::{NaiveDate, NaiveDateTime, NaiveTime};
    let mut obj = serde_json::Map::with_capacity(row.columns().len());
    for (i, col) in row.columns().iter().enumerate() {
        let raw = row.try_get_raw(i)?;
        let val = if raw.is_null() {
            Value::Null
        } else {
            let type_name = col.type_info().name().to_uppercase();
            let base = type_name.split('(').next().unwrap_or(&type_name).trim();
            match base {
                "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" => {
                    json!(row.try_get::<i32, _>(i)?)
                }
                "BIGINT" => json!(row.try_get::<i64, _>(i)?),
                "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED"
                | "INT UNSIGNED" => json!(row.try_get::<u32, _>(i)?),
                "BIGINT UNSIGNED" => json!(row.try_get::<u64, _>(i)?),
                "FLOAT" => json!(row.try_get::<f32, _>(i)?),
                "DOUBLE" | "REAL" => json!(row.try_get::<f64, _>(i)?),
                // sqlx's `String` decode isn't type-compatible with MySQL's DECIMAL wire type
                // (only text/blob/enum column types are) — a checked `try_get` here would fail
                // every query touching a DECIMAL column. Decoded as a string, not `Decimal`, to
                // avoid f64 precision loss on arbitrary-precision values.
                // SET/ENUM decode the same way.
                "DECIMAL" | "NUMERIC" | "SET" | "ENUM" => {
                    json!(row.try_get_unchecked::<String, _>(i)?)
                }
                "BOOL" | "BOOLEAN" => json!(row.try_get::<bool, _>(i)?),
                "DATE" => json!(row.try_get::<NaiveDate, _>(i)?.to_string()),
                // Same zone-less ISO-8601 shape as postgres.rs's `TIMESTAMP` arm, so a value
                // that happens to match across the two connectors round-trips identically.
                // MySQL's TIMESTAMP is zone-aware server-side (stored UTC, returned in the
                // session zone) unlike Postgres's — only the formatting of the naive value the
                // driver hands back is unified here, not that underlying semantic. Unchecked:
                // `NaiveDateTime`'s `Type` impl only declares itself compatible with
                // `ColumnType::Datetime`, so a checked get would reject an actual TIMESTAMP
                // column even though the same decode works fine on its wire bytes.
                "DATETIME" | "TIMESTAMP" => {
                    json!(row
                        .try_get_unchecked::<NaiveDateTime, _>(i)?
                        .format("%Y-%m-%dT%H:%M:%S%.f")
                        .to_string())
                }
                "TIME" => json!(row.try_get::<NaiveTime, _>(i)?.to_string()),
                // sqlx doesn't check YEAR's reported UNSIGNED flag consistently across
                // MySQL/MariaDB — decode unchecked rather than risk a spurious type mismatch.
                "YEAR" => json!(row.try_get_unchecked::<u16, _>(i)?),
                // sqlx's BIT decode already packs the raw bit string into a u64 regardless of
                // the column's declared signedness; MySQL caps BIT at 64 bits so this never
                // truncates.
                "BIT" => json!(row.try_get_unchecked::<u64, _>(i)?),
                "JSON" => row.try_get::<Value, _>(i)?,
                "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "VARBINARY" | "BINARY" => {
                    blob_placeholder("blob", &row.try_get::<Vec<u8>, _>(i)?)
                }
                "GEOMETRY" => {
                    blob_placeholder("geometry", &row.try_get_unchecked::<Vec<u8>, _>(i)?)
                }
                // Any type not special-cased above degrades to its string representation
                // rather than failing the whole query — one unsupported column must never take
                // down an otherwise-good report.
                _ => row.try_get::<String, _>(i).map_or_else(
                    |_| {
                        tracing::debug!(
                            column = names[i].as_str(),
                            mysql_type = type_name.as_str(),
                            "undecodable cell degraded to null"
                        );
                        Value::Null
                    },
                    |s| json!(s),
                ),
            }
        };
        obj.insert(names[i].clone(), val);
    }
    Ok(Value::Object(obj))
}

#[async_trait]
impl QueryRunner for MySqlConnector {
    fn type_name(&self) -> &'static str {
        self.type_name
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "title": "Host" },
                "port": { "type": "integer", "title": "Port", "default": 3306 },
                "user": { "type": "string", "title": "User" },
                "password": { "type": "string", "title": "Password" },
                "db": { "type": "string", "title": "Database Name" }
            },
            "required": ["host", "user", "db"],
            "secret": ["password"]
        })
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let opts = parse_options(opts)?;
        let mut conn = connect(&opts).await?;
        sqlx::query("SELECT 1")
            .execute(&mut conn)
            .await
            .context("test failed")?;
        Ok(())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let opts = parse_options(opts)?;
        let mut conn = connect(&opts).await?;
        let rows = sqlx::query(
            "SELECT CAST(table_name AS CHAR) AS tbl, CAST(column_name AS CHAR) AS col, \
             CAST(data_type AS CHAR) AS typ \
             FROM information_schema.columns \
             WHERE table_schema = DATABASE() \
             ORDER BY table_name, ordinal_position",
        )
        .fetch_all(&mut conn)
        .await?;
        let mut tables: Vec<TableSchema> = Vec::new();
        for row in rows {
            let table: String = row.try_get("tbl")?;
            let column = ColumnSchema::typed(
                row.try_get::<String, _>("col")?,
                row.try_get::<String, _>("typ")?,
            );
            super::push_schema_column(&mut tables, table, column);
        }
        Ok(tables)
    }

    async fn get_view_names(&self, opts: &Value) -> Result<Vec<String>> {
        let opts = parse_options(opts)?;
        let mut conn = connect(&opts).await?;
        let rows = sqlx::query(
            "SELECT CAST(table_name AS CHAR) AS name FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_type = 'VIEW' ORDER BY table_name",
        )
        .fetch_all(&mut conn)
        .await?;
        rows.iter().map(|r| Ok(r.try_get("name")?)).collect()
    }

    async fn get_foreign_keys(&self, opts: &Value) -> Result<Vec<super::ForeignKey>> {
        let opts = parse_options(opts)?;
        let mut conn = connect(&opts).await?;
        let rows = sqlx::query(
            "SELECT CAST(table_name AS CHAR) AS from_table, CAST(column_name AS CHAR) AS from_column, \
             CAST(referenced_table_name AS CHAR) AS to_table, \
             CAST(referenced_column_name AS CHAR) AS to_column \
             FROM information_schema.key_column_usage \
             WHERE table_schema = DATABASE() AND referenced_table_name IS NOT NULL \
             ORDER BY table_name, constraint_name, ordinal_position",
        )
        .fetch_all(&mut conn)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(super::ForeignKey {
                    from_table: r.try_get("from_table")?,
                    from_column: r.try_get("from_column")?,
                    to_table: r.try_get("to_table")?,
                    to_column: r.try_get("to_column")?,
                })
            })
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
        let start = Instant::now();
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .fetch_all(&mut conn)
            .await
            .context("query failed")?;
        let runtime_seconds = start.elapsed().as_secs_f64();
        let first_cols = rows.first().map(sqlx::Row::columns).unwrap_or_default();
        let mut names: Vec<String> = first_cols.iter().map(|c| c.name().to_string()).collect();
        super::dedupe_column_names(&mut names);
        let types = first_cols
            .iter()
            .map(|c| friendly_type(c.type_info().name()));
        let columns = ColumnMeta::zip_types(&names, types);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_type_maps_tinyint_1_to_boolean() {
        assert_eq!(friendly_type("tinyint(1)"), "boolean");
    }

    #[test]
    fn friendly_type_maps_other_tinyint_widths_to_integer() {
        assert_eq!(friendly_type("tinyint(4)"), "integer");
        assert_eq!(friendly_type("tinyint"), "integer");
    }

    #[test]
    fn friendly_type_maps_unsigned_tinyint_1_to_boolean() {
        assert_eq!(friendly_type("TINYINT(1) UNSIGNED"), "boolean");
    }

    /// Options for the live MySQL 8.4 (docker-compose.testing.yml, port 5500), or `None` (after
    /// logging a skip) unless `TEST_MYSQL_HOST` is set.
    fn live_opts() -> Option<Value> {
        let Ok(host) = std::env::var("TEST_MYSQL_HOST") else {
            eprintln!("[SKIP] TEST_MYSQL_HOST not set — skipping live MySQL test");
            return None;
        };
        let port: u16 = std::env::var("TEST_MYSQL_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(5500);
        Some(
            json!({"host": host, "port": port, "user": "test", "password": "test", "db": "testdb"}),
        )
    }

    #[tokio::test]
    async fn live_get_foreign_keys_reports_declared_pairs() {
        let _env = crate::test_support::private_net_env(Some("1"));
        let Some(opts) = live_opts() else { return };
        let c = MySqlConnector::mysql();
        for sql in [
            "DROP TABLE IF EXISTS fk_probe_child",
            "DROP TABLE IF EXISTS fk_probe_parent",
            "CREATE TABLE fk_probe_parent (id INT PRIMARY KEY)",
            "CREATE TABLE fk_probe_child (id INT PRIMARY KEY, parent_id INT, \
             FOREIGN KEY (parent_id) REFERENCES fk_probe_parent (id))",
        ] {
            c.run_query(&opts, sql).await.expect(sql);
        }
        let fks = c.get_foreign_keys(&opts).await;
        c.run_query(&opts, "DROP TABLE fk_probe_child")
            .await
            .unwrap();
        c.run_query(&opts, "DROP TABLE fk_probe_parent")
            .await
            .unwrap();
        let want = super::super::ForeignKey {
            from_table: "fk_probe_child".into(),
            from_column: "parent_id".into(),
            to_table: "fk_probe_parent".into(),
            to_column: "id".into(),
        };
        assert!(fks.unwrap().contains(&want));
    }

    /// Live MySQL 8.4 (see above): a view is listed, its base table is not.
    #[tokio::test]
    async fn live_get_view_names_lists_views_not_tables() {
        let _env = crate::test_support::private_net_env(Some("1"));
        let Some(opts) = live_opts() else { return };
        let c = MySqlConnector::mysql();
        for sql in [
            "DROP VIEW IF EXISTS vn_probe_view",
            "DROP TABLE IF EXISTS vn_probe_table",
            "CREATE TABLE vn_probe_table (id INT PRIMARY KEY)",
            "CREATE VIEW vn_probe_view AS SELECT id FROM vn_probe_table",
        ] {
            c.run_query(&opts, sql).await.expect(sql);
        }
        let names = c.get_view_names(&opts).await;
        c.run_query(&opts, "DROP VIEW vn_probe_view").await.unwrap();
        c.run_query(&opts, "DROP TABLE vn_probe_table")
            .await
            .unwrap();
        let names = names.unwrap();
        assert!(names.contains(&"vn_probe_view".to_string()));
        assert!(!names.contains(&"vn_probe_table".to_string()));
    }

    /// A direct connect failure (ECONNREFUSED, nothing listening) must surface as the real
    /// cause, not sqlx's generic "pool timed out". Binds then drops a listener to get a port
    /// nothing is listening on.
    #[tokio::test]
    async fn live_run_query_against_closed_port_surfaces_real_connect_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let _env = crate::test_support::private_net_env(Some("1"));
        let connector = MySqlConnector::mysql();
        let opts = json!({
            "host": "127.0.0.1",
            "port": port,
            "user": "nope",
            "password": "nope",
            "db": "nope"
        });
        let err = connector
            .run_query(&opts, "SELECT 1")
            .await
            .expect_err("closed local port must not be a live MySQL — connection must fail");

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
}
