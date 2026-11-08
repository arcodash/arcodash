//! Content validation for a file-upload data source's bytes, run once at upload time — before
//! anything is written to `ObjectStore` or gets a `data_source_uploads` row.
//!
//! Catches a malformed file (or an unrelated file wearing a `.csv`/`.sqlite` extension) up front
//! rather than at first-query time.

use std::io::Cursor;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use arcodash_core::connectors::sqlite_connector;

/// Default app-level cap on an uploaded file (25MB), overridable via `MAX_UPLOAD_BYTES`. Kept
/// under the ~30MB body limit many reverse proxies and load balancers enforce, so this limit
/// (and the client's own check) is what rejects an oversized file.
const MAX_UPLOAD_BYTES_DEFAULT: u64 = 26_214_400;
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
const CSV_PREVIEW_ROWS: usize = 20;

#[must_use]
pub fn max_upload_bytes() -> u64 {
    arcodash_core::config::env_parsed("MAX_UPLOAD_BYTES", MAX_UPLOAD_BYTES_DEFAULT)
}

/// Deletes its backing file on drop, including on unwind — a `let _ = remove_file(...).await`
/// after the validation call only runs on the non-panicking path, so a panic mid-validation
/// would otherwise leak the temp copy of every upload that triggers it.
struct TempFileGuard(std::path::PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Parses the header row plus up to [`CSV_PREVIEW_ROWS`] data rows.
///
/// Returns them as `(columns, rows)` in the same shape the CSV connector's own `run_query` produces
/// (column `{"name", "friendly_type": "string"}`, row objects keyed by header) — this doubles as
/// validation (a malformed file errors here) so callers get the preview for free, no second parse
/// of the same bytes.
pub fn parse_csv_preview(bytes: &[u8], delimiter: &str) -> Result<(Vec<Value>, Vec<Value>)> {
    let delim = arcodash_core::connectors::http_connectors::delimiter_byte(delimiter)?;
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delim)
        .from_reader(Cursor::new(bytes));
    let headers: Vec<String> = rdr
        .headers()
        .context("could not parse CSV")?
        .iter()
        .map(String::from)
        .collect();
    if headers.is_empty() {
        anyhow::bail!("could not parse CSV: no header row");
    }
    let columns: Vec<Value> = headers
        .iter()
        .map(|h| json!({ "name": h, "friendly_type": "string" }))
        .collect();

    let rows = rdr
        .records()
        .take(CSV_PREVIEW_ROWS)
        .map(|row| {
            let record = row.context("could not parse CSV")?;
            Ok(Value::Object(
                headers
                    .iter()
                    .enumerate()
                    .map(|(i, h)| (h.clone(), record.get(i).map_or(Value::Null, |s| json!(s))))
                    .collect(),
            ))
        })
        .collect::<Result<_>>()?;
    Ok((columns, rows))
}

/// Validates an uploaded `SQLite` file and previews its first table.
///
/// Magic-byte check, then a real read-only open + `SELECT 1` against a temp copy (magic bytes alone
/// don't rule out a truncated/corrupt database), then a preview of the first table read from that
/// same temp file before it's deleted — no DB lookup, no object-store fetch.
///
/// Corruption is a hard error (the whole upload is rejected). A valid-but-empty database (no
/// tables) is not — it succeeds with an empty preview and a warning message, same as a CSV with
/// only a header row.
pub async fn parse_sqlite_preview(
    bytes: &[u8],
) -> Result<(Vec<Value>, Vec<Value>, Option<String>)> {
    if bytes.len() < SQLITE_MAGIC.len() || &bytes[..SQLITE_MAGIC.len()] != SQLITE_MAGIC {
        anyhow::bail!("not a valid SQLite database (bad file signature)");
    }

    let path =
        std::env::temp_dir().join(format!("upload-validate-{}.sqlite", uuid::Uuid::new_v4()));
    tokio::fs::write(&path, bytes)
        .await
        .context("failed to write temp file for SQLite validation")?;
    let guard = TempFileGuard(path);
    sqlite_connector::validate_sqlite_file(&guard.0).await?;
    Ok(
        match sqlite_connector::preview_local_file(&guard.0, CSV_PREVIEW_ROWS).await {
            Ok((columns, rows)) => (columns, rows, None),
            Err(e) => (
                Vec::new(),
                Vec::new(),
                Some(format!("preview unavailable: {e:#}")),
            ),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_csv_preview_returns_columns_and_rows_for_a_well_formed_file() {
        let bytes = b"name,age\nalice,30\nbob,40\n";
        let (columns, rows) = parse_csv_preview(bytes, ",").unwrap();
        let names: Vec<&str> = columns
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["name", "age"]);
        assert!(columns.iter().all(|c| c["friendly_type"] == "string"));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], "alice");
        assert_eq!(rows[0]["age"], "30");
        assert_eq!(rows[1]["name"], "bob");
    }

    #[test]
    fn parse_csv_preview_rejects_an_empty_file() {
        // No header row at all — csv::Reader treats an empty input as an empty (not absent)
        // header record, which our own emptiness check must still catch.
        let err = parse_csv_preview(b"", ",").unwrap_err();
        assert!(err.to_string().contains("could not parse CSV"));
    }

    #[test]
    fn parse_csv_preview_rejects_a_ragged_row_within_the_preview_window() {
        // Second data row has an extra field — csv's default `flexible(false)` reader errors on
        // a record whose length doesn't match the header, which is exactly the "not really a
        // CSV" signal this check exists to catch.
        let bytes = b"a,b\n1,2\n1,2,3\n";
        assert!(parse_csv_preview(bytes, ",").is_err());
    }

    #[test]
    fn parse_csv_preview_respects_a_custom_delimiter() {
        let bytes = b"name;age\nalice;30\n";
        assert!(parse_csv_preview(bytes, ";").is_ok());
        // Parsed as comma-delimited, the whole line is one column — still a valid (if useless)
        // single-column CSV, not an error.
        assert!(parse_csv_preview(bytes, ",").is_ok());
    }

    /// Reuses `http_connectors::delimiter_byte`'s validation rather than a separate, more
    /// permissive ad-hoc first-byte truncation — a multi-byte delimiter must be a clear
    /// configuration error here too, not silently reduced to its first byte.
    #[test]
    fn parse_csv_preview_rejects_an_invalid_delimiter_instead_of_silently_truncating_it() {
        let bytes = b"a,b\n1,2\n";
        let err = parse_csv_preview(bytes, "||").unwrap_err();
        assert!(err.to_string().contains("single ASCII character"));
    }

    #[tokio::test]
    async fn parse_sqlite_preview_rejects_wrong_magic_bytes() {
        let err = parse_sqlite_preview(b"not a sqlite file at all")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bad file signature"));
    }

    // No "right magic bytes, garbage body" test: SQLite defers all page I/O until a query
    // actually needs table data, and `SELECT 1` is a constant needing none — so the open+SELECT 1
    // step never reads far enough into the file to notice corruption past the header. That kind
    // of corruption surfaces later, at the first real query, not here.

    /// Bytes of a fresh on-disk SQLite database after running `statements` against it.
    async fn sqlite_db_bytes(statements: &[&'static str]) -> Vec<u8> {
        use sqlx::sqlite::SqliteConnectOptions;
        use sqlx::ConnectOptions;
        let path = std::env::temp_dir().join(format!(
            "upload-service-test-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        {
            let mut conn = SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .connect()
                .await
                .expect("failed to create test SQLite database");
            for statement in statements {
                sqlx::query(*statement)
                    .execute(&mut conn)
                    .await
                    .expect("failed to run setup statement");
            }
        }
        let bytes = tokio::fs::read(&path)
            .await
            .expect("failed to read test db");
        let _ = tokio::fs::remove_file(&path).await;
        bytes
    }

    #[tokio::test]
    async fn parse_sqlite_preview_returns_table_columns_and_rows_for_a_real_database() {
        let bytes = sqlite_db_bytes(&[
            "CREATE TABLE t (id INTEGER, label TEXT)",
            "INSERT INTO t (id, label) VALUES (1, 'one'), (2, 'two')",
        ])
        .await;
        let (columns, rows, warning) = parse_sqlite_preview(&bytes)
            .await
            .expect("valid database must produce a preview");
        assert!(warning.is_none(), "warning: {warning:?}");
        let names: Vec<&str> = columns
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["id", "label"]);
        assert_eq!(columns[0]["friendly_type"], "integer");
        assert_eq!(columns[1]["friendly_type"], "string");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["id"], 1);
        assert_eq!(rows[0]["label"], "one");
    }

    #[tokio::test]
    async fn parse_sqlite_preview_warns_instead_of_failing_on_a_table_less_database() {
        // A freshly connected, never-written-to file is 0 bytes (SQLite defers the header write
        // until the first real transaction) — create-then-drop forces a real header onto disk so
        // this exercises "valid db, no tables" rather than "not a db at all".
        let bytes = sqlite_db_bytes(&["CREATE TABLE t (id INTEGER)", "DROP TABLE t"]).await;
        let (columns, rows, warning) = parse_sqlite_preview(&bytes)
            .await
            .expect("an empty-but-valid database is not a hard error");
        assert!(columns.is_empty());
        assert!(rows.is_empty());
        assert!(warning.unwrap().contains("no tables found"));
    }

    #[test]
    fn sqlite_preview_temp_file_is_removed_even_when_validation_panics() {
        let path =
            std::env::temp_dir().join(format!("upload-guard-test-{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"x").expect("failed to write scratch file");
        let guard_path = path.clone();
        let result = std::panic::catch_unwind(|| {
            let _guard = TempFileGuard(guard_path);
            panic!("simulated validation panic");
        });
        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn max_upload_bytes_falls_back_to_the_documented_default() {
        std::env::remove_var("MAX_UPLOAD_BYTES");
        assert_eq!(max_upload_bytes(), MAX_UPLOAD_BYTES_DEFAULT);
    }
}
