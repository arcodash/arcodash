//! Amazon Athena connector via `aws-sdk-athena`: `StartQueryExecution` → poll `GetQueryExecution` →
//! paginate `GetQueryResults` (so no `aws-sdk-s3` dependency is needed).
//!
//! Auth uses the static access-key/secret from the data source config, never the ambient AWS
//! credential chain. The default `athena.{region}.amazonaws.com` host is AWS-owned and unguarded; a
//! user-supplied `api_endpoint` is guarded (`guard_url` in `run_athena_sql`) since a signed request
//! to an internal host is still SSRF.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_sdk_athena::config::timeout::TimeoutConfig;
use aws_sdk_athena::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_athena::error::DisplayErrorContext;
use aws_sdk_athena::types::{QueryExecutionContext, QueryExecutionState, ResultConfiguration};
use aws_sdk_athena::{Client, Config};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::http_connectors::{guard_url, validate_identifier};
use super::{
    ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult, QueryRunner, TableSchema,
    CONNECT_TIMEOUT,
};

/// Bound on each individual SDK call (quick metadata operations); the query's own deadline is
/// `query_timeout_secs()`.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

/// `GetQueryExecution` poll cadence: starts at `POLL_INTERVAL_MIN`, grows 1.5x per iteration up to
/// `POLL_INTERVAL_MAX`.
const POLL_INTERVAL_MIN: Duration = Duration::from_millis(500);
const POLL_INTERVAL_MAX: Duration = Duration::from_secs(5);

/// Row cap on `GetQueryResults` pagination, independent of the wall-clock deadline; hardcodes the
/// worker's default `max_query_rows`.
const ATHENA_MAX_ROWS: usize = 10_000;
/// Page-count cap on the same loop, so many small pages can't spin it forever.
const ATHENA_MAX_PAGES: usize = 2000;

fn next_poll_interval(current: Duration) -> Duration {
    current.mul_f64(1.5).min(POLL_INTERVAL_MAX)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AthenaOptions {
    pub aws_access_key: String,
    pub aws_secret_key: String,
    pub region: String,
    /// S3 URI Athena writes results to; required unless the workgroup has its own result
    /// configuration.
    pub s3_output_location: String,
    pub database: String,
    #[serde(default)]
    pub work_group: Option<String>,
    #[serde(default)]
    pub catalog: Option<String>,
    /// Endpoint URL override for an Athena-compatible emulator, passed to the SDK's
    /// `Config::endpoint_url`; unset for real AWS.
    #[serde(default)]
    pub api_endpoint: Option<String>,
}

fn parse_options(opts: &Value) -> Result<AthenaOptions> {
    let opts: AthenaOptions = super::parse_opts(opts, "invalid Athena connection options")?;
    if opts.database.trim().is_empty() {
        anyhow::bail!("invalid Athena connection options: database must not be empty");
    }
    Ok(opts)
}

fn build_client(opts: &AthenaOptions) -> Client {
    let creds = Credentials::new(
        &opts.aws_access_key,
        &opts.aws_secret_key,
        None,
        None,
        "arcodash-athena-datasource",
    );
    let timeout_config = TimeoutConfig::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .operation_timeout(OPERATION_TIMEOUT)
        .build();
    let mut builder = Config::builder()
        .region(Region::new(opts.region.clone()))
        .credentials_provider(creds)
        .behavior_version(BehaviorVersion::latest())
        .timeout_config(timeout_config);
    builder.set_endpoint_url(opts.api_endpoint.clone());
    Client::from_conf(builder.build())
}

/// Maps Athena/Presto/Trino `ColumnInfo::type()` strings to `ColumnMeta::friendly_type` tags, by
/// prefix since parameterized types carry extra text.
fn friendly_type(presto_type: &str) -> &'static str {
    let t = presto_type.to_ascii_lowercase();
    if t.starts_with("tinyint")
        || t.starts_with("smallint")
        || t.starts_with("integer")
        || t.starts_with("bigint")
    {
        "integer"
    } else if t.starts_with("double")
        || t.starts_with("real")
        || t.starts_with("float")
        || t.starts_with("decimal")
    {
        "float"
    } else if t.starts_with("boolean") {
        "boolean"
    } else if t.starts_with("timestamp") {
        "datetime"
    } else if t.starts_with("date") {
        "date"
    } else {
        "string"
    }
}

/// Decode one `Datum::var_char_value()` (every cell arrives as a string, or is omitted for `NULL`)
/// to JSON per its column type.
fn cell_to_json(presto_type: &str, raw: Option<&str>) -> Value {
    let Some(raw) = raw else {
        return Value::Null;
    };
    match friendly_type(presto_type) {
        "integer" => super::parse_or_raw::<i64>(raw),
        "float" if !presto_type.to_ascii_lowercase().starts_with("decimal") => {
            super::parse_or_raw::<f64>(raw)
        }
        "boolean" => super::parse_or_raw::<bool>(raw),
        // Other types stay strings: lossless for decimal, and Athena already returns
        // dates/timestamps as plain text.
        _ => json!(raw),
    }
}

/// Build one result row (JSON object keyed by column name) from a `GetQueryResults` `Row`.
fn row_to_json(
    row: &aws_sdk_athena::types::Row,
    columns: &[ColumnMeta],
    column_types: &[String],
) -> Value {
    let mut obj = serde_json::Map::with_capacity(columns.len());
    for (i, col) in columns.iter().enumerate() {
        let raw = row.data().get(i).and_then(|d| d.var_char_value());
        let ty = column_types.get(i).map_or("varchar", String::as_str);
        obj.insert(col.name.clone(), cell_to_json(ty, raw));
    }
    Value::Object(obj)
}

/// Whether a first-page row is the column-name header (present only for `SELECT`): its values equal
/// the declared column names in order. A real first row equal to the names would also be skipped,
/// the standard heuristic for this API.
fn is_athena_header_row(row: &aws_sdk_athena::types::Row, columns: &[ColumnMeta]) -> bool {
    row.data().len() == columns.len()
        && row
            .data()
            .iter()
            .zip(columns)
            .all(|(d, c)| d.var_char_value() == Some(c.name.as_str()))
}

/// Whether an SDK failure never reached AWS (no HTTP response: DNS, TCP refused, TLS), as opposed
/// to AWS answering with an error; see `connect_err` for why the safe default matters.
const fn is_connect_failure<E, R>(e: &aws_sdk_athena::error::SdkError<E, R>) -> bool {
    matches!(e, aws_sdk_athena::error::SdkError::DispatchFailure(_))
}

/// Renders a failed `operation` call, tagged with `connect_err` when it never reached AWS.
fn sdk_err<E, R>(operation: &str, e: &aws_sdk_athena::error::SdkError<E, R>) -> anyhow::Error
where
    E: std::error::Error + 'static,
    R: std::fmt::Debug,
{
    let msg = anyhow::anyhow!("Athena {operation} failed: {}", DisplayErrorContext(e));
    if is_connect_failure(e) {
        super::connect_err(msg)
    } else {
        msg
    }
}

async fn run_athena_sql(opts: &AthenaOptions, sql: &str) -> Result<(Vec<ColumnMeta>, Vec<Value>)> {
    // Guard a user-supplied `api_endpoint` host (SigV4 with dummy creds against an internal host is
    // still SSRF); all trait methods route through here.
    if let Some(endpoint) = &opts.api_endpoint {
        guard_url(endpoint).await?;
    }
    let client = build_client(opts);

    let start_out = client
        .start_query_execution()
        .query_string(sql)
        .query_execution_context(
            QueryExecutionContext::builder()
                .database(&opts.database)
                .set_catalog(opts.catalog.clone())
                .build(),
        )
        .result_configuration(
            ResultConfiguration::builder()
                .output_location(&opts.s3_output_location)
                .build(),
        )
        .set_work_group(opts.work_group.clone())
        .send()
        .await
        .map_err(|e| sdk_err("StartQueryExecution", &e))?;
    let query_execution_id = start_out
        .query_execution_id()
        .ok_or_else(|| {
            anyhow::anyhow!("Athena StartQueryExecution returned no query_execution_id")
        })?
        .to_string();

    // Wall-clock deadline for the whole start→poll→paginate sequence; the backend's interactive
    // `test_connection`/`get_schema` have no outer timeout wrapper.
    let started_at = Instant::now();
    let timeout_secs = super::query_timeout_secs();
    let deadline = started_at + Duration::from_secs(timeout_secs);
    let timed_out = |query_execution_id: &str| {
        anyhow::anyhow!(
            "Athena query {query_execution_id} timed out after {:.1}s (limit {timeout_secs}s)",
            started_at.elapsed().as_secs_f64()
        )
    };

    let mut poll_interval = POLL_INTERVAL_MIN;
    loop {
        if Instant::now() >= deadline {
            return Err(timed_out(&query_execution_id));
        }
        let exec_out = client
            .get_query_execution()
            .query_execution_id(&query_execution_id)
            .send()
            .await
            .map_err(|e| sdk_err("GetQueryExecution", &e))?;
        let status = exec_out.query_execution().and_then(|q| q.status());
        match status.and_then(|s| s.state()) {
            Some(QueryExecutionState::Succeeded) => break,
            Some(QueryExecutionState::Failed | QueryExecutionState::Cancelled) => {
                let reason = status
                    .and_then(|s| s.state_change_reason())
                    .unwrap_or("unknown reason");
                anyhow::bail!("Athena query failed: {reason}");
            }
            _ => {
                tokio::time::sleep(poll_interval).await;
                poll_interval = next_poll_interval(poll_interval);
            }
        }
    }

    let mut columns: Vec<ColumnMeta> = Vec::new();
    let mut column_types: Vec<String> = Vec::new();
    let mut rows: Vec<Value> = Vec::new();
    let mut next_token: Option<String> = None;
    let mut pages_fetched = 0;

    loop {
        if Instant::now() >= deadline {
            return Err(timed_out(&query_execution_id));
        }
        pages_fetched += 1;
        if pages_fetched > ATHENA_MAX_PAGES {
            anyhow::bail!(
                "Athena result has more pages than this connector will follow ({ATHENA_MAX_PAGES})"
            );
        }
        let out = client
            .get_query_results()
            .query_execution_id(&query_execution_id)
            .set_next_token(next_token.clone())
            .send()
            .await
            .map_err(|e| sdk_err("GetQueryResults", &e))?;
        let result_set = out
            .result_set()
            .ok_or_else(|| anyhow::anyhow!("Athena GetQueryResults returned no result set"))?;

        if columns.is_empty() {
            if let Some(meta) = result_set.result_set_metadata() {
                for col in meta.column_info() {
                    columns.push(ColumnMeta {
                        name: col.name().to_string(),
                        friendly_type: friendly_type(col.r#type()).to_string(),
                    });
                    column_types.push(col.r#type().to_string());
                }
            }
        }

        // Only the first page (no token yet) can carry the header row, and only for `SELECT`;
        // skipping unconditionally would drop data for DESCRIBE/SHOW.
        let is_header = next_token.is_none()
            && result_set
                .rows()
                .first()
                .is_some_and(|r| is_athena_header_row(r, &columns));
        for row in result_set.rows().iter().skip(usize::from(is_header)) {
            rows.push(row_to_json(row, &columns, &column_types));
            if rows.len() > ATHENA_MAX_ROWS {
                anyhow::bail!(
                    "Athena result exceeds this connector's row cap ({ATHENA_MAX_ROWS} rows); \
                     narrow the query"
                );
            }
        }

        next_token = out.next_token().map(str::to_string);
        if next_token.is_none() {
            break;
        }
    }

    Ok((columns, rows))
}

pub struct AthenaConnector;

fn views_sql(database: &str) -> String {
    format!(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = '{database}' AND table_type = 'VIEW' ORDER BY table_name"
    )
}

#[async_trait]
impl QueryRunner for AthenaConnector {
    fn type_name(&self) -> &'static str {
        "athena"
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "aws_access_key": { "type": "string", "title": "AWS Access Key ID" },
                "aws_secret_key": { "type": "string", "title": "AWS Secret Access Key" },
                "region": { "type": "string", "title": "AWS Region", "default": "us-east-1" },
                "s3_output_location": {
                    "type": "string",
                    "title": "S3 Output Location (e.g. s3://bucket/prefix/)"
                },
                "work_group": { "type": "string", "title": "Work Group (optional)" },
                "catalog": {
                    "type": "string",
                    "title": "Athena Data Catalog (optional)",
                    "description": "Leave blank to use Athena's default AwsDataCatalog."
                },
                "database": { "type": "string", "title": "Database" },
                "api_endpoint": {
                    "type": "string",
                    "title": "API Endpoint override (advanced — local emulator testing only)"
                }
            },
            "required": ["aws_access_key", "aws_secret_key", "region", "s3_output_location", "database"],
            "secret": ["aws_secret_key"]
        })
    }

    fn network_targets(&self, opts: &Value) -> Vec<NetworkTarget> {
        parse_options(opts)
            .ok()
            .and_then(|opts| opts.api_endpoint)
            .map(|endpoint| vec![NetworkTarget::Url(endpoint)])
            .unwrap_or_default()
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let opts = parse_options(opts)?;
        run_athena_sql(&opts, "SELECT 1").await.map(|_| ())
    }

    async fn get_view_names(&self, opts: &Value) -> Result<Vec<String>> {
        let opts = parse_options(opts)?;
        validate_identifier(&opts.database, "database")?;
        let (_, rows) = run_athena_sql(&opts, &views_sql(&opts.database))
            .await
            .context("Athena view introspection failed")?;
        Ok(rows
            .iter()
            .filter_map(|r| r["table_name"].as_str().map(String::from))
            .collect())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let opts = parse_options(opts)?;
        validate_identifier(&opts.database, "database")?;
        let sql = format!(
            "SELECT table_name, column_name, data_type FROM information_schema.columns \
             WHERE table_schema = '{}' ORDER BY table_name, ordinal_position",
            opts.database
        );
        let (_, rows) = run_athena_sql(&opts, &sql)
            .await
            .context("Athena schema introspection failed")?;

        let mut tables: Vec<TableSchema> = Vec::new();
        for row in rows {
            let field = |key: &str| row[key].as_str().unwrap_or_default().to_string();
            let column = ColumnSchema::typed_or_blank(field("column_name"), field("data_type"));
            super::push_schema_column(&mut tables, field("table_name"), column);
        }
        Ok(tables)
    }

    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        let opts = parse_options(opts)?;
        let start = Instant::now();
        let (columns, rows) = run_athena_sql(&opts, query).await?;
        Ok(QueryExecutionResult {
            columns,
            rows,
            runtime_seconds: start.elapsed().as_secs_f64(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_sql_scopes_to_database_views() {
        let sql = views_sql("db1");
        assert!(sql.contains("table_schema = 'db1'") && sql.contains("table_type = 'VIEW'"));
    }
    use aws_sdk_athena::types::{ColumnInfo, Datum, ResultSet, ResultSetMetadata, Row};

    /// The poll interval starts at the floor, grows 1.5x per step, and never exceeds the ceiling.
    #[test]
    fn poll_interval_grows_then_caps() {
        let mut interval = POLL_INTERVAL_MIN;
        assert_eq!(interval, Duration::from_millis(500));

        interval = next_poll_interval(interval);
        assert_eq!(interval, Duration::from_millis(750));

        interval = next_poll_interval(interval);
        assert_eq!(interval, Duration::from_millis(1125));

        for _ in 0..20 {
            interval = next_poll_interval(interval);
            assert!(interval <= POLL_INTERVAL_MAX);
        }
        assert_eq!(interval, POLL_INTERVAL_MAX);
    }

    /// The deadline is `started_at + timeout_secs` checked with `>=`; locks the off-by-one cases.
    #[test]
    fn deadline_arithmetic_is_elapsed_or_not_as_expected() {
        let started_at = Instant::now() - Duration::from_secs(301);
        let deadline = started_at + Duration::from_secs(300);
        assert!(
            Instant::now() >= deadline,
            "a 300s deadline started 301s ago must already have passed"
        );

        let started_at = Instant::now();
        let deadline = started_at + Duration::from_secs(300);
        assert!(
            Instant::now() < deadline,
            "a 300s deadline started just now must not have passed yet"
        );
    }

    #[test]
    fn friendly_type_mapping() {
        assert_eq!(friendly_type("bigint"), "integer");
        assert_eq!(friendly_type("integer"), "integer");
        assert_eq!(friendly_type("double"), "float");
        assert_eq!(friendly_type("decimal(10,2)"), "float");
        assert_eq!(friendly_type("boolean"), "boolean");
        assert_eq!(friendly_type("timestamp"), "datetime");
        assert_eq!(friendly_type("date"), "date");
        assert_eq!(friendly_type("varchar(255)"), "string");
    }

    #[test]
    fn cell_to_json_parses_by_declared_type() {
        assert_eq!(cell_to_json("bigint", Some("42")), json!(42));
        assert_eq!(cell_to_json("double", Some("3.5")), json!(3.5));
        assert_eq!(cell_to_json("boolean", Some("true")), json!(true));
        assert_eq!(cell_to_json("varchar(10)", Some("hi")), json!("hi"));
        assert_eq!(cell_to_json("decimal(10,2)", Some("12.34")), json!("12.34"));
        assert_eq!(cell_to_json("bigint", None), Value::Null);
        assert_eq!(cell_to_json("bigint", Some("nope")), json!("nope"));
    }

    fn datum(v: &str) -> Datum {
        Datum::builder().var_char_value(v).build()
    }

    fn row(values: &[&str]) -> Row {
        let mut b = Row::builder();
        for v in values {
            b = b.data(datum(v));
        }
        b.build()
    }

    /// Builds a two-row `ResultSet` as Athena returns it for a SELECT: row 0 is the header.
    fn sample_result_set() -> ResultSet {
        let metadata = ResultSetMetadata::builder()
            .column_info(
                ColumnInfo::builder()
                    .name("id")
                    .r#type("bigint")
                    .build()
                    .unwrap(),
            )
            .column_info(
                ColumnInfo::builder()
                    .name("name")
                    .r#type("varchar(255)")
                    .build()
                    .unwrap(),
            )
            .build();
        ResultSet::builder()
            .result_set_metadata(metadata)
            .rows(row(&["id", "name"]))
            .rows(row(&["1", "a"]))
            .rows(row(&["2", "b"]))
            .build()
    }

    #[test]
    fn first_page_header_row_is_skipped_and_types_are_decoded() {
        let result_set = sample_result_set();
        let meta = result_set.result_set_metadata().unwrap();
        let columns: Vec<ColumnMeta> = meta
            .column_info()
            .iter()
            .map(|c| ColumnMeta {
                name: c.name().to_string(),
                friendly_type: friendly_type(c.r#type()).to_string(),
            })
            .collect();
        let column_types: Vec<String> = meta
            .column_info()
            .iter()
            .map(|c| c.r#type().to_string())
            .collect();

        let mut rows_iter = result_set.rows().iter();
        rows_iter.next(); // simulate the first-page header-row skip
        let rows: Vec<Value> = rows_iter
            .map(|r| row_to_json(r, &columns, &column_types))
            .collect();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["id"], json!(1));
        assert_eq!(rows[0]["name"], json!("a"));
        assert_eq!(rows[1]["id"], json!(2));
    }

    fn columns(names: &[&str]) -> Vec<ColumnMeta> {
        ColumnMeta::all_string(&names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn header_matching_first_row_is_detected_as_header() {
        assert!(is_athena_header_row(
            &row(&["id", "name"]),
            &columns(&["id", "name"])
        ));
    }

    #[test]
    fn non_matching_first_row_is_kept_as_data() {
        assert!(!is_athena_header_row(
            &row(&["1", "a"]),
            &columns(&["id", "name"])
        ));
    }

    #[test]
    fn a_row_with_a_different_length_than_the_columns_is_never_treated_as_header() {
        assert!(!is_athena_header_row(
            &row(&["id", "extra"]),
            &columns(&["id"])
        ));
    }

    #[test]
    fn configuration_schema_shape() {
        assert_eq!(AthenaConnector.type_name(), "athena");
        let schema = AthenaConnector.configuration_schema();
        assert_eq!(
            schema["required"],
            json!([
                "aws_access_key",
                "aws_secret_key",
                "region",
                "s3_output_location",
                "database"
            ])
        );
        assert_eq!(schema["secret"], json!(["aws_secret_key"]));
    }

    #[test]
    fn options_require_a_non_empty_database() {
        let options = json!({
            "aws_access_key": "key",
            "aws_secret_key": "secret",
            "region": "us-east-1",
            "s3_output_location": "s3://results/",
            "database": ""
        });

        assert!(parse_options(&options).is_err());
    }

    /// Mocked with the SDK's own `SdkError` constructors: only `DispatchFailure` (no HTTP response)
    /// classifies as a connect failure; `TimeoutError` and `ServiceError` must not.
    #[test]
    fn is_connect_failure_flags_dispatch_failures_only() {
        use aws_sdk_athena::error::{ConnectorError, SdkError};

        let dispatch = SdkError::<(), ()>::dispatch_failure(ConnectorError::io(Box::new(
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused"),
        )));
        assert!(
            is_connect_failure(&dispatch),
            "a dispatch failure (no HTTP response received) must classify as connect"
        );

        let timeout = SdkError::<(), ()>::timeout_error("request timed out");
        assert!(
            !is_connect_failure(&timeout),
            "a timeout is ambiguous (a response may have started) — must not classify as connect"
        );

        let service = SdkError::<(), ()>::service_error((), ());
        assert!(
            !is_connect_failure(&service),
            "a service error means AWS answered — must not classify as connect"
        );
    }
}
