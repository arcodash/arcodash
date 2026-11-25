//! SQLite connector: uploaded file only, downloaded to a local cache and opened strictly read-only
//! and immutable.
//!
//! Never open a caller-supplied path or a writable connection: that would let an editor read or
//! corrupt arbitrary files.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use rusqlite::fallible_iterator::FallibleIterator;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{Batch, ErrorCode, OpenFlags};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use uuid::Uuid;

use crate::entities::data_source_upload;
use crate::org_storage::OrgStorageResolver;
use crate::storage::{get_upload, upload_read_error};

use super::{
    friendly_type_from_json_sample, ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult,
    QueryRunner, TableSchema,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqliteOptions {
    pub upload_id: String,
}

pub struct SqliteConnector {
    db: DatabaseConnection,
    storage: Arc<dyn OrgStorageResolver>,
    cache_dir: PathBuf,
}

pub(crate) fn upload_cache_dir() -> PathBuf {
    crate::config::env_parsed(
        "UPLOAD_CACHE_DIR",
        std::env::temp_dir().join("arcodash-uploads"),
    )
}

/// How long an unwritten cached upload stays on disk before the next cache-miss prunes it; age is
/// the only backstop since there is no LRU/size cap.
fn upload_cache_max_age() -> std::time::Duration {
    std::time::Duration::from_secs(crate::config::env_parsed(
        "UPLOAD_CACHE_MAX_AGE_SECS",
        7 * 24 * 60 * 60,
    ))
}

/// Best-effort removal of cache entries older than `max_age`; per-entry failures are logged and
/// skipped, never failing the download. Skips `.tmp.` staging names, matched on the filename alone
/// so a cache dir containing `.tmp.` can't disable pruning. Takes `max_age` as a parameter so tests
/// can use a short cap without mutating the shared env var.
async fn prune_stale_cache_files(cache_dir: &Path, max_age: std::time::Duration) {
    let mut entries = match tokio::fs::read_dir(cache_dir).await {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(error = %e, "sqlite upload cache: failed to list cache dir for pruning");
            return;
        }
    };
    let now = std::time::SystemTime::now();
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(error = %e, "sqlite upload cache: failed to read next cache dir entry");
                break;
            }
        };
        if entry.file_name().to_string_lossy().contains(".tmp.") {
            continue;
        }
        let age = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok());
        if age.is_some_and(|age| age > max_age) {
            let path = entry.path();
            if let Err(e) = tokio::fs::remove_file(&path).await {
                tracing::debug!(error = %e, path = %path.display(), "sqlite upload cache: failed to prune a stale cache file");
            }
        }
    }
}

/// Cache filename: the uuid plus a digest of `object_key`, so reassigning an `upload_id` to a new
/// object can't hit a stale cache entry.
fn cache_filename(uuid: &Uuid, object_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(object_key.as_bytes());
    let short_hex = crate::secrets::hex_encode(&digest[..8]);
    format!("{uuid}-{short_hex}.sqlite")
}

impl SqliteConnector {
    pub fn new(db: DatabaseConnection, storage: Arc<dyn OrgStorageResolver>) -> Self {
        Self {
            db,
            storage,
            cache_dir: upload_cache_dir(),
        }
    }

    // No LRU/size cap: [`prune_stale_cache_files`] (age-only, run on every cache miss) is the only
    // eviction.
    /// Resolves the data source's `upload_id` (scoped to its injected `org_id`) to a local file safe
    /// to open read-only. The filename encodes `object_key`, so a reassigned upload re-downloads;
    /// written atomically (temp file + rename).
    async fn local_path(&self, opts: &Value) -> Result<PathBuf> {
        let org_id = super::injected_id(opts, "org_id", "sqlite")?;
        let opts: SqliteOptions =
            serde_json::from_value(opts.clone()).context("invalid SQLite options")?;
        let (uuid, row) = find_upload(&self.db, org_id, &opts.upload_id).await?;

        let dest = self.cache_dir.join(cache_filename(&uuid, &row.object_key));
        if dest.exists() {
            return Ok(dest);
        }

        tokio::fs::create_dir_all(&self.cache_dir)
            .await
            .context("failed to create upload cache dir")?;
        // Only on a miss: cheap at that frequency, wasteful on the hot hit path.
        prune_stale_cache_files(&self.cache_dir, upload_cache_max_age()).await;
        let bytes = get_upload(&*self.storage, &self.db, org_id, &row.object_key)
            .await
            .map_err(|e| upload_read_error(e, "uploaded SQLite database"))?;
        let tmp = self
            .cache_dir
            .join(format!("{uuid}.tmp.{}", Uuid::new_v4()));
        tokio::fs::write(&tmp, &bytes)
            .await
            .context("failed to write SQLite cache temp file")?;
        tokio::fs::rename(&tmp, &dest)
            .await
            .context("failed to finalize SQLite cache file")?;
        Ok(dest)
    }

    async fn open_pool(&self, opts: &Value) -> Result<sqlx::SqlitePool> {
        pool(&self.local_path(opts).await?).await
    }
}

/// Loads the `upload_id` record, scoped to `org_id` so one org can never resolve another's upload.
pub(crate) async fn find_upload(
    db: &DatabaseConnection,
    org_id: i64,
    upload_id: &str,
) -> Result<(Uuid, data_source_upload::Model)> {
    let uuid = Uuid::parse_str(upload_id).context("invalid upload_id")?;
    let row = data_source_upload::Entity::find_by_id(uuid)
        .filter(data_source_upload::Column::OrgId.eq(org_id))
        .one(db)
        .await
        .context("failed to load upload record")?
        .ok_or_else(|| anyhow::anyhow!("upload not found"))?;
    Ok((uuid, row))
}

/// Only for fixed connector-authored SQL (schema introspection, connectivity checks, upload
/// preview), never a query author's text; that goes through [`run_sql_file`] with an authorizer.
async fn pool(path: &Path) -> Result<sqlx::SqlitePool> {
    // sqlx can't set `SQLITE_DBCONFIG_DEFENSIVE` here (as `open_query_conn` does via rusqlite);
    // this pool only runs connector-authored SQL, so `read_only`/`immutable` are its protection.
    let co = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .immutable(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(co)
        .await
        .context("failed to open SQLite database")
}

/// Confirms `path` opens as a SQLite database (magic bytes don't rule out corruption) via the same
/// read-only/immutable open every query uses.
pub async fn validate_sqlite_file(path: &Path) -> Result<()> {
    let p = pool(path).await.context("not a valid SQLite database")?;
    let result = sqlx::query("SELECT 1")
        .execute(&p)
        .await
        .context("not a valid SQLite database");
    p.close().await;
    result.map(|_| ())
}

/// Preview query for a caller-supplied local file (e.g. the upload endpoint's validation temp
/// file), with no `upload_id`, DB lookup or object-store fetch.
pub async fn preview_local_file(path: &Path, limit: usize) -> Result<(Vec<Value>, Vec<Value>)> {
    let p = pool(path).await?;
    let tables_rows =
        sqlx::query("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .fetch_all(&p)
            .await?;
    let table_name: String = tables_rows
        .first()
        .ok_or_else(|| anyhow::anyhow!("no tables found in uploaded database"))?
        .try_get("name")?;
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT * FROM \"{}\" LIMIT {limit}",
        table_name.replace('"', "\"\"")
    )))
    .fetch_all(&p)
    .await
    .context("query failed");
    p.close().await;
    let rows = rows?;
    let json_rows: Vec<Value> = rows.iter().map(row_to_json).collect();
    let columns = rows
        .first()
        .map(|r| {
            r.columns()
                .iter()
                .map(|c| {
                    json!({
                        "name": c.name(),
                        "friendly_type": friendly_type_for(c.type_info().name(), json_rows[0].get(c.name())),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok((columns, json_rows))
}

fn friendly_type(t: &str) -> &'static str {
    let u = t.to_uppercase();
    if u.contains("INT") {
        return "integer";
    }
    if u.contains("REAL") || u.contains("FLOAT") || u.contains("DOUBLE") || u.contains("NUMERIC") {
        return "float";
    }
    if u.contains("DATE") || u.contains("TIME") {
        return "datetime";
    }
    "string"
}

/// Computed expression columns have no declared type, so fall back to the decoded shape of the
/// first row's value rather than labeling them `"string"`.
fn friendly_type_for(declared: &str, sample: Option<&Value>) -> &'static str {
    if declared.is_empty() || declared.eq_ignore_ascii_case("NULL") {
        return friendly_type_from_json_sample(sample);
    }
    friendly_type(declared)
}

/// Decodes column `i` by the value's runtime storage class, not the declared type. `i64` is tried
/// before `f64` so integers stay integers. NULL is checked first: SQLite's int64/double accessors
/// return 0 for NULL, which would turn every NULL into `0`.
fn decode_cell(row: &SqliteRow, i: usize) -> Value {
    if matches!(row.try_get_raw(i), Ok(raw) if raw.is_null()) {
        return Value::Null;
    }
    if let Ok(v) = row.try_get::<i64, _>(i) {
        return json!(v);
    }
    if let Ok(v) = row.try_get::<f64, _>(i) {
        return json!(v);
    }
    if let Ok(Some(v)) = row.try_get::<Option<String>, _>(i) {
        return json!(v);
    }
    Value::Null
}

fn row_to_json(row: &SqliteRow) -> Value {
    let cells = row.columns().iter().enumerate();
    Value::Object(
        cells
            .map(|(i, col)| (col.name().to_string(), decode_cell(row, i)))
            .collect(),
    )
}

#[async_trait]
impl QueryRunner for SqliteConnector {
    fn type_name(&self) -> &'static str {
        "sqlite"
    }

    fn configuration_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "upload_id": {
                    "type": "file",
                    "title": "SQLite database",
                    "accept": ".sqlite,.db,.sqlite3"
                }
            },
            "required": ["upload_id"],
            "secret": []
        })
    }

    async fn test_connection(&self, opts: &Value) -> Result<()> {
        let p = self.open_pool(opts).await?;
        sqlx::query("SELECT 1")
            .execute(&p)
            .await
            .context("test failed")?;
        p.close().await;
        Ok(())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let p = self.open_pool(opts).await?;
        let tables_rows =
            sqlx::query("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&p)
                .await?;
        let mut tables = Vec::new();
        for tr in tables_rows {
            let tname: String = tr.try_get("name")?;
            let col_rows = sqlx::query(sqlx::AssertSqlSafe(format!(
                "PRAGMA table_info({})",
                quote_ident(&tname)
            )))
            .fetch_all(&p)
            .await?;
            let columns: Vec<ColumnSchema> = col_rows
                .iter()
                .map(|r| {
                    let name = r.try_get::<String, _>("name").unwrap_or_default();
                    // SQLite allows columns with no declared type.
                    let type_name = r.try_get::<String, _>("type").unwrap_or_default();
                    ColumnSchema::typed_or_blank(name, type_name)
                })
                .collect();
            tables.push(TableSchema {
                name: tname,
                columns,
            });
        }
        p.close().await;
        Ok(tables)
    }

    async fn get_view_names(&self, opts: &Value) -> Result<Vec<String>> {
        let p = self.open_pool(opts).await?;
        let names = sqlite_view_names(&p).await;
        p.close().await;
        names
    }

    async fn get_foreign_keys(&self, opts: &Value) -> Result<Vec<super::ForeignKey>> {
        let p = self.open_pool(opts).await?;
        let fks = sqlite_foreign_keys(&p).await;
        p.close().await;
        fks
    }

    fn network_targets(&self, _opts: &Value) -> Vec<NetworkTarget> {
        vec![]
    }

    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        run_sql_file(&self.local_path(opts).await?, query).await
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

async fn sqlite_foreign_keys(p: &sqlx::SqlitePool) -> Result<Vec<super::ForeignKey>> {
    let tables = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
    )
    .fetch_all(p)
    .await?;
    let mut out = Vec::new();
    for t in tables {
        let from_table: String = t.try_get("name")?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "PRAGMA foreign_key_list({})",
            quote_ident(&from_table)
        )))
        .fetch_all(p)
        .await?;
        for r in rows {
            let to_table: String = r.try_get("table")?;
            let Ok(from_column) = r.try_get::<String, _>("from") else {
                continue;
            };
            // A NULL `to` means the FK targets the parent's primary key implicitly.
            let to_column = if let Some(c) = r.try_get::<Option<String>, _>("to")? {
                c
            } else {
                let cols = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "PRAGMA table_info({})",
                    quote_ident(&to_table)
                )))
                .fetch_all(p)
                .await?;
                let pk = cols
                    .iter()
                    .find(|c| c.try_get::<i64, _>("pk").unwrap_or(0) == 1)
                    .and_then(|c| c.try_get::<String, _>("name").ok());
                match pk {
                    Some(c) => c,
                    None => continue,
                }
            };
            out.push(super::ForeignKey {
                from_table: from_table.clone(),
                from_column,
                to_table,
                to_column,
            });
        }
    }
    Ok(out)
}

async fn sqlite_view_names(p: &sqlx::SqlitePool) -> Result<Vec<String>> {
    sqlx::query("SELECT name FROM sqlite_master WHERE type='view' ORDER BY name")
        .fetch_all(p)
        .await?
        .iter()
        .map(|r| Ok(r.try_get("name")?))
        .collect()
}

/// Pragmas with process-global effect (heap limits, `temp_store_directory`/`data_store_directory`),
/// which one org's query could use to degrade or wedge every other connection.
const DENIED_GLOBAL_PRAGMAS: &[&str] = &[
    "hard_heap_limit",
    "soft_heap_limit",
    "temp_store_directory",
    "data_store_directory",
];

/// Refuses `SQLITE_ATTACH` outright, which also denies `VACUUM INTO` (implemented internally as an
/// ATTACH through the same authorizer), and the pragmas in [`DENIED_GLOBAL_PRAGMAS`]. Everything
/// else is allowed: the read-only flag and `SQLITE_DBCONFIG_DEFENSIVE` already stop mutation, so
/// this only polices what reaches outside the connection.
fn deny_attach(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Attach { .. } => Authorization::Deny,
        AuthAction::Pragma { pragma_name, .. }
            if DENIED_GLOBAL_PRAGMAS
                .iter()
                .any(|p| p.eq_ignore_ascii_case(pragma_name)) =>
        {
            Authorization::Deny
        }
        _ => Authorization::Allow,
    }
}

/// Opens `path` read-only/immutable with [`deny_attach`] as the authorizer. Uses rusqlite because
/// sqlx has no `sqlite3_set_authorizer`. The authorizer runs on SQLite's real parser, so
/// dialect-confusing payloads can't hide ATTACH/VACUUM INTO the way they could from a keyword
/// scanner.
fn open_query_conn(path: &Path) -> Result<rusqlite::Connection> {
    let uri = format!("file:{}?immutable=1", path.display());
    let conn = rusqlite::Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("failed to open SQLite database")?;
    conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .context("failed to enable defensive mode")?;
    conn.authorizer(Some(deny_attach))
        .context("failed to install authorizer")?;
    Ok(conn)
}

/// Turns a [`deny_attach`] denial into the connector's own message rather than SQLite's generic
/// "not authorized"; other errors keep their message under "query failed".
fn map_sqlite_err(err: rusqlite::Error) -> anyhow::Error {
    let is_auth_denial = matches!(
        &err,
        rusqlite::Error::SqliteFailure(e, _) if e.code == ErrorCode::AuthorizationForStatementDenied
    );
    if is_auth_denial {
        anyhow::anyhow!(
            "this statement is not permitted against an uploaded-file data source \
             (ATTACH, VACUUM INTO, and a small set of process-global pragmas are blocked)"
        )
    } else {
        anyhow::Error::new(err).context("query failed")
    }
}

/// Decodes a cell by its runtime storage class, which rusqlite exposes directly (see
/// [`decode_cell`] for the sqlx paths). Blobs decode as `null`.
pub(crate) fn sqlite_valueref_to_json(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null | ValueRef::Blob(_) => Value::Null,
        ValueRef::Integer(n) => json!(n),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
    }
}

/// Runs the SQL text against `path`, shared by every uploaded-file connector. `path` must be a file
/// this process created in its own cache directory, never caller-supplied. `query` may hold several
/// `;`-separated statements, each run through the same authorizer via [`Batch`]; a rejection aborts
/// the rest.
pub(crate) async fn run_sql_file(path: &Path, query: &str) -> Result<QueryExecutionResult> {
    let path = path.to_path_buf();
    let query = query.to_string();
    tokio::task::spawn_blocking(move || run_sql_file_blocking(&path, &query))
        .await
        .context("sqlite query task panicked")?
}

fn run_sql_file_blocking(path: &Path, query: &str) -> Result<QueryExecutionResult> {
    let conn = open_query_conn(path).map_err(super::connect_err)?;
    let start = Instant::now();
    let mut rows_out: Vec<Value> = Vec::new();
    let mut columns: Vec<ColumnMeta> = Vec::new();
    let mut batch = Batch::new(&conn, query);
    while let Some(mut stmt) = batch.next().map_err(map_sqlite_err)? {
        let col_info: Vec<(String, Option<String>)> = stmt
            .columns()
            .iter()
            .map(|c| (c.name().to_string(), c.decl_type().map(str::to_string)))
            .collect();
        let mut result_rows = stmt.query([]).map_err(map_sqlite_err)?;
        let mut statement_rows = Vec::new();
        while let Some(row) = result_rows.next().map_err(map_sqlite_err)? {
            let obj = col_info
                .iter()
                .enumerate()
                .map(|(i, (name, _))| {
                    let v = row
                        .get_ref(i)
                        .context("failed to read query result column")?;
                    Ok((name.clone(), sqlite_valueref_to_json(v)))
                })
                .collect::<Result<serde_json::Map<_, _>>>()?;
            statement_rows.push(Value::Object(obj));
        }
        if columns.is_empty() && !statement_rows.is_empty() {
            columns = col_info
                .iter()
                .map(|(name, decl_type)| ColumnMeta {
                    name: name.clone(),
                    friendly_type: friendly_type_for(
                        decl_type.as_deref().unwrap_or(""),
                        statement_rows[0].get(name),
                    )
                    .to_string(),
                })
                .collect();
        }
        rows_out.extend(statement_rows);
    }
    Ok(QueryExecutionResult {
        columns,
        rows: rows_out,
        runtime_seconds: start.elapsed().as_secs_f64(),
    })
}

#[cfg(test)]
mod org_scoping_tests {
    use super::super::injected_id;
    use serde_json::json;

    #[test]
    fn extract_org_id_fails_closed_when_caller_forgot_to_inject_it() {
        let err = injected_id(&json!({"upload_id": "abc"}), "org_id", "sqlite").unwrap_err();
        assert!(
            err.to_string().contains("caller bug"),
            "must name the caller-bug contract, got: {err}"
        );
    }

    #[test]
    fn extract_org_id_reads_the_injected_value() {
        assert_eq!(
            injected_id(&json!({"org_id": 42}), "org_id", "sqlite").unwrap(),
            42
        );
    }
}

#[cfg(test)]
mod prune_stale_cache_files_tests {
    // `max_age` is passed directly so these tests never touch process env.
    use super::prune_stale_cache_files;
    use std::path::PathBuf;
    use std::time::Duration;

    async fn temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{prefix}{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        dir
    }

    #[tokio::test]
    async fn prunes_entries_past_max_age_but_keeps_tmp_staging_files() {
        let dir = temp_dir("sqlite-cache-prune-test-").await;
        let stale = dir.join("stale-entry.sqlite");
        let staging = dir.join("uuid.tmp.other-uuid");
        tokio::fs::write(&stale, b"data").await.unwrap();
        tokio::fs::write(&staging, b"partial").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        prune_stale_cache_files(&dir, Duration::ZERO).await;

        assert!(
            !stale.exists(),
            "an entry older than the cap must be pruned"
        );
        assert!(
            staging.exists(),
            "a `.tmp.` staging file mid-download must never be pruned"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_fresh_entry_under_a_generous_cap_survives() {
        let dir = temp_dir("sqlite-cache-prune-test-").await;
        let fresh = dir.join("fresh-entry.sqlite");
        tokio::fs::write(&fresh, b"data").await.unwrap();

        prune_stale_cache_files(&dir, Duration::from_secs(7 * 24 * 60 * 60)).await;

        assert!(
            fresh.exists(),
            "a fresh entry under the default cap must survive"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A cache directory whose own path contains `.tmp.` must not disable pruning; only the entry
    /// filename is checked.
    #[tokio::test]
    async fn a_tmp_substring_in_the_directory_path_does_not_block_pruning() {
        let dir = temp_dir("sqlite-cache-prune-test.tmp.").await;
        let stale = dir.join("stale-entry.sqlite");
        tokio::fs::write(&stale, b"data").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        prune_stale_cache_files(&dir, Duration::ZERO).await;

        assert!(
            !stale.exists(),
            "a real cache entry must still be pruned even when the directory path contains `.tmp.`"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}

#[cfg(test)]
mod cache_filename_tests {
    use super::cache_filename;
    use uuid::Uuid;

    #[test]
    fn different_object_keys_for_the_same_uuid_produce_different_filenames() {
        let uuid = Uuid::new_v4();
        let a = cache_filename(&uuid, "uploads/one.sqlite");
        let b = cache_filename(&uuid, "uploads/two.sqlite");
        assert_ne!(a, b);
    }

    #[test]
    fn the_same_object_key_produces_a_stable_filename() {
        let uuid = Uuid::new_v4();
        let a = cache_filename(&uuid, "uploads/one.sqlite");
        let b = cache_filename(&uuid, "uploads/one.sqlite");
        assert_eq!(a, b);
    }
}

#[cfg(test)]
mod runtime_decode_tests {
    use super::*;
    use sqlx::ConnectOptions;

    async fn build_test_db(setup_sql: &[&str]) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("sqlite-connector-test-{}.sqlite", Uuid::new_v4()));
        let mut conn = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .connect()
            .await
            .expect("create test sqlite db");
        for stmt in setup_sql {
            sqlx::query(sqlx::AssertSqlSafe(*stmt))
                .execute(&mut conn)
                .await
                .expect("setup statement");
        }
        path
    }

    /// `SQLITE_DBCONFIG_DEFENSIVE` has a query-back form, so this asserts it took effect on the
    /// `run_sql_file` connection.
    #[tokio::test]
    async fn defensive_mode_is_actually_enabled_on_the_query_connection() {
        let path = build_test_db(&["CREATE TABLE t (x INTEGER)"]).await;

        let query_conn = open_query_conn(&path).expect("open query connection");
        assert!(
            query_conn
                .db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE)
                .unwrap(),
            "SQLITE_DBCONFIG_DEFENSIVE must be enabled on the connection run_sql_file uses"
        );
        drop(query_conn);
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn foreign_keys_include_declared_and_implicit_pk_references() {
        let path = build_test_db(&[
            "CREATE TABLE customers (id INTEGER PRIMARY KEY)",
            "CREATE TABLE payments (id INTEGER PRIMARY KEY, customer_id INTEGER REFERENCES customers(id))",
            "CREATE TABLE refunds (id INTEGER PRIMARY KEY, \"payment id\" INTEGER REFERENCES payments)",
        ])
        .await;
        let p = pool(&path).await.unwrap();
        let fks = sqlite_foreign_keys(&p).await.unwrap();
        p.close().await;
        let _ = tokio::fs::remove_file(&path).await;
        let got: Vec<_> = fks
            .iter()
            .map(|f| {
                (
                    f.from_table.as_str(),
                    f.from_column.as_str(),
                    f.to_table.as_str(),
                    f.to_column.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("payments", "customer_id", "customers", "id"),
                ("refunds", "payment id", "payments", "id"),
            ]
        );
    }

    #[tokio::test]
    async fn view_names_lists_views_not_tables() {
        let path = build_test_db(&[
            "CREATE TABLE t (a INTEGER)",
            "CREATE VIEW v AS SELECT a FROM t",
        ])
        .await;
        let p = pool(&path).await.unwrap();
        let names = sqlite_view_names(&p).await.unwrap();
        p.close().await;
        let _ = tokio::fs::remove_file(&path).await;
        assert_eq!(names, vec!["v".to_string()]);
    }

    #[tokio::test]
    async fn aggregate_sum_and_count_decode_as_integers() {
        let path = build_test_db(&[
            "CREATE TABLE widgets (qty INTEGER)",
            "INSERT INTO widgets (qty) VALUES (3), (4)",
        ])
        .await;
        let result = run_sql_file(
            &path,
            "SELECT sum(qty) AS total_qty, count(*) AS n FROM widgets",
        )
        .await
        .expect("aggregate query must decode");
        let _ = tokio::fs::remove_file(&path).await;
        assert_eq!(result.rows[0]["total_qty"], json!(7));
        assert_eq!(result.rows[0]["n"], json!(2));
        assert!(result.rows[0]["total_qty"].is_i64());
        assert!(result.rows[0]["n"].is_i64());
    }

    #[tokio::test]
    async fn avg_decodes_as_float_text_expression_as_string_null_as_null() {
        let path = build_test_db(&[
            "CREATE TABLE widgets (qty INTEGER, name TEXT)",
            "INSERT INTO widgets (qty, name) VALUES (3, 'a'), (5, 'b')",
        ])
        .await;

        let avg_result = run_sql_file(&path, "SELECT avg(qty) AS avg_qty FROM widgets")
            .await
            .unwrap();
        assert!(avg_result.rows[0]["avg_qty"].is_f64());

        let text_result = run_sql_file(
            &path,
            "SELECT name || '!' AS shout FROM widgets ORDER BY name LIMIT 1",
        )
        .await
        .unwrap();
        assert_eq!(text_result.rows[0]["shout"], json!("a!"));

        let null_result = run_sql_file(&path, "SELECT NULL AS x").await.unwrap();
        assert_eq!(null_result.rows[0]["x"], Value::Null);

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// Aggregate/expression decoding must not regress the plain-column path.
    #[tokio::test]
    async fn plain_columns_with_declared_types_still_decode_correctly() {
        let path = build_test_db(&[
            "CREATE TABLE t (i INTEGER, r REAL, s TEXT, n INTEGER)",
            "INSERT INTO t (i, r, s, n) VALUES (1, 1.5, 'hi', NULL)",
        ])
        .await;
        let result = run_sql_file(&path, "SELECT * FROM t").await.unwrap();
        let _ = tokio::fs::remove_file(&path).await;
        assert_eq!(result.rows[0]["i"], json!(1));
        assert_eq!(result.rows[0]["r"], json!(1.5));
        assert_eq!(result.rows[0]["s"], json!("hi"));
        assert_eq!(result.rows[0]["n"], Value::Null);
    }

    #[tokio::test]
    async fn friendly_type_labels_aggregates_by_value_and_declared_columns_by_decltype() {
        let path = build_test_db(&[
            "CREATE TABLE widgets (qty INTEGER, label TEXT)",
            "INSERT INTO widgets (qty, label) VALUES (3, 'a'), (5, 'b')",
        ])
        .await;
        let result = run_sql_file(
            &path,
            "SELECT sum(qty) AS total_qty, avg(qty) AS avg_qty, label FROM widgets GROUP BY label LIMIT 1",
        )
        .await
        .unwrap();
        let _ = tokio::fs::remove_file(&path).await;
        let by_name = |name: &str| {
            result
                .columns
                .iter()
                .find(|c| c.name == name)
                .unwrap()
                .friendly_type
                .clone()
        };
        assert_eq!(by_name("total_qty"), "integer");
        assert_eq!(by_name("avg_qty"), "float");
        assert_eq!(by_name("label"), "string");
    }

    #[tokio::test]
    async fn run_sql_file_runs_multiple_statements_and_trailing_forms() {
        let path = build_test_db(&["CREATE TABLE t (x INTEGER)"]).await;
        let multi = run_sql_file(&path, "SELECT 0 AS x; SELECT 1 AS x")
            .await
            .unwrap();
        let semi = run_sql_file(&path, "SELECT 2 AS x;").await.unwrap();
        let comment = run_sql_file(&path, "SELECT 3 AS x; -- done").await.unwrap();
        let _ = tokio::fs::remove_file(&path).await;
        assert_eq!(multi.rows.len(), 2);
        assert_eq!(multi.rows[1]["x"], json!(1));
        assert_eq!(semi.rows[0]["x"], json!(2));
        assert_eq!(comment.rows[0]["x"], json!(3));
    }

    #[tokio::test]
    async fn run_sql_file_rejects_an_attach_statement() {
        let path = build_test_db(&["CREATE TABLE widgets (qty INTEGER)"]).await;
        let err = run_sql_file(
            &path,
            "ATTACH DATABASE 'file:/etc/passwd?mode=rw' AS x; SELECT 1",
        )
        .await
        .unwrap_err();
        let _ = tokio::fs::remove_file(&path).await;
        assert!(err.to_string().contains("not permitted"));
    }

    #[tokio::test]
    async fn run_sql_file_rejects_a_vacuum_into_statement() {
        let path = build_test_db(&["CREATE TABLE widgets (qty INTEGER)"]).await;
        let target = std::env::temp_dir().join(format!("sqlite-connector-pwn-{}", Uuid::new_v4()));
        let query = format!("VACUUM INTO '{}'", target.display());
        let err = run_sql_file(&path, &query).await.unwrap_err();
        let _ = tokio::fs::remove_file(&path).await;
        assert!(err.to_string().contains("not permitted"));
        assert!(
            !target.exists(),
            "VACUUM INTO must not have written the target file"
        );
    }

    #[tokio::test]
    async fn run_sql_file_still_allows_an_ordinary_select_after_the_attach_check() {
        let path = build_test_db(&[
            "CREATE TABLE widgets (qty INTEGER)",
            "INSERT INTO widgets (qty) VALUES (1)",
        ])
        .await;
        let result = run_sql_file(&path, "SELECT qty FROM widgets")
            .await
            .unwrap();
        let _ = tokio::fs::remove_file(&path).await;
        assert_eq!(result.rows[0]["qty"], json!(1));
    }

    /// Adversarial: a `$x$...$x$` body in `[...]` (a real SQLite identifier quote) looks like a
    /// Postgres dollar-quoted string to a foreign-dialect scanner but is executable SQLite; the
    /// authorizer can't be fooled.
    #[tokio::test]
    async fn run_sql_file_rejects_vacuum_into_disguised_by_a_bracket_quoted_dollar_string() {
        let path = build_test_db(&["CREATE TABLE widgets (qty INTEGER)"]).await;
        let target = std::env::temp_dir().join(format!("sqlite-connector-pwn-{}", Uuid::new_v4()));
        let query = format!(
            "SELECT 1 FROM (SELECT 1) AS [$x$] ; VACUUM INTO '{}' ; SELECT 1 FROM (SELECT 1) AS [$x$]",
            target.display()
        );
        let err = run_sql_file(&path, &query).await.unwrap_err();
        let _ = tokio::fs::remove_file(&path).await;
        assert!(err.to_string().contains("not permitted"), "got: {err}");
        assert!(
            !target.exists(),
            "VACUUM INTO must not have written the target file"
        );
    }

    /// Adversarial: SQLite block comments don't nest, so `/* /* */` closes at the first `*/` and
    /// the `VACUUM INTO` after it is live code.
    #[tokio::test]
    async fn run_sql_file_rejects_vacuum_into_disguised_by_a_non_nesting_comment() {
        let path = build_test_db(&["CREATE TABLE widgets (qty INTEGER)"]).await;
        let target = std::env::temp_dir().join(format!("sqlite-connector-pwn-{}", Uuid::new_v4()));
        let query = format!("/* /* */ ; VACUUM INTO '{}' ; --*/", target.display());
        let err = run_sql_file(&path, &query).await.unwrap_err();
        let _ = tokio::fs::remove_file(&path).await;
        assert!(err.to_string().contains("not permitted"), "got: {err}");
        assert!(
            !target.exists(),
            "VACUUM INTO must not have written the target file"
        );
    }

    /// Adversarial: a read-only `ATTACH` (`mode=ro`) opens a second connection with its own mode;
    /// the authorizer denies `SQLITE_ATTACH` unconditionally.
    #[tokio::test]
    async fn run_sql_file_rejects_a_read_only_attach() {
        let path = build_test_db(&["CREATE TABLE widgets (qty INTEGER)"]).await;
        let secret = build_test_db(&["CREATE TABLE secrets (v TEXT)"]).await;

        let query = format!("ATTACH 'file:{}?mode=ro' AS x", secret.display());
        let err = run_sql_file(&path, &query).await.unwrap_err();
        let _ = tokio::fs::remove_file(&path).await;
        let _ = tokio::fs::remove_file(&secret).await;
        assert!(err.to_string().contains("not permitted"), "got: {err}");
    }

    /// `hard_heap_limit` is a process-wide cap that would degrade every org's connections; must be
    /// denied like ATTACH.
    #[tokio::test]
    async fn run_sql_file_rejects_a_process_global_pragma() {
        let path = build_test_db(&["CREATE TABLE widgets (qty INTEGER)"]).await;
        let err = run_sql_file(&path, "PRAGMA hard_heap_limit = 1")
            .await
            .unwrap_err();
        let _ = tokio::fs::remove_file(&path).await;
        assert!(err.to_string().contains("not permitted"), "got: {err}");
    }

    /// Denying process-global pragmas must not deny pragmas generally: `table_info` still works.
    #[tokio::test]
    async fn run_sql_file_still_allows_an_ordinary_pragma() {
        let path = build_test_db(&["CREATE TABLE widgets (qty INTEGER)"]).await;
        let result = run_sql_file(&path, "PRAGMA table_info('widgets')")
            .await
            .unwrap();
        let _ = tokio::fs::remove_file(&path).await;
        assert_eq!(result.rows[0]["name"], json!("qty"));
    }
}
