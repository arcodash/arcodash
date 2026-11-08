//! SQL Server connector via `tiberius` (pure-Rust TDS driver, rustls TLS).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tiberius::{AuthMethod, Client, ColumnType, Config, EncryptionLevel, Row};
use tokio::net::TcpStream;
// `StreamExt::next()` lets `run_query` poll the stream directly instead of `into_results()`, which
// drops empty result sets.
use tokio_stream::StreamExt;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::{
    ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult, QueryRunner, TableSchema,
    CONNECT_TIMEOUT,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MssqlOptions {
    pub server: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub password: String,
    pub db: String,
    /// "off" | "optional" | "required" (default: "required").
    #[serde(default)]
    pub encrypt: Option<String>,
    #[serde(default)]
    pub trust_server_certificate: bool,
    /// Accepted for config-form compatibility only; tiberius has no runtime TDS-version knob, so it
    /// is never read.
    #[serde(default)]
    #[allow(dead_code)]
    pub tds_version: Option<String>,
}

const fn default_port() -> u16 {
    1433
}

/// Row cap for `run_query`: T-SQL has no `LIMIT` (`supports_auto_limit()` is false), so this is the
/// only bound on returned rows.
const MAX_ROWS: usize = 10_000;

pub struct MssqlConnector;

fn parse_options(opts: &Value) -> Result<MssqlOptions> {
    serde_json::from_value(opts.clone()).context("invalid SQL Server connection options")
}

/// Table name as `get_schema` spells it; the default "dbo" schema stays bare.
fn qualified_name(schema: &str, table: &str) -> String {
    if schema == "dbo" {
        table.to_string()
    } else {
        format!("{schema}.{table}")
    }
}

const VIEW_NAMES_SQL: &str = "SELECT TABLE_SCHEMA, TABLE_NAME FROM INFORMATION_SCHEMA.VIEWS \
                              ORDER BY TABLE_SCHEMA, TABLE_NAME";

fn build_config(opts: &MssqlOptions) -> Result<Config> {
    let mut config = Config::new();
    config.host(&opts.server);
    config.port(opts.port);
    config.authentication(AuthMethod::sql_server(&opts.user, &opts.password));
    config.database(&opts.db);
    // tiberius defaults to a 30s per-round-trip timeout; the caller's `query_timeout_secs()` is
    // the only intended bound.
    config.command_timeout(None);

    let level = match opts.encrypt.as_deref().unwrap_or("required") {
        "off" => EncryptionLevel::Off,
        "optional" => EncryptionLevel::On,
        "required" => EncryptionLevel::Required,
        other => {
            anyhow::bail!("invalid 'encrypt' option '{other}': expected off, optional, or required")
        }
    };
    config.encryption(level);

    if opts.trust_server_certificate {
        config.trust_cert();
    }

    Ok(config)
}

/// Connections are one-shot (opened, used once, dropped), no pooling.
async fn connect(opts: &MssqlOptions) -> Result<Client<Compat<TcpStream>>> {
    crate::net_guard::guard_host(&opts.server).await?;
    // tiberius has no query-level timeout, so each call wraps its statement in
    // `tokio::time::timeout`; dropping the future drops the connection, which stops server-side
    // work.
    let config = build_config(opts)?;
    // Two separate timeouts so an unreachable host and a stalled login give distinct errors.
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(config.get_addr()))
        .await
        .map_err(|_| anyhow::anyhow!("timed out connecting to {}", opts.server))?
        .context("failed to connect to SQL Server")?;
    let _ = tcp.set_nodelay(true);
    Box::pin(tokio::time::timeout(
        CONNECT_TIMEOUT,
        Client::connect(config, tcp.compat_write()),
    ))
    .await
    .map_err(|_| anyhow::anyhow!("timed out during SQL Server login to {}", opts.server))?
    .context("SQL Server login failed")
}

/// Runs `sql` on a fresh connection and returns its first result set, bounded by
/// `query_timeout_secs()`; `what` names the operation in errors.
async fn first_result_set(opts: &MssqlOptions, sql: &str, what: &str) -> Result<Vec<Row>> {
    let mut client = Box::pin(connect(opts)).await?;
    let timeout_secs = super::query_timeout_secs();
    tokio::time::timeout(Duration::from_secs(timeout_secs), async move {
        client
            .simple_query(sql)
            .await
            .with_context(|| format!("{what} failed"))?
            .into_first_result()
            .await
            .with_context(|| format!("{what} failed"))
    })
    .await
    .map_err(|_| anyhow::anyhow!("SQL Server {what} timed out after {timeout_secs}s"))?
}

/// A string column's value, with SQL `NULL` read as empty.
fn str_col<'r>(row: &'r Row, col: &str) -> Result<&'r str> {
    Ok(row.try_get::<&str, _>(col)?.unwrap_or_default())
}

const fn friendly_type(ct: ColumnType) -> &'static str {
    use ColumnType::{
        Bit, Bitn, Daten, Datetime, Datetime2, Datetime4, DatetimeOffsetn, Datetimen, Decimaln,
        Float4, Float8, Int1, Int2, Int4, Int8, Money, Money4, Numericn,
    };
    match ct {
        Bit | Bitn => "boolean",
        Int1 | Int2 | Int4 | Int8 => "integer",
        Float4 | Float8 | Money | Money4 | Decimaln | Numericn => "float",
        Daten => "date",
        Datetime | Datetime4 | Datetimen | Datetime2 | DatetimeOffsetn => "datetime",
        _ => "string",
    }
}

/// Decode one cell to JSON by TDS type; bails (never panics) on unsupported types.
fn cell_to_json(row: &Row, idx: usize, ct: ColumnType) -> Result<Value> {
    use ColumnType::{
        BigChar, BigVarChar, Bit, Bitn, Daten, Datetime, Datetime2, Datetime4, DatetimeOffsetn,
        Datetimen, Decimaln, Float4, Float8, Guid, Int1, Int2, Int4, Int8, Money, Money4, NChar,
        NText, NVarchar, Null, Numericn, Text, Timen,
    };
    // `json!` of an `Option` is `null` for SQL NULL.
    let value = match ct {
        Null => Value::Null,
        Bit | Bitn => json!(row.try_get::<bool, _>(idx)?),
        Int1 => json!(row.try_get::<u8, _>(idx)?),
        Int2 => json!(row.try_get::<i16, _>(idx)?),
        Int4 => json!(row.try_get::<i32, _>(idx)?),
        Int8 => json!(row.try_get::<i64, _>(idx)?),
        Float4 => json!(row.try_get::<f32, _>(idx)?),
        // tiberius decodes both MONEY and SMALLMONEY (Money4) to F64 — never F32.
        Float8 | Money | Money4 => json!(row.try_get::<f64, _>(idx)?),
        // Decimal/numeric stay strings for lossless precision.
        Decimaln | Numericn => json!(row
            .try_get::<tiberius::numeric::Numeric, _>(idx)?
            .map(|n| n.to_string())),
        Guid => json!(row.try_get::<uuid::Uuid, _>(idx)?.map(|u| u.to_string())),
        Datetime | Datetime4 | Datetimen | Datetime2 => json!(row
            .try_get::<chrono::NaiveDateTime, _>(idx)?
            .map(|dt| dt.to_string())),
        Daten => json!(row
            .try_get::<chrono::NaiveDate, _>(idx)?
            .map(|d| d.to_string())),
        Timen => json!(row
            .try_get::<chrono::NaiveTime, _>(idx)?
            .map(|t| t.to_string())),
        DatetimeOffsetn => json!(row
            .try_get::<chrono::DateTime<chrono::Utc>, _>(idx)?
            .map(|dt| dt.to_rfc3339())),
        BigVarChar | BigChar | NVarchar | NChar | Text | NText => {
            json!(row.try_get::<&str, _>(idx)?)
        }
        other => anyhow::bail!(
            "unsupported SQL Server column type {other:?} — binary/xml/udt/sql_variant columns \
             are not decoded by this connector"
        ),
    };
    Ok(value)
}

fn row_to_json(row: &Row, names: &[String]) -> Result<Value> {
    let cells = row.columns().iter().enumerate();
    cells
        .map(|(idx, col)| {
            Ok((
                names[idx].clone(),
                cell_to_json(row, idx, col.column_type())?,
            ))
        })
        .collect::<Result<_>>()
        .map(Value::Object)
}

#[async_trait]
impl QueryRunner for MssqlConnector {
    fn type_name(&self) -> &'static str {
        "mssql"
    }

    // T-SQL uses TOP/OFFSET-FETCH, not LIMIT.
    fn supports_auto_limit(&self) -> bool {
        false
    }

    fn network_targets(&self, opts: &Value) -> Vec<NetworkTarget> {
        parse_options(opts)
            .map(|opts| vec![NetworkTarget::Host(opts.server)])
            .unwrap_or_default()
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "title": "Server" },
                "port": { "type": "integer", "title": "Port", "default": 1433 },
                "user": { "type": "string", "title": "Login" },
                "password": { "type": "string", "title": "Password" },
                "db": { "type": "string", "title": "Database" },
                "encrypt": {
                    "type": "string",
                    "title": "Encrypt",
                    "enum": ["off", "optional", "required"],
                    "default": "required"
                },
                "trust_server_certificate": {
                    "type": "boolean",
                    "title": "Trust Server Certificate",
                    "default": false
                },
                "tds_version": { "type": "string", "title": "TDS Version (informational)" }
            },
            "required": ["server", "user", "db"],
            "secret": ["password"]
        })
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let opts = parse_options(opts)?;
        Box::pin(first_result_set(&opts, "SELECT 1", "test query")).await?;
        Ok(())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let opts = parse_options(opts)?;
        let rows = Box::pin(first_result_set(
            &opts,
            "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, DATA_TYPE \
             FROM INFORMATION_SCHEMA.COLUMNS \
             ORDER BY TABLE_SCHEMA, TABLE_NAME, ORDINAL_POSITION",
            "schema introspection",
        ))
        .await?;

        let mut tables: Vec<TableSchema> = Vec::new();
        for row in &rows {
            let full_name =
                qualified_name(str_col(row, "TABLE_SCHEMA")?, str_col(row, "TABLE_NAME")?);
            let column = ColumnSchema::typed_or_blank(
                str_col(row, "COLUMN_NAME")?,
                str_col(row, "DATA_TYPE")?,
            );
            super::push_schema_column(&mut tables, full_name, column);
        }
        Ok(tables)
    }

    async fn get_view_names(&self, opts: &Value) -> Result<Vec<String>> {
        let opts = parse_options(opts)?;
        let rows = Box::pin(first_result_set(
            &opts,
            VIEW_NAMES_SQL,
            "view introspection",
        ))
        .await?;
        rows.iter()
            .map(|row| {
                Ok(qualified_name(
                    str_col(row, "TABLE_SCHEMA")?,
                    str_col(row, "TABLE_NAME")?,
                ))
            })
            .collect()
    }

    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        let opts = parse_options(opts)?;
        let mut client = Box::pin(connect(&opts)).await.map_err(super::connect_err)?;
        let start = Instant::now();
        let timeout_secs = super::query_timeout_secs();

        // `client` moves into this future so a timeout drops the in-flight stream and closes the
        // connection, stopping server-side work. Not `into_results()`: tiberius 0.12.3 silently
        // drops empty (zero-row) result sets from it; walking the stream and resetting on every
        // `Metadata` event tracks the last statement correctly.
        let drain = async move {
            let mut stream = client.simple_query(query).await.context("query failed")?;
            let mut total = 0usize;
            let mut rows: Vec<Row> = Vec::new();
            while let Some(item) = stream.next().await {
                match item.context("query failed")? {
                    tiberius::QueryItem::Metadata(meta) => {
                        total = meta.result_index() + 1;
                        rows.clear();
                    }
                    tiberius::QueryItem::Row(row) => {
                        if rows.len() >= MAX_ROWS {
                            anyhow::bail!(
                                "SQL Server result exceeds this connector's row cap \
                                 ({MAX_ROWS} rows); narrow the query"
                            );
                        }
                        rows.push(row);
                    }
                }
            }
            Ok::<(usize, Vec<Row>), anyhow::Error>((total, rows))
        };
        let (total, rows) = tokio::time::timeout(Duration::from_secs(timeout_secs), drain)
            .await
            .map_err(|_| anyhow::anyhow!("SQL Server query timed out after {timeout_secs}s"))??;
        let runtime_seconds = start.elapsed().as_secs_f64();

        // T-SQL batches are idiomatic: the last statement's result is returned.
        // `QueryExecutionResult` can't report discarded sets, so log a warning.
        if total > 1 {
            tracing::warn!(
                total,
                "SQL Server batch query returned {total} result sets; only the last is returned, \
                 earlier ones were discarded"
            );
        }

        let first_cols = rows.first().map(tiberius::Row::columns).unwrap_or_default();
        let mut names: Vec<String> = first_cols.iter().map(|c| c.name().to_string()).collect();
        super::dedupe_column_names(&mut names);
        let types = first_cols.iter().map(|c| friendly_type(c.column_type()));
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
    fn view_names_sql_reads_views_and_names_follow_schema_spelling() {
        assert!(VIEW_NAMES_SQL.contains("INFORMATION_SCHEMA.VIEWS"));
        assert_eq!(qualified_name("dbo", "v"), "v");
        assert_eq!(qualified_name("sales", "v"), "sales.v");
    }

    /// `192.0.2.1` (RFC 5737 TEST-NET-1) black-holes like a firewalled host, proving
    /// `CONNECT_TIMEOUT` is applied. Needs `ARCODASH_ALLOW_PRIVATE_CONNECTORS=1` to match how
    /// `scripts/test-connectors.sh` runs connector tests.
    #[tokio::test]
    async fn live_connect_to_a_black_holed_address_times_out_well_under_the_os_default() {
        let _env = crate::test_support::private_net_env(Some("1"));
        let connector = MssqlConnector;
        let opts = json!({
            "server": "192.0.2.1",
            "port": 1433,
            "user": "nope",
            "password": "nope",
            "db": "nope",
            "encrypt": "off"
        });
        let started = std::time::Instant::now();
        let err = connector
            .test_connection(&opts)
            .await
            .expect_err("192.0.2.1 must never answer as SQL Server");
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "expected CONNECT_TIMEOUT (5s) to cut this off, took {elapsed:?} instead"
        );
        assert!(
            format!("{err:#}").to_lowercase().contains("timed out"),
            "expected a timeout error, got: {err:#}"
        );
    }

    #[test]
    fn build_config_disables_driver_command_timeout() {
        let opts: MssqlOptions =
            serde_json::from_value(json!({"server": "h", "user": "u", "db": "d"})).unwrap();
        assert_eq!(build_config(&opts).unwrap().get_command_timeout(), None);
    }

    #[test]
    fn friendly_type_maps_known_mssql_types() {
        assert_eq!(friendly_type(ColumnType::Int4), "integer");
        assert_eq!(friendly_type(ColumnType::Decimaln), "float");
        assert_eq!(friendly_type(ColumnType::Bitn), "boolean");
        assert_eq!(friendly_type(ColumnType::Daten), "date");
        assert_eq!(friendly_type(ColumnType::Datetime2), "datetime");
        assert_eq!(friendly_type(ColumnType::NVarchar), "string");
    }
}
