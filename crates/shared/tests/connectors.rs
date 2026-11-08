//! Integration tests for every connector. Each module skips with [SKIP] when its env var is unset;
//! run all via scripts/test-connectors.sh.

use arcodash_core::connectors::{
    apply_auto_limit,
    athena::AthenaConnector,
    bigquery::BigQueryConnector,
    clickhouse::ClickHouseConnector,
    elasticsearch::ElasticsearchConnector,
    http_connectors::{InfluxDbConnector, JsonConnector},
    mongodb_connector::MongoConnector,
    mssql::MssqlConnector,
    mysql::MySqlConnector,
    postgres::PostgresConnector,
    postgres_aliases::{CockroachDbConnector, RedshiftConnector},
    snowflake::SnowflakeConnector,
    QueryRunner,
};
use serde_json::{json, Value};

/// Parse a postgres URL into connector options JSON.
fn pg_opts_from_url(url: &str) -> Option<serde_json::Value> {
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

/// Parse a mysql://user:pass@host:port/db URL into connector options JSON.
fn mysql_opts_from_url(url: &str) -> Option<serde_json::Value> {
    let rest = url.strip_prefix("mysql://")?;
    let (userinfo, hostpath) = rest.split_once('@')?;
    let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let (hostport, db) = hostpath.split_once('/')?;
    let (host, port_str) = hostport.split_once(':').unwrap_or((hostport, "3306"));
    let port: u16 = port_str.parse().unwrap_or(3306);
    Some(json!({
        "host": host,
        "port": port,
        "user": user,
        "password": password,
        "db": db
    }))
}

/// Parse a mssql:// URL into connector options JSON; always trusts the (self-signed) server cert.
fn mssql_opts_from_url(url: &str) -> Option<serde_json::Value> {
    let rest = url.strip_prefix("mssql://")?;
    let (userinfo, hostpath) = rest.split_once('@')?;
    let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let (hostport, db) = hostpath.split_once('/')?;
    let (host, port_str) = hostport.split_once(':').unwrap_or((hostport, "1433"));
    let port: u16 = port_str.parse().unwrap_or(1433);
    Some(json!({
        "server": host,
        "port": port,
        "user": user,
        "password": password,
        "db": db,
        "encrypt": "required",
        "trust_server_certificate": true
    }))
}

/// Parse http://user:pass@host:port/db into ClickHouse options JSON.
fn ch_opts_from_url(url: &str) -> Option<serde_json::Value> {
    let (use_https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else {
        (false, url.strip_prefix("http://")?)
    };
    let (userinfo_host, db) = rest.split_once('/')?;
    let (userinfo, hostport) = if userinfo_host.contains('@') {
        let (u, h) = userinfo_host.split_once('@')?;
        (u, h)
    } else {
        ("", userinfo_host)
    };
    let (user, password) = if userinfo.contains(':') {
        let (u, p) = userinfo.split_once(':')?;
        (u, p)
    } else {
        (userinfo, "")
    };
    let (host, port_str) = hostport.split_once(':').unwrap_or((hostport, "8123"));
    let port: u16 = port_str.parse().unwrap_or(8123);
    Some(json!({
        "host": host,
        "port": port,
        "user": user,
        "password": password,
        "db": db,
        "use_https": use_https
    }))
}

/// Parse mongodb://user:pass@host:port/db into MongoOptions JSON.
fn mongo_opts_from_url(url: &str) -> Option<serde_json::Value> {
    let rest = url.strip_prefix("mongodb://")?;
    let (userinfo, hostpath) = rest.split_once('@')?;
    let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let (hostport, db) = hostpath.split_once('/')?;
    let (host, port_str) = hostport.split_once(':').unwrap_or((hostport, "27017"));
    let port: u16 = port_str.parse().unwrap_or(27017);
    Some(json!({
        "host": host,
        "port": port,
        "user": user,
        "password": password,
        "db": db,
        "auth_source": "admin"
    }))
}

/// Parse http://host:port into ES options JSON.
fn es_opts_from_url(url: &str) -> Option<serde_json::Value> {
    let (use_https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else {
        (false, url.strip_prefix("http://")?)
    };
    let (host, port_str) = rest.split_once(':').unwrap_or((rest, "9200"));
    let port: u16 = port_str.parse().unwrap_or(9200);
    Some(json!({
        "host": host,
        "port": port,
        "use_https": use_https
    }))
}

/// Parse http://host:port into InfluxDB options JSON using TEST_INFLUX_DB for the db name.
fn influx_opts_from_url(url: &str, db: &str) -> Option<serde_json::Value> {
    let (use_https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else {
        (false, url.strip_prefix("http://")?)
    };
    let (host, port_str) = rest.split_once(':').unwrap_or((rest, "8086"));
    let port: u16 = port_str.parse().unwrap_or(8086);
    Some(json!({
        "host": host,
        "port": port,
        "db": db,
        "use_https": use_https
    }))
}

mod pg {
    use super::*;

    fn opts() -> Option<serde_json::Value> {
        let url = std::env::var("TEST_PG_URL").ok()?;
        pg_opts_from_url(&url)
    }

    #[tokio::test]
    async fn pg_test_connection_ok() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_PG_URL not set");
            return;
        };
        PostgresConnector::new()
            .test_connection(&opts)
            .await
            .expect("pg test_connection failed");
    }

    #[tokio::test]
    async fn pg_get_schema_lists_conn_test() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_PG_URL not set");
            return;
        };
        let schema = PostgresConnector::new()
            .get_schema(&opts)
            .await
            .expect("pg get_schema failed");
        assert!(!schema.is_empty(), "expected at least one table");
        let t = schema
            .iter()
            .find(|t| t.name == "conn_test")
            .expect("conn_test table not found in schema");
        let name_col = t.columns.iter().find(|c| c.name == "name").unwrap();
        assert_eq!(name_col.type_name.as_deref(), Some("text"));
        let n_col = t.columns.iter().find(|c| c.name == "n").unwrap();
        assert_eq!(n_col.type_name.as_deref(), Some("integer"));
    }

    #[tokio::test]
    async fn pg_run_query_returns_rows() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_PG_URL not set");
            return;
        };
        let result = PostgresConnector::new()
            .run_query(&opts, "SELECT id, name, n FROM conn_test ORDER BY id")
            .await
            .expect("pg run_query failed");
        assert!(
            result.rows.len() >= 2,
            "expected >= 2 rows, got {}",
            result.rows.len()
        );
        assert!(
            result.columns.iter().any(|c| c.name == "name"),
            "expected 'name' column"
        );
    }

    #[tokio::test]
    async fn pg_run_query_with_trailing_comment_is_capped_not_unbounded() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_PG_URL not set");
            return;
        };
        // Without comment-aware scanning the LIMIT lands inside the `--` comment and the query runs
        // uncapped; conn_test has 2 rows, so max_rows=0 detects that.
        let sql = apply_auto_limit("SELECT * FROM conn_test -- trailing", 0);
        let result = PostgresConnector::new()
            .run_query(&opts, &sql)
            .await
            .expect("pg run_query with trailing comment failed");
        assert_eq!(
            result.rows.len(),
            1,
            "LIMIT 1 must have capped the 2-row conn_test table, got {} rows",
            result.rows.len()
        );
    }

    /// `timestamp without time zone` must come back as seeded, with no fabricated offset.
    #[tokio::test]
    async fn pg_timestamp_without_tz_has_no_fabricated_offset() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_PG_URL not set");
            return;
        };
        let result = PostgresConnector::new()
            .run_query(&opts, "SELECT ts_col FROM conn_types WHERE id = 1")
            .await
            .expect("pg run_query on conn_types failed");
        let value = result.rows[0]["ts_col"]
            .as_str()
            .expect("ts_col not a string")
            .to_string();
        assert_eq!(
            value, "2026-08-14T09:00:00",
            "a zone-less timestamp must round-trip as the bare wall-clock value, no offset"
        );
        assert!(
            !value.contains('+') && !value.ends_with('Z'),
            "timestamp without time zone must carry no offset suffix, got: {value}"
        );
    }

    /// `timestamptz` keeps its offset; only zone-less `TIMESTAMP` drops it.
    #[tokio::test]
    async fn pg_timestamptz_still_has_an_offset() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_PG_URL not set");
            return;
        };
        let result = PostgresConnector::new()
            .run_query(&opts, "SELECT tstz_col FROM conn_types WHERE id = 1")
            .await
            .expect("pg run_query on conn_types failed");
        let value = result.rows[0]["tstz_col"]
            .as_str()
            .expect("tstz_col not a string")
            .to_string();
        assert!(
            value.contains('+') || value.ends_with('Z'),
            "timestamptz must keep its offset, got: {value}"
        );
    }

    /// Duplicate column names must not collapse to one JSON key.
    #[tokio::test]
    async fn pg_duplicate_column_names_get_deduped() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_PG_URL not set");
            return;
        };
        let result = PostgresConnector::new()
            .run_query(&opts, "SELECT 1 AS id, 2 AS id")
            .await
            .expect("pg run_query with duplicate column names failed");
        assert_eq!(result.columns.len(), 2, "columns: {:?}", result.columns);
        assert_eq!(result.columns[0].name, "id");
        assert_eq!(result.columns[1].name, "id_1");
        assert_eq!(result.rows[0]["id"], json!(1));
        assert_eq!(result.rows[0]["id_1"], json!(2));
    }

    /// `NUMERIC` is emitted as an exact decimal string, preserving precision.
    #[tokio::test]
    async fn pg_numeric_decodes_as_an_exact_decimal_string() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_PG_URL not set");
            return;
        };
        let result = PostgresConnector::new()
            .run_query(&opts, "SELECT numeric_col FROM conn_types WHERE id = 1")
            .await
            .expect("pg run_query on conn_types failed");
        assert_eq!(result.rows[0]["numeric_col"], json!("12345678901234.5678"));
    }

    /// An undecodable range type (`int4range`) degrades to `null` without failing the query.
    #[tokio::test]
    async fn pg_range_type_degrades_to_null_instead_of_failing_the_query() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_PG_URL not set");
            return;
        };
        let result = PostgresConnector::new()
            .run_query(&opts, "SELECT range_col FROM conn_types WHERE id = 1")
            .await
            .expect("an undecodable column must degrade, not fail the query");
        assert_eq!(result.rows[0]["range_col"], Value::Null);
    }
}

mod aliases {
    use super::*;

    #[test]
    fn alias_type_names() {
        assert_eq!(RedshiftConnector::new().type_name(), "redshift");
        assert_eq!(CockroachDbConnector::new().type_name(), "cockroachdb");
    }

    #[tokio::test]
    async fn redshift_delegates_to_pg() {
        let Some(url) = std::env::var("TEST_PG_URL").ok() else {
            println!("[SKIP] TEST_PG_URL not set");
            return;
        };
        let Some(opts) = pg_opts_from_url(&url) else {
            panic!("failed to parse TEST_PG_URL");
        };
        let c = RedshiftConnector::new();
        assert_eq!(c.type_name(), "redshift");
        c.test_connection(&opts)
            .await
            .expect("redshift test_connection failed");
    }

    #[tokio::test]
    async fn cockroachdb_delegates_to_pg() {
        let Some(url) = std::env::var("TEST_PG_URL").ok() else {
            println!("[SKIP] TEST_PG_URL not set");
            return;
        };
        let Some(opts) = pg_opts_from_url(&url) else {
            panic!("failed to parse TEST_PG_URL");
        };
        let c = CockroachDbConnector::new();
        assert_eq!(c.type_name(), "cockroachdb");
        c.test_connection(&opts)
            .await
            .expect("cockroachdb test_connection failed");
    }
}

mod mysql {
    use super::*;

    fn mysql_opts() -> Option<serde_json::Value> {
        let url = std::env::var("TEST_MYSQL_URL").ok()?;
        mysql_opts_from_url(&url)
    }

    fn mariadb_opts() -> Option<serde_json::Value> {
        let url = std::env::var("TEST_MARIADB_URL").ok()?;
        mysql_opts_from_url(&url)
    }

    #[test]
    fn tidb_type_name() {
        assert_eq!(MySqlConnector::tidb().type_name(), "tidb");
    }

    #[tokio::test]
    async fn mysql_test_connection_ok() {
        let Some(opts) = mysql_opts() else {
            println!("[SKIP] TEST_MYSQL_URL not set");
            return;
        };
        MySqlConnector::mysql()
            .test_connection(&opts)
            .await
            .expect("mysql test_connection failed");
    }

    #[tokio::test]
    async fn mysql_get_schema_lists_conn_test() {
        let Some(opts) = mysql_opts() else {
            println!("[SKIP] TEST_MYSQL_URL not set");
            return;
        };
        let schema = MySqlConnector::mysql()
            .get_schema(&opts)
            .await
            .expect("mysql get_schema failed");
        assert!(!schema.is_empty());
        let t = schema
            .iter()
            .find(|t| t.name == "conn_test")
            .expect("conn_test not found");
        assert!(!t.columns.is_empty(), "conn_test must have columns");
        assert!(
            t.columns.iter().all(|c| c.type_name.is_some()),
            "every column should carry a type: {:?}",
            t.columns
        );
    }

    #[tokio::test]
    async fn mysql_run_query_returns_rows() {
        let Some(opts) = mysql_opts() else {
            println!("[SKIP] TEST_MYSQL_URL not set");
            return;
        };
        let result = MySqlConnector::mysql()
            .run_query(&opts, "SELECT id, name, n FROM conn_test ORDER BY id")
            .await
            .expect("mysql run_query failed");
        assert!(result.rows.len() >= 2, "expected >= 2 rows");
        assert!(result.columns.iter().any(|c| c.name == "name"));
    }

    #[tokio::test]
    async fn mariadb_run_query_returns_rows() {
        let Some(opts) = mariadb_opts() else {
            println!("[SKIP] TEST_MARIADB_URL not set");
            return;
        };
        let result = MySqlConnector::mariadb()
            .run_query(&opts, "SELECT id, name, n FROM conn_test ORDER BY id")
            .await
            .expect("mariadb run_query failed");
        assert!(result.rows.len() >= 2, "expected >= 2 rows");
    }

    /// `INT UNSIGNED` must decode; one unsupported cell type must not fail the whole query.
    #[tokio::test]
    async fn mysql_conn_types_unsigned_id_does_not_fail_the_whole_query() {
        let Some(opts) = mysql_opts() else {
            println!("[SKIP] TEST_MYSQL_URL not set");
            return;
        };
        let result = MySqlConnector::mysql()
            .run_query(&opts, "SELECT * FROM conn_types ORDER BY row_num")
            .await
            .expect("SELECT * FROM conn_types must not fail on an INT UNSIGNED column");
        assert_eq!(
            result.rows.len(),
            2,
            "expected the seeded row + the all-NULL row"
        );
    }

    /// Every type decodes to the documented shape (unsigned ints as numbers, JSON as object,
    /// YEAR/BIT as integers, binary as `<blob N bytes>`).
    #[tokio::test]
    async fn mysql_conn_types_decode_to_expected_shapes() {
        let Some(opts) = mysql_opts() else {
            println!("[SKIP] TEST_MYSQL_URL not set");
            return;
        };
        let result = MySqlConnector::mysql()
            .run_query(&opts, "SELECT * FROM conn_types ORDER BY row_num")
            .await
            .expect("mysql run_query on conn_types failed");
        let row = &result.rows[0];

        assert_eq!(row["id"], json!(42));
        assert_eq!(row["tiny_u"], json!(200));
        assert_eq!(row["small_u"], json!(50000));
        assert_eq!(row["medium_u"], json!(10_000_000));
        assert_eq!(row["big_u"], json!(18_446_744_073_709_551_615u64));
        assert_eq!(row["j"], serde_json::json!({"a": 1}));
        assert_eq!(row["y"], json!(2026));
        assert_eq!(row["b"], json!(170));
        assert_eq!(row["blob_col"], json!("<blob 5 bytes>"));
        assert_eq!(row["numeric_col"], json!("12345678901234.5678"));
        // sqlx reports TINYINT(1) as BOOLEAN; the value must still decode as a JSON bool.
        assert_eq!(row["flag_col"], json!(true));

        let col_type = |name: &str| -> &str {
            result
                .columns
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.friendly_type.as_str())
                .unwrap_or("MISSING")
        };
        assert_eq!(col_type("id"), "integer");
        assert_eq!(col_type("tiny_u"), "integer");
        assert_eq!(col_type("big_u"), "integer");
        assert_eq!(col_type("y"), "integer");
        assert_eq!(col_type("b"), "integer");
        assert_eq!(col_type("flag_col"), "boolean");

        // The all-NULL row must decode every type to `Value::Null`.
        let null_row = &result.rows[1];
        for col in [
            "id",
            "tiny_u",
            "big_u",
            "j",
            "y",
            "b",
            "blob_col",
            "numeric_col",
        ] {
            assert_eq!(null_row[col], Value::Null, "column '{col}' NULL row");
        }
    }

    /// MySQL `DATETIME` renders like postgres's zone-less `TIMESTAMP` (`%Y-%m-%dT%H:%M:%S%.f`).
    #[tokio::test]
    async fn mysql_datetime_matches_postgres_timestamp_shape() {
        let Some(opts) = mysql_opts() else {
            println!("[SKIP] TEST_MYSQL_URL not set");
            return;
        };
        let result = MySqlConnector::mysql()
            .run_query(&opts, "SELECT ts_col FROM conn_types WHERE row_num = 1")
            .await
            .expect("mysql run_query on conn_types failed");
        assert_eq!(result.rows[0]["ts_col"], json!("2026-08-14T09:00:00"));
    }

    /// Duplicate column names must not collapse to one JSON key.
    #[tokio::test]
    async fn mysql_duplicate_column_names_get_deduped() {
        let Some(opts) = mysql_opts() else {
            println!("[SKIP] TEST_MYSQL_URL not set");
            return;
        };
        let result = MySqlConnector::mysql()
            .run_query(&opts, "SELECT 1 AS id, 2 AS id")
            .await
            .expect("mysql run_query with duplicate column names failed");
        assert_eq!(result.columns.len(), 2, "columns: {:?}", result.columns);
        assert_eq!(result.columns[0].name, "id");
        assert_eq!(result.columns[1].name, "id_1");
        assert_eq!(result.rows[0]["id"], json!(1));
        assert_eq!(result.rows[0]["id_1"], json!(2));
    }

    /// MariaDB reports some types differently; the bar is only that the query doesn't fail.
    #[tokio::test]
    async fn mariadb_conn_types_does_not_fail_the_whole_query() {
        let Some(opts) = mariadb_opts() else {
            println!("[SKIP] TEST_MARIADB_URL not set");
            return;
        };
        let result = MySqlConnector::mariadb()
            .run_query(&opts, "SELECT * FROM conn_types ORDER BY row_num")
            .await
            .expect("SELECT * FROM conn_types must not fail on MariaDB either");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0]["id"], json!(42));
    }
}

mod mssql {
    use super::*;

    fn opts() -> Option<serde_json::Value> {
        let url = std::env::var("TEST_MSSQL_URL").ok()?;
        mssql_opts_from_url(&url)
    }

    #[test]
    fn mssql_type_name() {
        assert_eq!(MssqlConnector.type_name(), "mssql");
    }

    #[tokio::test]
    async fn mssql_test_connection_ok() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MSSQL_URL not set");
            return;
        };
        MssqlConnector
            .test_connection(&opts)
            .await
            .expect("mssql test_connection failed");
    }

    #[tokio::test]
    async fn mssql_get_schema_lists_conn_test() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MSSQL_URL not set");
            return;
        };
        let schema = MssqlConnector
            .get_schema(&opts)
            .await
            .expect("mssql get_schema failed");
        assert!(!schema.is_empty());
        let t = schema
            .iter()
            .find(|t| t.name == "conn_test")
            .expect("conn_test not found");
        assert!(!t.columns.is_empty(), "conn_test must have columns");
        assert!(
            t.columns.iter().all(|c| c.type_name.is_some()),
            "every column should carry a type: {:?}",
            t.columns
        );
    }

    #[tokio::test]
    async fn mssql_run_query_returns_rows() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MSSQL_URL not set");
            return;
        };
        let result = MssqlConnector
            .run_query(&opts, "SELECT id, name, n FROM conn_test ORDER BY id")
            .await
            .expect("mssql run_query failed");
        assert!(result.rows.len() >= 2, "expected >= 2 rows");
        assert!(result.columns.iter().any(|c| c.name == "name"));
    }

    /// T-SQL batches are supported (`supports_auto_limit() == false`); the connector returns the
    /// last statement's result.
    #[tokio::test]
    async fn mssql_batch_query_with_two_non_empty_sets_returns_the_last() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MSSQL_URL not set");
            return;
        };
        let result = MssqlConnector
            .run_query(&opts, "SELECT 1 AS a; SELECT 2 AS b")
            .await
            .expect("mssql batch query failed");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0]["b"], json!(2));
        assert!(
            result.rows[0].get("a").is_none(),
            "the first statement's result must not leak into the returned set"
        );
    }

    /// The last result set is chosen, not the last non-empty one; needs a batch whose final set is
    /// empty.
    #[tokio::test]
    async fn mssql_batch_query_returns_the_last_result_set() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MSSQL_URL not set");
            return;
        };
        let result = MssqlConnector
            .run_query(&opts, "SELECT 1 AS a; SELECT 2 AS b WHERE 1=0")
            .await
            .expect("mssql batch query failed");
        assert!(
            result.rows.is_empty(),
            "the last statement matched nothing; an earlier statement's row must not leak \
             through: {:?}",
            result.rows
        );
        assert!(
            result.columns.is_empty(),
            "a zero-row result set carries no columns: {:?}",
            result.columns
        );
    }

    /// Single-statement queries behave unchanged.
    #[tokio::test]
    async fn mssql_single_statement_query_is_unchanged() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MSSQL_URL not set");
            return;
        };
        let result = MssqlConnector
            .run_query(&opts, "SELECT 1 AS n")
            .await
            .expect("mssql single-statement query failed");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0]["n"], json!(1));
    }

    /// `NUMERIC`/`DECIMAL` is emitted as an exact decimal string, preserving precision.
    #[tokio::test]
    async fn mssql_numeric_decodes_as_an_exact_decimal_string() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MSSQL_URL not set");
            return;
        };
        let result = MssqlConnector
            .run_query(&opts, "SELECT numeric_col FROM conn_types WHERE id = 1")
            .await
            .expect("mssql run_query on conn_types failed");
        assert_eq!(result.rows[0]["numeric_col"], json!("12345678901234.5678"));
    }

    /// Duplicate column names must not collapse to one JSON key.
    #[tokio::test]
    async fn mssql_duplicate_column_names_get_deduped() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MSSQL_URL not set");
            return;
        };
        let result = MssqlConnector
            .run_query(&opts, "SELECT 1 AS id, 2 AS id")
            .await
            .expect("mssql run_query with duplicate column names failed");
        assert_eq!(result.columns.len(), 2, "columns: {:?}", result.columns);
        assert_eq!(result.columns[0].name, "id");
        assert_eq!(result.columns[1].name, "id_1");
        assert_eq!(result.rows[0]["id"], json!(1));
        assert_eq!(result.rows[0]["id_1"], json!(2));
    }

    /// With `supports_auto_limit() == false` the connector enforces the row cap itself; a
    /// cross-joined `sys.all_objects` exceeds it cheaply.
    #[tokio::test]
    async fn mssql_row_cap_errors_naming_the_cap_instead_of_returning_unbounded_rows() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MSSQL_URL not set");
            return;
        };
        let err = MssqlConnector
            .run_query(
                &opts,
                "SELECT TOP (10001) 1 AS n FROM sys.all_objects a CROSS JOIN sys.all_objects b",
            )
            .await
            .expect_err("a result past the row cap must error, not return unbounded rows");
        assert!(
            err.to_string().contains("row cap"),
            "expected the cap to be named in the error, got: {err}"
        );
    }

    /// tiberius has no query-level timeout, so `run_query` bounds itself via
    /// `connectors::query_timeout_secs()`; `WAITFOR DELAY` outruns a short one.
    /// `QUERY_TIMEOUT_SECS` is process-global, safe here because the suite runs with
    /// `--test-threads=1`.
    #[tokio::test]
    async fn mssql_query_timeout_stops_a_runaway_statement() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MSSQL_URL not set");
            return;
        };
        std::env::set_var("QUERY_TIMEOUT_SECS", "1");
        let result = MssqlConnector
            .run_query(&opts, "WAITFOR DELAY '00:00:05'; SELECT 1 AS n")
            .await;
        std::env::remove_var("QUERY_TIMEOUT_SECS");
        let err = result.expect_err("a statement past QUERY_TIMEOUT_SECS must time out");
        assert!(
            err.to_string().contains("timed out"),
            "expected a timeout error, got: {err}"
        );
    }
}

mod clickhouse {
    use super::*;

    fn opts() -> Option<serde_json::Value> {
        let url = std::env::var("TEST_CLICKHOUSE_URL").ok()?;
        ch_opts_from_url(&url)
    }

    #[tokio::test]
    async fn clickhouse_test_connection_ok() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_CLICKHOUSE_URL not set");
            return;
        };
        ClickHouseConnector
            .test_connection(&opts)
            .await
            .expect("clickhouse test_connection failed");
    }

    #[tokio::test]
    async fn clickhouse_get_schema_lists_conn_test() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_CLICKHOUSE_URL not set");
            return;
        };
        let schema = ClickHouseConnector
            .get_schema(&opts)
            .await
            .expect("clickhouse get_schema failed");
        assert!(!schema.is_empty());
        let t = schema
            .iter()
            .find(|t| t.name == "conn_test")
            .expect("conn_test not found");
        let name_col = t.columns.iter().find(|c| c.name == "name").unwrap();
        assert_eq!(name_col.type_name.as_deref(), Some("String"));
    }

    #[tokio::test]
    async fn clickhouse_run_query_returns_rows() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_CLICKHOUSE_URL not set");
            return;
        };
        let result = ClickHouseConnector
            .run_query(&opts, "SELECT id, name, n FROM conn_test ORDER BY id")
            .await
            .expect("clickhouse run_query failed");
        assert!(result.rows.len() >= 2, "expected >= 2 rows");
        assert!(result.columns.iter().any(|c| c.name == "name"));
    }

    /// A trailing `;` must not turn `<query>\nFORMAT JSONCompact` into a ClickHouse syntax error.
    #[tokio::test]
    async fn clickhouse_run_query_tolerates_a_trailing_semicolon() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_CLICKHOUSE_URL not set");
            return;
        };
        let result = ClickHouseConnector
            .run_query(&opts, "SELECT 1;")
            .await
            .expect("clickhouse run_query with a trailing semicolon failed");
        assert_eq!(result.rows.len(), 1);
    }

    /// A query naming its own `FORMAT` gets the connector's clear error, not a ClickHouse syntax
    /// error.
    #[tokio::test]
    async fn clickhouse_run_query_rejects_a_query_with_its_own_format_clause() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_CLICKHOUSE_URL not set");
            return;
        };
        let err = ClickHouseConnector
            .run_query(&opts, "SELECT 1 FORMAT JSONEachRow")
            .await
            .expect_err("a query with its own FORMAT clause must be rejected, not run");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("jsoncompact") || msg.contains("format"),
            "expected the connector's own clear error about FORMAT, got: {err:#}"
        );
    }

    /// `SETTINGS` before `FORMAT` (ClickHouse's canonical order) must be rejected too.
    #[tokio::test]
    async fn clickhouse_run_query_rejects_settings_before_its_own_format_clause() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_CLICKHOUSE_URL not set");
            return;
        };
        let err = ClickHouseConnector
            .run_query(&opts, "SELECT 1 SETTINGS max_threads=1 FORMAT JSONEachRow")
            .await
            .expect_err("a trailing FORMAT clause after SETTINGS must be rejected, not run");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("jsoncompact") || msg.contains("format"),
            "expected the connector's own clear error about FORMAT, got: {err:#}"
        );
    }

    /// `Date32` must report the `date` friendly type, like `Date`.
    #[tokio::test]
    async fn clickhouse_date32_column_reports_date_friendly_type() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_CLICKHOUSE_URL not set");
            return;
        };
        let result = ClickHouseConnector
            .run_query(&opts, "SELECT d32 FROM conn_test WHERE id = 1")
            .await
            .expect("clickhouse run_query on d32 failed");
        let col = result
            .columns
            .iter()
            .find(|c| c.name == "d32")
            .expect("d32 column not found");
        assert_eq!(col.friendly_type, "date");
    }

    /// Duplicate column names must not collapse to one JSON key; ClickHouse rejects `1 AS id, 2 AS
    /// id`, so a self-join selecting `a.id, b.id` is used.
    #[tokio::test]
    async fn clickhouse_duplicate_column_names_get_deduped() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_CLICKHOUSE_URL not set");
            return;
        };
        let result = ClickHouseConnector
            .run_query(&opts, "SELECT id, id FROM conn_test WHERE id = 2")
            .await
            .expect("clickhouse run_query with duplicate column names failed");
        assert_eq!(result.columns.len(), 2, "columns: {:?}", result.columns);
        assert_eq!(result.columns[0].name, "id");
        assert_eq!(result.columns[1].name, "id_1");
        assert_eq!(result.rows[0]["id"], json!(2));
        assert_eq!(result.rows[0]["id_1"], json!(2));
    }
}

mod mongodb {
    use super::*;

    fn opts() -> Option<serde_json::Value> {
        let url = std::env::var("TEST_MONGO_URL").ok()?;
        mongo_opts_from_url(&url)
    }

    #[tokio::test]
    async fn mongo_test_connection_ping_ok() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MONGO_URL not set");
            return;
        };
        MongoConnector
            .test_connection(&opts)
            .await
            .expect("mongo test_connection failed");
    }

    #[tokio::test]
    async fn mongo_get_schema_lists_conn_test() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MONGO_URL not set");
            return;
        };
        let schema = MongoConnector
            .get_schema(&opts)
            .await
            .expect("mongo get_schema failed");
        assert!(!schema.is_empty());
        let t = schema
            .iter()
            .find(|t| t.name == "conn_test")
            .expect("conn_test collection not found");
        assert!(!t.columns.is_empty(), "conn_test must have columns");
        assert!(t.columns.iter().all(|c| c.type_name.is_none()));
    }

    /// `get_schema`'s per-collection `$sample` carries `max_time`; a normal database still returns
    /// promptly.
    #[tokio::test]
    async fn mongo_get_schema_completes_within_a_sane_wall_clock_bound() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MONGO_URL not set");
            return;
        };
        let start = std::time::Instant::now();
        MongoConnector
            .get_schema(&opts)
            .await
            .expect("mongo get_schema failed");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(30),
            "get_schema took {:?}, expected well under the statement timeout",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn mongo_run_query_returns_rows() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MONGO_URL not set");
            return;
        };
        let result = MongoConnector
            .run_query(&opts, r#"{"collection":"conn_test","filter":{}}"#)
            .await
            .expect("mongo run_query failed");
        assert!(result.rows.len() >= 2, "expected >= 2 rows");
        assert!(result.columns.iter().any(|c| c.name == "name"));
    }

    /// `"limit": 0` must not be taken as unlimited: the query still succeeds with the real rows.
    #[tokio::test]
    async fn mongo_limit_zero_does_not_error_and_still_returns_rows() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MONGO_URL not set");
            return;
        };
        let result = MongoConnector
            .run_query(&opts, r#"{"collection":"conn_test","filter":{},"limit":0}"#)
            .await
            .expect("mongo run_query with limit:0 failed");
        assert!(
            result.rows.len() >= 2,
            "expected >= 2 rows even with limit:0"
        );
    }

    /// A `find` cut off by the server-side limit returns exactly the limit when more documents
    /// exist.
    #[tokio::test]
    async fn mongo_find_result_cut_at_the_limit_returns_exactly_the_limit() {
        let Some(url) = std::env::var("TEST_MONGO_URL").ok() else {
            println!("[SKIP] TEST_MONGO_URL not set");
            return;
        };
        let Some(opts) = mongo_opts_from_url(&url) else {
            panic!("failed to parse TEST_MONGO_URL");
        };
        let db_name = opts["db"].as_str().unwrap().to_string();
        // The connector authenticates against "admin" via `auth_source`; a raw URI without
        // `authSource` fails SCRAM.
        let client = ::mongodb::Client::with_uri_str(format!("{url}?authSource=admin"))
            .await
            .expect("mongodb driver connect failed");
        let coll = client
            .database(&db_name)
            .collection::<::mongodb::bson::Document>("qr_limit_cutoff_test");
        coll.drop().await.ok();
        let docs: Vec<::mongodb::bson::Document> =
            (0..5).map(|i| ::mongodb::bson::doc! { "n": i }).collect();
        coll.insert_many(docs).await.expect("seed insert failed");

        let result = MongoConnector
            .run_query(
                &opts,
                r#"{"collection":"qr_limit_cutoff_test","filter":{},"limit":3}"#,
            )
            .await
            .expect("mongo run_query with limit:3 failed");
        assert_eq!(
            result.rows.len(),
            3,
            "a result cut at the limit must return exactly the limit, not more or fewer"
        );
    }

    /// A filter that fails BSON conversion must error naming the field, not fall back to `{}`.
    #[tokio::test]
    async fn mongo_invalid_filter_errors_naming_the_field() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MONGO_URL not set");
            return;
        };
        let result = MongoConnector
            .run_query(
                &opts,
                r#"{"collection":"conn_test","filter":"not-an-object"}"#,
            )
            .await;
        let err =
            result.expect_err("a non-document filter must error, not silently match everything");
        assert!(
            err.to_string().contains("filter"),
            "error must name the offending field: {err}"
        );
    }

    /// An absent `filter` is the legitimate match-everything case.
    #[tokio::test]
    async fn mongo_absent_filter_still_matches_everything() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MONGO_URL not set");
            return;
        };
        let result = MongoConnector
            .run_query(&opts, r#"{"collection":"conn_test"}"#)
            .await
            .expect("mongo run_query with no filter field failed");
        assert!(
            result.rows.len() >= 2,
            "an absent filter must still match every document"
        );
    }

    /// The `$limit` stage appended to aggregate pipelines must not break a normal query.
    #[tokio::test]
    async fn mongo_aggregate_still_works_with_the_row_cap_stage_appended() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_MONGO_URL not set");
            return;
        };
        let result = MongoConnector
            .run_query(
                &opts,
                r#"{"collection":"conn_test","aggregate":[{"$match":{}}]}"#,
            )
            .await
            .expect("mongo aggregate run_query failed");
        assert!(
            result.rows.len() >= 2,
            "aggregate must still return the seeded rows despite the appended $limit stage"
        );
    }
}

mod elasticsearch {
    use super::*;

    fn opts() -> Option<serde_json::Value> {
        let url = std::env::var("TEST_ES_URL").ok()?;
        es_opts_from_url(&url)
    }

    #[tokio::test]
    async fn es_test_connection_ok() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_ES_URL not set");
            return;
        };
        ElasticsearchConnector
            .test_connection(&opts)
            .await
            .expect("es test_connection failed");
    }

    #[tokio::test]
    async fn es_get_schema_lists_index() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_ES_URL not set");
            return;
        };
        let schema = ElasticsearchConnector
            .get_schema(&opts)
            .await
            .expect("es get_schema failed");
        assert!(!schema.is_empty());
        let t = schema
            .iter()
            .find(|t| t.name == "conn_test")
            .expect("conn_test index not found");
        assert!(!t.columns.is_empty(), "conn_test must have columns");
        assert!(
            t.columns.iter().all(|c| c.type_name.is_some()),
            "every column should carry a type: {:?}",
            t.columns
        );
    }

    #[tokio::test]
    async fn es_run_query_match_all_returns_hits() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_ES_URL not set");
            return;
        };
        let query = "conn_test\n{\"query\":{\"match_all\":{}}}";
        let result = ElasticsearchConnector
            .run_query(&opts, query)
            .await
            .expect("es run_query failed");
        assert!(result.rows.len() >= 2, "expected >= 2 hits");
    }

    async fn conn_test_count(base: &str) -> u64 {
        let resp = reqwest::Client::new()
            .get(format!("{base}/conn_test/_count"))
            .send()
            .await
            .expect("count request failed")
            .json::<serde_json::Value>()
            .await
            .expect("count response not JSON");
        resp["count"].as_u64().expect("count field missing")
    }

    #[tokio::test]
    async fn es_run_query_rejects_path_traversal_index() {
        let Some(url) = std::env::var("TEST_ES_URL").ok() else {
            println!("[SKIP] TEST_ES_URL not set");
            return;
        };
        let opts = es_opts_from_url(&url).expect("valid TEST_ES_URL");
        let before = conn_test_count(&url).await;

        // ".." escapes the index segment onto `/_bulk`; a trailing `#` would truncate the path at
        // the query parser.
        let query =
            "x/../_bulk?refresh#\n{\"index\":{\"_index\":\"conn_test\"}}\n{\"field\":\"pwned\"}\n";
        let result = ElasticsearchConnector.run_query(&opts, query).await;
        assert!(result.is_err(), "path-traversal index must be rejected");

        let after = conn_test_count(&url).await;
        assert_eq!(before, after, "no document should have reached _bulk");
    }

    #[tokio::test]
    async fn es_run_query_rejects_path_traversal_to_delete_by_query() {
        let Some(url) = std::env::var("TEST_ES_URL").ok() else {
            println!("[SKIP] TEST_ES_URL not set");
            return;
        };
        let opts = es_opts_from_url(&url).expect("valid TEST_ES_URL");
        let before = conn_test_count(&url).await;

        let query = "x/../conn_test/_delete_by_query?refresh#\n{\"query\":{\"match_all\":{}}}";
        let result = ElasticsearchConnector.run_query(&opts, query).await;
        assert!(result.is_err(), "path-traversal index must be rejected");

        let after = conn_test_count(&url).await;
        assert_eq!(before, after, "no document should have been deleted");
    }

    /// The container runs with security disabled, so a malformed Query DSL clause (reliable 400)
    /// exercises `post_with_auth`'s status check, which catches non-2xx before `run_query`'s
    /// `result.get("error")` check.
    #[tokio::test]
    async fn es_run_query_with_malformed_query_dsl_surfaces_as_error() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_ES_URL not set");
            return;
        };
        let query = "conn_test\n{\"query\":{\"not_a_real_es_clause_xyz\":{}}}";
        let result = ElasticsearchConnector.run_query(&opts, query).await;
        assert!(
            result.is_err(),
            "a malformed ES query DSL clause must surface as an error"
        );
    }

    /// `_id` lives outside `_source`, so the column scan must still declare it; no row key may be
    /// undeclared.
    #[tokio::test]
    async fn es_run_query_declares_id_column_and_matches_row_keys() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_ES_URL not set");
            return;
        };
        let query = "conn_test\n{\"query\":{\"match_all\":{}}}";
        let result = ElasticsearchConnector
            .run_query(&opts, query)
            .await
            .expect("es run_query failed");
        assert!(
            result.columns.iter().any(|c| c.name == "_id"),
            "columns must declare _id: {:?}",
            result.columns
        );
        let declared: std::collections::HashSet<&str> =
            result.columns.iter().map(|c| c.name.as_str()).collect();
        for row in &result.rows {
            let obj = row.as_object().expect("row must be a JSON object");
            for key in obj.keys() {
                assert!(
                    declared.contains(key.as_str()),
                    "row key {key:?} not declared in columns {declared:?}"
                );
            }
        }
    }
}

mod influxdb {
    use super::*;

    fn opts() -> Option<serde_json::Value> {
        let url = std::env::var("TEST_INFLUX_URL").ok()?;
        let db = std::env::var("TEST_INFLUX_DB").unwrap_or_else(|_| "testdb".to_string());
        influx_opts_from_url(&url, &db)
    }

    #[tokio::test]
    async fn influx_test_connection_ok() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_INFLUX_URL not set");
            return;
        };
        InfluxDbConnector
            .test_connection(&opts)
            .await
            .expect("influx test_connection failed");
    }

    #[tokio::test]
    async fn influx_get_schema_lists_measurement() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_INFLUX_URL not set");
            return;
        };
        let schema = InfluxDbConnector
            .get_schema(&opts)
            .await
            .expect("influx get_schema failed");
        assert!(!schema.is_empty());
        assert!(
            schema.iter().any(|t| t.name == "conn_test"),
            "conn_test measurement not found: {:?}",
            schema.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn influx_run_query_returns_rows() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_INFLUX_URL not set");
            return;
        };
        let result = InfluxDbConnector
            .run_query(&opts, "SELECT * FROM conn_test")
            .await
            .expect("influx run_query failed");
        assert!(result.rows.len() >= 2, "expected >= 2 rows");
    }

    /// `GROUP BY host` yields multiple series; reading only `/results/0/series/0` would drop all
    /// but the first.
    #[tokio::test]
    async fn influx_group_by_returns_rows_from_every_series() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_INFLUX_URL not set");
            return;
        };
        let result = InfluxDbConnector
            .run_query(&opts, "SELECT value FROM conn_test GROUP BY host")
            .await
            .expect("influx run_query failed");
        assert!(
            result.rows.len() >= 2,
            "rows from more than one series must come back: {:?}",
            result.rows
        );
        let series_labels: std::collections::HashSet<String> = result
            .rows
            .iter()
            .map(|r| r["series"].as_str().unwrap_or_default().to_string())
            .collect();
        assert!(
            series_labels.len() >= 2,
            "series must be distinguishable from each other: {series_labels:?}"
        );
    }
}

// CSV and SQLite connectors need a DB + `ObjectStore` this crate's test binary lacks; their
// round-trip tests live in the backend integration tests.

mod json {
    use super::*;

    fn opts() -> Option<serde_json::Value> {
        let url = std::env::var("TEST_JSON_URL").ok()?;
        Some(json!({ "url": url }))
    }

    #[tokio::test]
    async fn json_test_connection_ok() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_JSON_URL not set");
            return;
        };
        JsonConnector
            .test_connection(&opts)
            .await
            .expect("json test_connection failed");
    }

    #[tokio::test]
    async fn json_get_schema_columns() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_JSON_URL not set");
            return;
        };
        let schema = JsonConnector
            .get_schema(&opts)
            .await
            .expect("json get_schema failed");
        assert!(!schema.is_empty());
        assert!(schema[0].columns.iter().any(|c| c.name == "name"));
        assert!(schema[0].columns.iter().all(|c| c.type_name.is_none()));
    }

    #[tokio::test]
    async fn json_run_query_returns_rows() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_JSON_URL not set");
            return;
        };
        let result = JsonConnector
            .run_query(&opts, "SELECT * FROM data")
            .await
            .expect("json run_query failed");
        assert!(result.rows.len() >= 2);
        assert!(result.columns.iter().any(|c| c.name == "name"));
    }
}

// ── bigquery / snowflake / athena ──
// BigQuery runs against the local emulator by default (TEST_BIGQUERY_*), or real BigQuery with TEST_BIGQUERY_SA_JSON_PATH.
// Snowflake and Athena have no usable local emulator and stay env-gated (TEST_SNOWFLAKE_*/TEST_ATHENA_*), printing [SKIP] otherwise.

mod bigquery {
    use super::*;

    /// Emulator mode (`TEST_BIGQUERY_ENDPOINT`) needs no service-account key; real mode reads it
    /// from a file path (`TEST_BIGQUERY_SA_JSON_PATH`).
    fn opts() -> Option<serde_json::Value> {
        let project_id = std::env::var("TEST_BIGQUERY_PROJECT_ID").ok()?;
        let dataset = std::env::var("TEST_BIGQUERY_DATASET").ok();
        if let Ok(api_endpoint) = std::env::var("TEST_BIGQUERY_ENDPOINT") {
            return Some(json!({
                "project_id": project_id,
                "dataset": dataset,
                "api_endpoint": api_endpoint,
            }));
        }
        let sa_json_path = std::env::var("TEST_BIGQUERY_SA_JSON_PATH").ok()?;
        let service_account_json = std::fs::read_to_string(&sa_json_path).unwrap_or_else(|e| {
            panic!("failed to read TEST_BIGQUERY_SA_JSON_PATH ({sa_json_path}): {e}")
        });
        Some(json!({
            "project_id": project_id,
            "service_account_json": service_account_json,
            "dataset": dataset,
        }))
    }

    const SKIP_MSG: &str = "[SKIP] neither TEST_BIGQUERY_ENDPOINT (emulator) nor \
        TEST_BIGQUERY_PROJECT_ID+TEST_BIGQUERY_SA_JSON_PATH (real BigQuery) set";

    #[test]
    fn bigquery_type_name() {
        assert_eq!(BigQueryConnector.type_name(), "bigquery");
    }

    #[tokio::test]
    async fn bigquery_test_connection_ok() {
        let Some(opts) = opts() else {
            println!("{SKIP_MSG}");
            return;
        };
        BigQueryConnector
            .test_connection(&opts)
            .await
            .expect("bigquery test_connection failed");
    }

    #[tokio::test]
    async fn bigquery_get_schema_lists_conn_test() {
        let Some(opts) = opts() else {
            println!("{SKIP_MSG}");
            return;
        };
        let schema = BigQueryConnector
            .get_schema(&opts)
            .await
            .expect("bigquery get_schema failed");
        // Only the emulator run seeds `conn_test`; against real BigQuery only assert introspection
        // didn't error.
        if std::env::var("TEST_BIGQUERY_ENDPOINT").is_ok() {
            let t = schema
                .iter()
                .find(|t| t.name.ends_with(".conn_test"))
                .expect("conn_test table not found");
            assert!(!t.columns.is_empty(), "conn_test must have columns");
            assert!(
                t.columns.iter().all(|c| c.type_name.is_some()),
                "every column should carry a type: {:?}",
                t.columns
            );
        }
    }

    #[tokio::test]
    async fn bigquery_run_query_returns_rows() {
        let Some(opts) = opts() else {
            println!("{SKIP_MSG}");
            return;
        };
        let result = BigQueryConnector
            .run_query(&opts, "SELECT 1 AS n, 'a' AS s")
            .await
            .expect("bigquery run_query failed");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0]["n"], json!(1));
        assert_eq!(result.rows[0]["s"], json!("a"));
    }

    /// The emulator skips the SA-JWT to OAuth2 exchange, so this `#[ignore]`d live test is the only
    /// proof it works against real Google Cloud.
    #[tokio::test]
    #[ignore = "requires a real GCP service account with BigQuery access (BIGQUERY_TEST_SA_JSON + BIGQUERY_TEST_PROJECT_ID)"]
    async fn bigquery_live_token_exchange_runs_a_real_query() {
        let (Ok(sa_json), Ok(project_id)) = (
            std::env::var("BIGQUERY_TEST_SA_JSON"),
            std::env::var("BIGQUERY_TEST_PROJECT_ID"),
        ) else {
            println!("[SKIP] BIGQUERY_TEST_SA_JSON / BIGQUERY_TEST_PROJECT_ID not set");
            return;
        };

        let opts = json!({
            "project_id": project_id,
            "service_account_json": sa_json,
        });
        let result = BigQueryConnector
            .run_query(&opts, "SELECT 1 AS n")
            .await
            .expect("bigquery live run_query failed");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0]["n"], json!(1));

        // A crafted `token_uri` in the key file must be ignored by the real exchange.
        let mut key: Value =
            serde_json::from_str(&sa_json).expect("BIGQUERY_TEST_SA_JSON must be valid JSON");
        key["token_uri"] = json!("http://169.254.169.254/token");
        let malicious_opts = json!({
            "project_id": project_id,
            "service_account_json": key.to_string(),
        });
        let result = BigQueryConnector
            .run_query(&malicious_opts, "SELECT 1 AS n")
            .await
            .expect("bigquery run_query with a crafted token_uri must still succeed");
        assert_eq!(result.rows[0]["n"], json!(1));
    }
}

mod snowflake {
    use super::*;

    /// Reads the PEM private key from a file path (`TEST_SNOWFLAKE_PRIVATE_KEY_PATH`).
    fn opts() -> Option<serde_json::Value> {
        let account = std::env::var("TEST_SNOWFLAKE_ACCOUNT").ok()?;
        let user = std::env::var("TEST_SNOWFLAKE_USER").ok()?;
        let key_path = std::env::var("TEST_SNOWFLAKE_PRIVATE_KEY_PATH").ok()?;
        let private_key = std::fs::read_to_string(&key_path).unwrap_or_else(|e| {
            panic!("failed to read TEST_SNOWFLAKE_PRIVATE_KEY_PATH ({key_path}): {e}")
        });
        Some(json!({
            "account": account,
            "user": user,
            "private_key": private_key,
            "warehouse": std::env::var("TEST_SNOWFLAKE_WAREHOUSE").ok(),
            "database": std::env::var("TEST_SNOWFLAKE_DATABASE").ok(),
            "schema": std::env::var("TEST_SNOWFLAKE_SCHEMA").ok(),
        }))
    }

    #[test]
    fn snowflake_type_name() {
        assert_eq!(SnowflakeConnector.type_name(), "snowflake");
    }

    #[tokio::test]
    async fn snowflake_test_connection_ok() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_SNOWFLAKE_ACCOUNT/TEST_SNOWFLAKE_USER/TEST_SNOWFLAKE_PRIVATE_KEY_PATH not set");
            return;
        };
        SnowflakeConnector
            .test_connection(&opts)
            .await
            .expect("snowflake test_connection failed");
    }

    #[tokio::test]
    async fn snowflake_get_schema_does_not_error() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_SNOWFLAKE_ACCOUNT/TEST_SNOWFLAKE_USER/TEST_SNOWFLAKE_PRIVATE_KEY_PATH not set");
            return;
        };
        SnowflakeConnector
            .get_schema(&opts)
            .await
            .expect("snowflake get_schema failed");
    }

    #[tokio::test]
    async fn snowflake_run_query_returns_rows() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_SNOWFLAKE_ACCOUNT/TEST_SNOWFLAKE_USER/TEST_SNOWFLAKE_PRIVATE_KEY_PATH not set");
            return;
        };
        let result = SnowflakeConnector
            .run_query(&opts, "SELECT 1 AS n, 'a' AS s")
            .await
            .expect("snowflake run_query failed");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0]["N"], json!(1));
    }
}

mod athena {
    use super::*;

    fn opts() -> Option<serde_json::Value> {
        let aws_access_key = std::env::var("TEST_ATHENA_ACCESS_KEY").ok()?;
        let aws_secret_key = std::env::var("TEST_ATHENA_SECRET_KEY").ok()?;
        let region = std::env::var("TEST_ATHENA_REGION").ok()?;
        let s3_output_location = std::env::var("TEST_ATHENA_S3_OUTPUT").ok()?;
        let database = std::env::var("TEST_ATHENA_DATABASE").ok()?;
        Some(json!({
            "aws_access_key": aws_access_key,
            "aws_secret_key": aws_secret_key,
            "region": region,
            "s3_output_location": s3_output_location,
            "database": database,
        }))
    }

    #[test]
    fn athena_type_name() {
        assert_eq!(AthenaConnector.type_name(), "athena");
    }

    #[tokio::test]
    async fn athena_test_connection_ok() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_ATHENA_ACCESS_KEY/TEST_ATHENA_SECRET_KEY/TEST_ATHENA_REGION/TEST_ATHENA_S3_OUTPUT/TEST_ATHENA_DATABASE not set");
            return;
        };
        AthenaConnector
            .test_connection(&opts)
            .await
            .expect("athena test_connection failed");
    }

    #[tokio::test]
    async fn athena_get_schema_does_not_error() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_ATHENA_ACCESS_KEY/TEST_ATHENA_SECRET_KEY/TEST_ATHENA_REGION/TEST_ATHENA_S3_OUTPUT/TEST_ATHENA_DATABASE not set");
            return;
        };
        AthenaConnector
            .get_schema(&opts)
            .await
            .expect("athena get_schema failed");
    }

    #[tokio::test]
    async fn athena_run_query_returns_rows() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_ATHENA_ACCESS_KEY/TEST_ATHENA_SECRET_KEY/TEST_ATHENA_REGION/TEST_ATHENA_S3_OUTPUT/TEST_ATHENA_DATABASE not set");
            return;
        };
        let result = AthenaConnector
            .run_query(&opts, "SELECT 1 AS n, 'a' AS s")
            .await
            .expect("athena run_query failed");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0]["n"], json!(1));
    }

    /// Proves the deadline terminates the poll loop: 1s is well under Athena's typical latency.
    #[tokio::test]
    async fn athena_run_query_times_out_at_query_timeout_secs() {
        let Some(opts) = opts() else {
            println!("[SKIP] TEST_ATHENA_ACCESS_KEY/TEST_ATHENA_SECRET_KEY/TEST_ATHENA_REGION/TEST_ATHENA_S3_OUTPUT/TEST_ATHENA_DATABASE not set");
            return;
        };
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var("QUERY_TIMEOUT_SECS", "1");
        let result = AthenaConnector.run_query(&opts, "SELECT 1 AS n").await;
        std::env::remove_var("QUERY_TIMEOUT_SECS");
        let err = result.expect_err("a 1s query timeout must trip before Athena can finish");
        assert!(
            err.to_string().contains("timed out"),
            "expected a timeout error, got: {err}"
        );
    }

    // ── live wire-protocol tests ──
    // A `wiremock` server speaks enough of Athena's AWS-JSON-1.1 protocol to exercise `run_query`'s real SigV4 requests, `TimeoutConfig`, poll/backoff loop and deadline.
    // Needs `ARCODASH_ALLOW_PRIVATE_CONNECTORS=1`, set per test via `private_net_env`.

    /// Serializes the tests below that mutate the process-global `QUERY_TIMEOUT_SECS`.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn wire_opts(endpoint: &str) -> Value {
        json!({
            "aws_access_key": "test",
            "aws_secret_key": "test",
            "region": "us-east-1",
            "s3_output_location": "s3://test-bucket/",
            "database": "default",
            "api_endpoint": endpoint,
        })
    }

    async fn mount_start_query_execution(server: &wiremock::MockServer, query_execution_id: &str) {
        use wiremock::matchers::{header, method};
        use wiremock::{Mock, ResponseTemplate};
        Mock::given(method("POST"))
            .and(header("x-amz-target", "AmazonAthena.StartQueryExecution"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "QueryExecutionId": query_execution_id })),
            )
            .mount(server)
            .await;
    }

    async fn mount_get_query_execution_state(server: &wiremock::MockServer, state: &str) {
        use wiremock::matchers::{header, method};
        use wiremock::{Mock, ResponseTemplate};
        Mock::given(method("POST"))
            .and(header("x-amz-target", "AmazonAthena.GetQueryExecution"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "QueryExecution": {
                    "QueryExecutionId": "q-1",
                    "Status": { "State": state },
                }
            })))
            .mount(server)
            .await;
    }

    /// Happy path over real HTTP (start, get already `SUCCEEDED`, one results page) with the
    /// `TimeoutConfig` client.
    #[tokio::test]
    async fn athena_wire_protocol_happy_path_over_real_http() {
        use wiremock::matchers::{header, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        mount_start_query_execution(&server, "q-1").await;
        mount_get_query_execution_state(&server, "SUCCEEDED").await;
        Mock::given(method("POST"))
            .and(header("x-amz-target", "AmazonAthena.GetQueryResults"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ResultSet": {
                    "ResultSetMetadata": {
                        "ColumnInfo": [ { "Name": "n", "Type": "integer" } ]
                    },
                    "Rows": [
                        { "Data": [ { "VarCharValue": "n" } ] },
                        { "Data": [ { "VarCharValue": "1" } ] },
                    ]
                }
            })))
            .mount(&server)
            .await;

        let _env = arcodash_core::test_support::private_net_env(Some("1"));
        let opts = wire_opts(&server.uri());
        let result = AthenaConnector
            .run_query(&opts, "SELECT 1 AS n")
            .await
            .expect("run_query against the fake Athena server failed");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0]["n"], json!(1));
    }

    /// `GetQueryExecution` always answers `RUNNING`, so only the wall-clock deadline can stop the
    /// poll loop.
    #[tokio::test]
    async fn athena_poll_loop_trips_the_deadline_over_real_http() {
        use wiremock::MockServer;

        let server = MockServer::start().await;
        mount_start_query_execution(&server, "q-1").await;
        mount_get_query_execution_state(&server, "RUNNING").await;

        let _env = arcodash_core::test_support::private_net_env(Some("1"));
        let opts = wire_opts(&server.uri());
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var("QUERY_TIMEOUT_SECS", "1");
        let result = AthenaConnector.run_query(&opts, "SELECT 1 AS n").await;
        std::env::remove_var("QUERY_TIMEOUT_SECS");
        let err = result.expect_err("a query stuck RUNNING forever must still hit the deadline");
        assert!(
            err.to_string().contains("timed out"),
            "expected a timeout error, got: {err}"
        );
    }

    /// Live check against the compose `athena` service (community LocalStack): its "not yet
    /// implemented" response must surface as an error, not a panic or hang. Skips when the stack
    /// isn't up.
    #[tokio::test]
    async fn athena_localstack_connectivity_reaches_the_real_service() {
        if tokio::net::TcpStream::connect("127.0.0.1:4566")
            .await
            .is_err()
        {
            println!("[SKIP] localstack (docker-compose.testing.yml's `athena` service) not reachable on :4566");
            return;
        }
        let _env = arcodash_core::test_support::private_net_env(Some("1"));
        let opts = wire_opts("http://127.0.0.1:4566");
        let err = AthenaConnector
            .run_query(&opts, "SELECT 1 AS n")
            .await
            .expect_err("community LocalStack doesn't implement Athena; a real query must fail");
        assert!(
            err.to_string().contains("not yet implemented")
                || err.to_string().contains("pro feature"),
            "expected LocalStack's documented Athena stub error, got: {err}"
        );
    }
}

/// Fails when the testing stack is expected but its env is missing, so a stackless run can't pass
/// green having connected to nothing. Gated on `ARCODASH_REQUIRE_CONNECTOR_ENV=1`; all missing vars
/// are named at once. Excludes `TEST_ATHENA_*` (needs a paid LocalStack token), `TEST_SNOWFLAKE_*`
/// and `TEST_BIGQUERY_SA_JSON_PATH` (real vendor credentials).
#[test]
fn required_connector_env_is_present_when_the_stack_is_expected() {
    if std::env::var("ARCODASH_REQUIRE_CONNECTOR_ENV").as_deref() != Ok("1") {
        return;
    }
    const REQUIRED: &[&str] = &[
        "TEST_PG_URL",
        "TEST_MYSQL_URL",
        "TEST_MARIADB_URL",
        "TEST_CLICKHOUSE_URL",
        "TEST_MONGO_URL",
        "TEST_ES_URL",
        "TEST_INFLUX_URL",
        "TEST_MSSQL_URL",
        "TEST_BIGQUERY_ENDPOINT",
        "TEST_BIGQUERY_PROJECT_ID",
    ];
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|name| std::env::var(name).is_err())
        .collect();
    assert!(
        missing.is_empty(),
        "ARCODASH_REQUIRE_CONNECTOR_ENV=1 but missing: {}",
        missing.join(", ")
    );
}
