//! CSV connector over an uploaded file only, never a server-local path (that would let any
//! data-source editor read arbitrary files off the backend's disk).
//!
//! `upload_id` resolves through `data_source_upload` to an object key in the org's upload storage.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use uuid::Uuid;

use crate::connectors::sqlite_connector::{find_upload, run_sql_file, upload_cache_dir};
use crate::connectors::{
    ColumnSchema, NetworkTarget, QueryExecutionResult, QueryRunner, TableSchema,
};
use crate::entities::data_source_upload;
use crate::org_storage::OrgStorageResolver;
use crate::storage::{get_upload, upload_read_error};

pub struct CsvConnector {
    db: DatabaseConnection,
    storage: Arc<dyn OrgStorageResolver>,
    cache_dir: PathBuf,
}

impl CsvConnector {
    pub fn new(db: DatabaseConnection, storage: Arc<dyn OrgStorageResolver>) -> Self {
        Self {
            db,
            storage,
            cache_dir: upload_cache_dir(),
        }
    }

    /// Resolves `upload_id` (org-scoped) to a local SQLite file built from the CSV, cached by
    /// (upload uuid, delimiter byte): the delimiter must be in the key or a data-source edit would
    /// serve a stale materialization. Uploads are immutable so a hit never rebuilds, but the
    /// org-scoped lookup always runs first. Written atomically (temp file + rename) so readers
    /// never see a partial build.
    async fn materialized_path(
        &self,
        org_id: i64,
        upload_id: &str,
        delimiter: &str,
    ) -> Result<PathBuf> {
        let (uuid, row) = find_upload(&self.db, org_id, upload_id).await?;

        let delim = delimiter_byte(delimiter)?;
        let dest = self
            .cache_dir
            .join(format!("{uuid}-{delim:02x}.csv.sqlite"));
        if dest.exists() {
            return Ok(dest);
        }

        tokio::fs::create_dir_all(&self.cache_dir)
            .await
            .context("failed to create upload cache dir")?;
        let bytes = get_upload(&*self.storage, &self.db, org_id, &row.object_key)
            .await
            .map_err(|e| upload_read_error(e, "uploaded CSV"))?;
        let tmp = self
            .cache_dir
            .join(format!("{uuid}.tmp.{}", Uuid::new_v4()));
        // A failed build must not leave a half-built file in the cache directory.
        if let Err(e) = build_csv_sqlite(bytes, delim, &row.filename, &tmp).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        tokio::fs::rename(&tmp, &dest)
            .await
            .context("failed to finalize CSV materialization cache file")?;
        Ok(dest)
    }

    async fn fetch(
        &self,
        org_id: i64,
        upload_id: &str,
    ) -> Result<(data_source_upload::Model, bytes::Bytes)> {
        let (_, row) = find_upload(&self.db, org_id, upload_id).await?;
        let bytes = get_upload(&*self.storage, &self.db, org_id, &row.object_key)
            .await
            .map_err(|e| upload_read_error(e, "uploaded CSV"))?;
        Ok((row, bytes))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CsvOptions {
    pub upload_id: String,
    #[serde(default = "csv_delimiter")]
    pub delimiter: String,
}

fn csv_delimiter() -> String {
    ",".to_string()
}

/// `csv::ReaderBuilder::delimiter` takes a `u8`, so only a single-byte ASCII delimiter is
/// representable; truncating a multi-byte char would silently mis-parse.
///
/// Empty keeps the default (`,`); anything else not exactly one ASCII byte is a configuration
/// error. `pub` so the upload preview path (`upload_service::parse_csv_preview`) shares the rule.
pub fn delimiter_byte(delimiter: &str) -> Result<u8> {
    match delimiter.as_bytes() {
        [b] if b.is_ascii() => Ok(*b),
        [] => Ok(b','),
        _ => anyhow::bail!("CSV delimiter must be a single ASCII character, got {delimiter:?}"),
    }
}

fn csv_reader(bytes: bytes::Bytes, delimiter: u8) -> csv::Reader<Cursor<bytes::Bytes>> {
    csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .from_reader(Cursor::new(bytes))
}

fn table_name_from_filename(filename: &str) -> String {
    std::path::Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("csv")
        .to_string()
}

/// A CSV field's sniffed value, typed at insert time rather than per column: a declared column type
/// would give SQLite type affinity and make numeric predicates silently wrong (see
/// `build_csv_sqlite`).
enum CsvCell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

/// Empty → NULL; an integer/real parse is trusted only if formatting it back reproduces the
/// original text, so zip codes (`"01234"`), `"+5"` and `"1e3"` aren't rewritten as different
/// numbers. Trailing fractional zeros (`"10.50"`) are tolerated; every other mismatch stays text.
fn sniff_cell(raw: &str) -> CsvCell {
    if raw.is_empty() {
        return CsvCell::Null;
    }
    if let Ok(v) = raw.parse::<i64>() {
        if v.to_string() == raw {
            return CsvCell::Int(v);
        }
    }
    if let Ok(v) = raw.parse::<f64>() {
        // "inf"/"-inf"/"NaN" would round-trip clean, but a non-finite REAL reads back as SQL NULL,
        // silently dropping the text.
        if v.is_finite() && trim_trailing_fractional_zeros(raw) == v.to_string() {
            return CsvCell::Real(v);
        }
    }
    CsvCell::Text(raw.to_string())
}

/// `"10.50"` → `"10.5"`, `"100.00"` → `"100"`: strips trailing fractional zeros only. A string with
/// no `.` (e.g. `"01234"`) is returned unchanged so `sniff_cell`'s round-trip check still keeps zip
/// codes text.
fn trim_trailing_fractional_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Materializes CSV `bytes` into a fresh SQLite database at `dest` so submitted SQL runs through
/// the same engine and decode path as other uploads. This is the crate's only writable SQLite open,
/// and only ever on a path built from the upload cache directory, never one supplied by an editor.
///
/// Declares no column types and binds each value with its sniffed type: a TEXT-affinity column
/// would compare `WHERE salary > 100` as TEXT vs INTEGER and match every row. Streams rows inside a
/// single transaction since `bytes` is already resident.
async fn build_csv_sqlite(
    bytes: bytes::Bytes,
    delimiter: u8,
    filename: &str,
    dest: &Path,
) -> Result<()> {
    let mut rdr = csv_reader(bytes, delimiter);
    let headers: Vec<String> = rdr
        .headers()
        .context("could not parse CSV")?
        .iter()
        .map(String::from)
        .collect();

    let table = table_name_from_filename(filename).replace('"', "\"\"");
    let col_list = headers
        .iter()
        .map(|h| format!("\"{}\"", h.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = vec!["?"; headers.len()].join(", ");
    let ddl = format!("CREATE TABLE \"{table}\" ({col_list})");
    let insert_sql = format!("INSERT INTO \"{table}\" ({col_list}) VALUES ({placeholders})");

    let opts = SqliteConnectOptions::new()
        .filename(dest)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .context("failed to create CSV materialization file")?;

    let mut txn = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(ddl.as_str()))
        .execute(&mut *txn)
        .await
        .with_context(|| format!("could not create table for {filename}"))?;

    for result in rdr.records() {
        let record = result.context("CSV parse failed")?;
        let mut q = sqlx::query(sqlx::AssertSqlSafe(insert_sql.as_str()));
        for i in 0..headers.len() {
            q = match sniff_cell(record.get(i).unwrap_or("")) {
                CsvCell::Null => q.bind(Option::<String>::None),
                CsvCell::Int(v) => q.bind(v),
                CsvCell::Real(v) => q.bind(v),
                CsvCell::Text(s) => q.bind(s),
            };
        }
        q.execute(&mut *txn)
            .await
            .with_context(|| format!("could not insert row into {filename}"))?;
    }

    txn.commit().await?;
    pool.close().await;
    Ok(())
}

#[async_trait]
impl QueryRunner for CsvConnector {
    fn type_name(&self) -> &'static str {
        "csv"
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "upload_id": { "type": "file", "title": "CSV file", "accept": ".csv" },
                "delimiter": { "type": "string", "title": "Delimiter", "default": "," }
            },
            "required": ["upload_id"],
            "secret": []
        })
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let org_id = crate::connectors::injected_id(opts, "org_id", "csv")?;
        let opts: CsvOptions = serde_json::from_value(opts.clone())?;
        let (_row, bytes) = self.fetch(org_id, &opts.upload_id).await?;
        let mut rdr = csv_reader(bytes, delimiter_byte(&opts.delimiter)?);
        let headers = rdr.headers().context("could not parse CSV")?;
        if headers.is_empty() {
            anyhow::bail!("could not parse CSV: no header row");
        }
        Ok(())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let org_id = crate::connectors::injected_id(opts, "org_id", "csv")?;
        let opts: CsvOptions = serde_json::from_value(opts.clone())?;
        let (row, bytes) = self.fetch(org_id, &opts.upload_id).await?;
        let mut rdr = csv_reader(bytes, delimiter_byte(&opts.delimiter)?);
        let headers: Vec<ColumnSchema> = rdr
            .headers()
            .context("could not parse CSV")?
            .iter()
            .map(ColumnSchema::untyped)
            .collect();
        Ok(vec![TableSchema {
            name: table_name_from_filename(&row.filename),
            columns: headers,
        }])
    }

    fn network_targets(&self, _opts: &Value) -> Vec<NetworkTarget> {
        vec![]
    }

    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        let org_id = crate::connectors::injected_id(opts, "org_id", "csv")?;
        let opts: CsvOptions = serde_json::from_value(opts.clone())?;
        let path = self
            .materialized_path(org_id, &opts.upload_id, &opts.delimiter)
            .await?;
        run_sql_file(&path, query).await
    }
}

#[cfg(test)]
mod csv_materializer_tests {
    use super::*;

    #[tokio::test]
    async fn sniffs_value_types_and_guards_the_round_trip() {
        let csv = bytes::Bytes::from(
            "id,price,amount,zip,\"full name\"\n1,19.99,10.50,01234,Ann Lee\n,,,,\n".to_string(),
        );
        let dest =
            std::env::temp_dir().join(format!("csv-materializer-test-{}.sqlite", Uuid::new_v4()));
        build_csv_sqlite(csv, b',', "widgets.csv", &dest)
            .await
            .expect("materialization must succeed");

        let result = run_sql_file(&dest, "SELECT * FROM widgets ORDER BY rowid")
            .await
            .expect("query against the materialized file must succeed");
        let _ = tokio::fs::remove_file(&dest).await;

        assert_eq!(
            result.rows[0]["id"],
            json!(1),
            "an integer field must decode as one"
        );
        assert_eq!(
            result.rows[0]["price"],
            json!(19.99),
            "a decimal field must decode as a real"
        );
        assert_eq!(
            result.rows[0]["amount"],
            json!(10.5),
            "a trailing-zero decimal must decode as a real, not text — money amounts commonly \
             carry a trailing zero (10.50, 100.00)"
        );
        assert_eq!(
            result.rows[0]["zip"],
            json!("01234"),
            "a leading-zero value must stay text, not become 1234 (the round-trip guard)"
        );
        assert_eq!(
            result.rows[0]["full name"],
            json!("Ann Lee"),
            "a quoted header containing a space must be addressable"
        );

        assert_eq!(
            result.rows[1]["id"],
            Value::Null,
            "an empty field must decode as null"
        );
        assert_eq!(result.rows[1]["price"], Value::Null);
    }

    #[test]
    fn sniff_cell_tolerates_trailing_zero_decimals_but_still_rejects_leading_zero_integers() {
        match sniff_cell("10.50") {
            CsvCell::Real(v) => assert_eq!(v, 10.5),
            _ => panic!("\"10.50\" must sniff as a real, not text"),
        }
        match sniff_cell("100.00") {
            CsvCell::Real(v) => assert_eq!(v, 100.0),
            _ => panic!("\"100.00\" must sniff as a real, not text"),
        }
        match sniff_cell("01234") {
            CsvCell::Text(s) => assert_eq!(s, "01234"),
            _ => panic!("a leading-zero value must stay text"),
        }
    }

    #[test]
    fn sniff_cell_keeps_non_finite_literals_as_text() {
        // A non-finite SQLite REAL reads back as NULL, so these must stay text.
        for raw in ["inf", "-inf", "NaN"] {
            match sniff_cell(raw) {
                CsvCell::Text(s) => assert_eq!(s, raw),
                _ => panic!("{raw:?} must sniff as text, not a non-finite real"),
            }
        }
    }
}

#[cfg(test)]
mod csv_org_scoping_tests {
    use crate::connectors::injected_id;
    use serde_json::json;

    #[test]
    fn extract_org_id_fails_closed_when_caller_forgot_to_inject_it() {
        let err = injected_id(&json!({"upload_id": "abc"}), "org_id", "csv").unwrap_err();
        assert!(
            err.to_string().contains("caller bug"),
            "must name the caller-bug contract, got: {err}"
        );
    }

    #[test]
    fn extract_org_id_reads_the_injected_value() {
        assert_eq!(
            injected_id(&json!({"org_id": 42}), "org_id", "csv").unwrap(),
            42
        );
    }
}

#[cfg(test)]
mod csv_delimiter_tests {
    use super::delimiter_byte;

    #[test]
    fn comma_default() {
        assert_eq!(delimiter_byte(",").unwrap(), b',');
    }

    #[test]
    fn semicolon() {
        assert_eq!(delimiter_byte(";").unwrap(), b';');
    }

    #[test]
    fn empty_string_falls_back_to_comma() {
        assert_eq!(delimiter_byte("").unwrap(), b',');
    }

    #[test]
    fn non_ascii_character_is_rejected_not_silently_truncated() {
        assert!(delimiter_byte("§").is_err());
    }

    #[test]
    fn multi_char_string_is_rejected() {
        assert!(delimiter_byte("||").is_err());
    }
}
