//! "Query Results" meta-connector: lets users JOIN the cached results of other queries they may
//! view.
//!
//! Each `run_query` loads an in-memory SQLite with one table per authorized cached result
//! (`query_<id>`) and runs the user's SQL against it.
//!
//! The visible table set depends on the caller, so this schema is never persisted to the shared
//! `data_sources.schema` cache.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use sea_orm::DatabaseConnection;
use serde_json::{json, Value};

use super::{
    friendly_type_from_json_sample, ColumnMeta, ColumnSchema, NetworkTarget, QueryExecutionResult,
    QueryRunner, TableSchema,
};

/// Max distinct cached queries loaded into the in-memory SQLite (bounds `query_<id>` tables, not
/// rows; see [`MAX_ROWS`]).
const MAX_RESULTS: u64 = 500;

/// Hard ceiling on returned rows; backstop for SQL `apply_auto_limit` won't rewrite. Set above the
/// worker's row cap so its truncation is the normal bound.
const MAX_ROWS: usize = 50_000;

pub struct QueryResultsConnector {
    db: DatabaseConnection,
}

impl QueryResultsConnector {
    #[must_use]
    pub const fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// The datasets visible to the caller whose ids [`inject_caller`] put in `opts`.
    async fn load_for_caller(&self, opts: &Value) -> Result<Vec<ResultData>> {
        let org_id = super::injected_id(opts, "org_id", "query_results")?;
        let user_id = super::injected_id(opts, "user_id", "query_results")?;
        load_result_data(&self.db, org_id, user_id).await
    }
}

/// Parsed representation of the `data` JSONB column inside a `query_results` row.
struct ResultData {
    query_id: i64,
    columns: Vec<ColumnMeta>,
    rows: Vec<Value>,
}

/// Loads the most recent `data`-populated result per query, joined to `queries` for the table name
/// and authorization. `org_id` scopes both sides (join condition and WHERE): `query_hash` isn't
/// secret or org-scoped, so identical SQL in two orgs hashes identically and a one-sided filter
/// would leak another org's cached data. The DB role bypasses RLS, so this is the only org
/// boundary.
///
/// Within an org a row is returned only if `authz::access::can_view_query_like` would allow it:
/// explicit reach, or published with workspace/public visibility and data-source scope over
/// `qr.data_source_id` (which the join requires to be one of the query's data sources). The rule is
/// reimplemented in SQL because it must filter candidate rows;
/// `query_results_authz_agreement_tests` keeps the two in step.
async fn load_result_data(
    db: &DatabaseConnection,
    org_id: i64,
    user_id: i64,
) -> Result<Vec<ResultData>> {
    // Raw SQL (DISTINCT ON, array containment and the access CTEs are awkward in sea-orm): joins
    // query_results to queries on query_hash and data_source_id, picks the latest non-null row per
    // query, limits to MAX_RESULTS.
    use sea_orm::{ConnectionTrait, DbBackend, Statement};

    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        r"
                WITH my_groups AS (
                    SELECT g.id, g.data_source_access_restricted
                    FROM users u
                    JOIN groups g ON g.org_id = u.org_id AND g.id = ANY(u.groups)
                    WHERE u.id = $2 AND u.org_id = $1
                ),
                my_admin AS (
                    SELECT EXISTS (
                        SELECT 1
                        FROM my_groups mg
                        JOIN group_roles gr ON gr.group_id = mg.id
                        JOIN roles r ON r.id = gr.role_id
                        WHERE 'admin' = ANY(r.permissions)
                    ) AS is_admin
                ),
                my_unrestricted AS (
                    SELECT EXISTS (
                        SELECT 1 FROM my_groups WHERE NOT data_source_access_restricted
                    ) AS any_unrestricted
                )
                SELECT DISTINCT ON (q.id)
                    q.id          AS query_id,
                    qr.data       AS data
                FROM query_results qr
                JOIN queries q
                  ON  q.query_hash      = qr.query_hash
                  AND q.org_id          = qr.org_id
                  AND qr.data_source_id IN (q.data_source_id, COALESCE(q.published_data_source_id, q.data_source_id))
                CROSS JOIN my_admin
                CROSS JOIN my_unrestricted
                WHERE qr.data IS NOT NULL
                  AND qr.org_id = $1
                  AND q.org_id = $1
                  AND (
                    -- explicit reach: owner, admin, or a recognized access_permissions grant
                    q.user_id = $2
                    OR my_admin.is_admin
                    OR EXISTS (
                        SELECT 1 FROM access_permissions ap
                        WHERE ap.object_type = 'query'
                          AND ap.object_id   = q.id
                          AND ap.access_type IN ('owner', 'editor', 'modify', 'viewer', 'view')
                          AND (
                              ap.grantee_id = $2
                              OR ap.grantee_group_id IN (SELECT id FROM my_groups)
                          )
                    )
                    OR (
                        -- org-open default: published + workspace/public visibility, gated on
                        -- data-source scope over the row's own data source
                        q.published_change_id IS NOT NULL
                        AND q.unpublished_at IS NULL
                        AND q.visibility IN ('workspace', 'public')
                        AND (
                            my_unrestricted.any_unrestricted
                            OR EXISTS (
                                SELECT 1 FROM data_source_groups dsg
                                WHERE dsg.data_source_id = qr.data_source_id
                                  AND dsg.group_id IN (SELECT id FROM my_groups)
                            )
                        )
                    )
                  )
                ORDER BY q.id, qr.retrieved_at DESC
                LIMIT $3
        ",
        [org_id.into(), user_id.into(), MAX_RESULTS.into()],
    );

    let rows = db
        .query_all_raw(stmt)
        .await
        .context("failed to load query results for meta-connector")?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let query_id: i64 = row.try_get("", "query_id").ok()?;
            let data: Value = row.try_get("", "data").ok()?;
            let columns = parse_columns(&data);
            if columns.is_empty() {
                // A zero-column table isn't creatable in SQLite; warn instead of failing later
                // with a bare "no such table".
                tracing::warn!(
                    query_id,
                    org_id,
                    "query_results connector: skipping query with no columns in its cached result"
                );
                return None;
            }
            Some(ResultData {
                query_id,
                columns,
                rows: data["rows"].as_array().cloned().unwrap_or_default(),
            })
        })
        .collect())
}

fn parse_columns(data: &Value) -> Vec<ColumnMeta> {
    data["columns"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let name = c["name"].as_str()?.to_string();
                    let friendly_type = c["friendly_type"].as_str().unwrap_or("string").to_string();
                    Some(ColumnMeta {
                        name,
                        friendly_type,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Injects the caller's identity into connector options.
///
/// Applies to connectors whose org/ownership can't be expressed in user config: `query_results`
/// gets `org_id` and `user_id`, `csv`/`sqlite` get `org_id`, others are untouched.
///
/// Call before `run_query`/`get_schema`/`test_connection` at every site that invokes these
/// connectors.
#[must_use]
pub fn inject_caller(mut options: Value, type_name: &str, org_id: i64, user_id: i64) -> Value {
    let with_user = match type_name {
        "query_results" => true,
        "csv" | "sqlite" => false,
        _ => return options,
    };
    if let Some(obj) = options.as_object_mut() {
        obj.insert("org_id".to_string(), json!(org_id));
        if with_user {
            obj.insert("user_id".to_string(), json!(user_id));
        }
    }
    options
}

/// Prepares one user statement. A trailing `;` or comment is fine; a second statement is a
/// user-facing error.
pub(crate) fn prepare_single<'c>(
    conn: &'c rusqlite::Connection,
    query: &str,
) -> Result<rusqlite::Statement<'c>> {
    conn.prepare(query).map_err(|e| match e {
        rusqlite::Error::MultipleStatement => {
            anyhow::anyhow!("Only one SQL statement is allowed")
        }
        e => e.into(),
    })
}

/// In-memory SQLite connection for running user SQL over loaded rows (also used by the JSON
/// connector). URI filenames stay off and DEFENSIVE mode is on, so user SQL can't `ATTACH` files.
pub(crate) fn open_sqlite_conn() -> Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_in_memory_with_flags(
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
    )
    .context("failed to open in-memory SQLite")?;
    conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .context("failed to enable SQLite defensive mode")?;
    Ok(conn)
}

/// Stops the SQLite thread's work when this future is dropped (a dropped `spawn_blocking` handle
/// detaches the thread). `sqlite3_interrupt` only aborts a running statement, so `cancelled` also
/// stops the population loop. Also guards the JSON connector, which has no population loop to
/// stop.
pub(crate) struct InterruptOnDrop {
    pub(crate) handle: rusqlite::InterruptHandle,
    pub(crate) cancelled: Arc<AtomicBool>,
}

impl Drop for InterruptOnDrop {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.handle.interrupt();
    }
}

/// Shared text for the cancellation checkpoints below.
const CANCELLED_MSG: &str =
    "query_results query was cancelled before it finished loading cached results";

/// Populates `conn` with the result data and runs the user's SQL on a blocking thread; `cancelled`
/// is checked between statements, where an interrupt would be lost.
#[allow(
    clippy::needless_pass_by_value,
    reason = "takes ownership of what the caller moves into its spawn_blocking task"
)]
fn run_on_sqlite(
    conn: rusqlite::Connection,
    datasets: Vec<ResultData>,
    query: &str,
    cancelled: &AtomicBool,
) -> Result<QueryExecutionResult> {
    for dataset in &datasets {
        if cancelled.load(Ordering::Relaxed) {
            anyhow::bail!(CANCELLED_MSG);
        }
        let table_name = format!("query_{}", dataset.query_id);

        // No declared column type: a declared TEXT column would convert bound Integer/Real values
        // to text.
        let col_defs: Vec<String> = dataset
            .columns
            .iter()
            .map(|c| format!("\"{}\"", c.name.replace('"', "\"\"")))
            .collect();
        let create_sql = format!("CREATE TABLE \"{table_name}\" ({})", col_defs.join(", "));
        conn.execute(&create_sql, [])
            .with_context(|| format!("failed to create SQLite table {table_name}"))?;

        if !dataset.rows.is_empty() {
            let insert_sql = format!(
                "INSERT INTO \"{table_name}\" VALUES ({})",
                vec!["?"; dataset.columns.len()].join(", ")
            );
            let mut stmt = conn
                .prepare(&insert_sql)
                .with_context(|| format!("failed to prepare INSERT for {table_name}"))?;

            for row in &dataset.rows {
                if cancelled.load(Ordering::Relaxed) {
                    anyhow::bail!(CANCELLED_MSG);
                }
                let values = dataset
                    .columns
                    .iter()
                    .map(|col| json_to_sqlite_value(&row[&col.name]));
                stmt.execute(rusqlite::params_from_iter(values))
                    .with_context(|| format!("failed to insert row into {table_name}"))?;
            }
        }
    }

    // Execute the user's query. SQLite ignores an interrupt issued before a statement starts, so
    // re-check cancellation here.
    if cancelled.load(Ordering::Relaxed) {
        anyhow::bail!(CANCELLED_MSG);
    }
    let started = Instant::now();
    let mut stmt = prepare_single(&conn, query).context("failed to prepare user query")?;

    let col_names: Vec<String> = stmt
        .column_names()
        .iter()
        .map(std::string::ToString::to_string)
        .collect();

    let mut result_rows: Vec<Value> = Vec::new();
    let rows = stmt
        .query_map([], |row| {
            let mut obj = serde_json::Map::with_capacity(col_names.len());
            for (i, name) in col_names.iter().enumerate() {
                obj.insert(name.clone(), sqlite_value_to_json(row.get(i)?));
            }
            Ok(Value::Object(obj))
        })
        .context("failed to execute user query")?;
    for row in rows {
        let row = row.context("failed to collect query rows")?;
        if result_rows.len() >= MAX_ROWS {
            anyhow::bail!(
                "query_results result exceeds this connector's row cap ({MAX_ROWS} rows); \
                 narrow the query"
            );
        }
        result_rows.push(row);
    }

    let runtime_seconds = started.elapsed().as_secs_f64();

    let columns: Vec<ColumnMeta> = col_names
        .iter()
        .map(|name| {
            let sample = result_rows.iter().find_map(|row| {
                row.as_object()
                    .and_then(|obj| obj.get(name))
                    .filter(|v| !v.is_null())
            });
            ColumnMeta {
                name: name.clone(),
                friendly_type: friendly_type_from_json_sample(sample).to_string(),
            }
        })
        .collect();

    Ok(QueryExecutionResult {
        columns,
        rows: result_rows,
        runtime_seconds,
    })
}

/// Binds a cached cell as a real typed SQLite value, not text: a `"null"` string would break `IS
/// NULL` and `COUNT(col)`, and numbers must stay numeric.
pub(crate) fn json_to_sqlite_value(v: &Value) -> rusqlite::types::Value {
    match v {
        Value::Null => rusqlite::types::Value::Null,
        Value::Bool(b) => rusqlite::types::Value::Integer(i64::from(*b)),
        Value::Number(n) => n.as_i64().map_or_else(
            || rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0)),
            rusqlite::types::Value::Integer,
        ),
        Value::String(s) => rusqlite::types::Value::Text(s.clone()),
        other => rusqlite::types::Value::Text(other.to_string()),
    }
}

fn sqlite_value_to_json(v: rusqlite::types::Value) -> Value {
    match v {
        rusqlite::types::Value::Null => Value::Null,
        rusqlite::types::Value::Integer(i) => json!(i),
        rusqlite::types::Value::Real(f) => json!(f),
        rusqlite::types::Value::Text(s) => json!(s),
        rusqlite::types::Value::Blob(b) => json!(format!("<blob {} bytes>", b.len())),
    }
}

#[async_trait]
impl QueryRunner for QueryResultsConnector {
    fn type_name(&self) -> &'static str {
        "query_results"
    }

    fn configuration_schema(&self) -> Value {
        json!({ "secret": [] })
    }

    async fn test_connection(&self, _opts: &Value) -> Result<()> {
        Ok(())
    }

    async fn get_schema(&self, opts: &Value) -> Result<Vec<TableSchema>> {
        let tables = self
            .load_for_caller(opts)
            .await?
            .into_iter()
            .map(|d| TableSchema {
                name: format!("query_{}", d.query_id),
                columns: d
                    .columns
                    .into_iter()
                    .map(|c| ColumnSchema::typed(c.name, c.friendly_type))
                    .collect(),
            })
            .collect();
        Ok(tables)
    }

    fn network_targets(&self, _opts: &Value) -> Vec<NetworkTarget> {
        vec![]
    }

    async fn run_query(&self, opts: &Value, query: &str) -> Result<QueryExecutionResult> {
        let datasets = self.load_for_caller(opts).await?;
        let query = query.to_string();
        let conn = open_sqlite_conn()?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let interrupt = InterruptOnDrop {
            handle: conn.get_interrupt_handle(),
            cancelled: Arc::clone(&cancelled),
        };
        let mut task =
            tokio::task::spawn_blocking(move || run_on_sqlite(conn, datasets, &query, &cancelled));
        let secs = super::query_timeout_secs();
        if let Ok(joined) = tokio::time::timeout(Duration::from_secs(secs), &mut task).await {
            joined.context("SQLite thread panicked")?
        } else {
            drop(interrupt);
            task.abort();
            anyhow::bail!("query_results query exceeded the {secs}s time limit and was cancelled")
        }
    }
}

/// Pins `load_result_data`'s SQL to the same verdict as the Rust rule
/// `authz::access::can_view_query_like`, comparing both against a real database.
#[cfg(test)]
mod query_results_authz_agreement_tests {
    use super::*;
    use crate::authz::access::{can_view_query_like, query_is_published};
    use crate::entities::{
        access_permission, change, data_source_group, group_role, query, role, user,
    };
    use crate::test_support::{self, test_db};
    use sea_orm::ActiveValue::{NotSet, Set};
    use sea_orm::{ActiveModelTrait, DatabaseConnection};

    fn unique(label: &str) -> String {
        format!("{label}-{}", uuid::Uuid::new_v4().simple())
    }

    async fn seed_org(db: &DatabaseConnection, slug_label: &str) -> i64 {
        let mut org = test_support::new_organization();
        org.slug = Set(unique(slug_label));
        org.insert(db).await.expect("insert org").id
    }

    async fn seed_data_source(db: &DatabaseConnection, org_id: i64) -> i64 {
        test_support::new_data_source(org_id)
            .insert(db)
            .await
            .expect("insert data source")
            .id
    }

    async fn seed_user(db: &DatabaseConnection, org_id: i64, groups: Vec<i64>) -> user::Model {
        let mut u = test_support::new_user(org_id);
        u.groups = Set(groups);
        u.api_key_hash = Set(crate::hash::hash_api_key(&unique("user-key")));
        u.email = Set(format!("{}@example.com", unique("user")));
        u.insert(db).await.expect("insert user")
    }

    /// A group holding `permissions` via a real `roles`/`group_roles` link (the
    /// `groups.permissions` column is vestigial).
    async fn seed_group_with_permissions(
        db: &DatabaseConnection,
        org_id: i64,
        permissions: Vec<String>,
        restricted: bool,
    ) -> i64 {
        let mut g = test_support::new_group(org_id, &unique("group"), vec![]);
        g.data_source_access_restricted = Set(restricted);
        let g = g.insert(db).await.expect("insert group");
        let r = role::ActiveModel {
            id: NotSet,
            org_id: Set(org_id),
            name: Set(unique("role")),
            permissions: Set(permissions),
            r#type: Set("custom".to_string()),
            created_at: NotSet,
            updated_at: NotSet,
        }
        .insert(db)
        .await
        .expect("insert role");
        group_role::ActiveModel {
            id: NotSet,
            group_id: Set(g.id),
            role_id: Set(r.id),
            created_at: NotSet,
            updated_at: NotSet,
        }
        .insert(db)
        .await
        .expect("insert group_role");
        g.id
    }

    /// `queries.published_change_id` is an FK to `changes`, so a "published" fixture needs a real
    /// row.
    async fn seed_change(db: &DatabaseConnection, org_id: i64, user_id: i64, query_id: i64) -> i64 {
        change::ActiveModel {
            id: NotSet,
            object_type: Set("query".to_string()),
            object_id: Set(query_id),
            user_id: Set(user_id),
            change: Set(json!({})),
            created_at: NotSet,
            updated_at: NotSet,
        }
        .insert(db)
        .await
        .unwrap_or_else(|_| panic!("insert change failed for org {org_id}"))
        .id
    }

    async fn seed_query(
        db: &DatabaseConnection,
        org_id: i64,
        data_source_id: i64,
        owner_id: i64,
        visibility: &str,
        published: bool,
    ) -> query::Model {
        let query_text = format!("SELECT 1 AS a -- {}", unique("q"));
        let mut q = test_support::new_query(org_id, data_source_id, owner_id);
        q.query = Set(query_text.clone());
        q.query_hash = Set(crate::hash::gen_query_hash(&query_text, &json!({})));
        q.visibility = Set(visibility.to_string());
        let q = q.insert(db).await.expect("insert query");
        if !published {
            return q;
        }
        let change_id = seed_change(db, org_id, owner_id, q.id).await;
        let mut update: query::ActiveModel = q.into();
        update = test_support::seed_published(update, Some(change_id));
        update.update(db).await.expect("publish query")
    }

    async fn seed_result(
        db: &DatabaseConnection,
        org_id: i64,
        data_source_id: i64,
        q: &query::Model,
    ) {
        use crate::entities::query_result;
        let now = chrono::Utc::now().into();
        query_result::ActiveModel {
            id: NotSet,
            org_id: Set(org_id),
            data_source_id: Set(data_source_id),
            query_hash: Set(q.query_hash.clone()),
            query: Set(q.query.clone()),
            data: Set(Some(json!({
                "columns": [{"name": "a", "friendly_type": "integer"}],
                "rows": [{"a": 1}],
            }))),
            data_location: Set(None),
            data_size_bytes: Set(None),
            runtime: Set(Some(0.01)),
            retrieved_at: Set(now),
            last_used_at: Set(now),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(db)
        .await
        .expect("insert query_result");
    }

    async fn grant(db: &DatabaseConnection, query_id: i64, grantee_id: i64, access_type: &str) {
        access_permission::ActiveModel {
            id: NotSet,
            object_type: Set("query".to_string()),
            object_id: Set(query_id),
            access_type: Set(access_type.to_string()),
            grantor_id: Set(grantee_id),
            grantee_id: Set(Some(grantee_id)),
            grantee_group_id: Set(None),
            created_at: NotSet,
            updated_at: NotSet,
        }
        .insert(db)
        .await
        .expect("insert access_permission");
    }

    async fn sql_says_visible(
        db: &DatabaseConnection,
        org_id: i64,
        caller_id: i64,
        query_id: i64,
    ) -> bool {
        load_result_data(db, org_id, caller_id)
            .await
            .expect("load_result_data failed")
            .iter()
            .any(|r| r.query_id == query_id)
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // current-thread runtime; lock is test-only
    async fn sql_and_rust_authorization_rules_agree_across_a_fixture_matrix() {
        let _max_rows_guard = crate::test_support::hold_max_query_rows_env();
        let Some(db) = test_db().await else { return };

        let org_id = seed_org(&db, "qr-authz-agreement").await;
        let ds_id = seed_data_source(&db, org_id).await;
        let ds_other = seed_data_source(&db, org_id).await;

        let unrestricted_group = seed_group_with_permissions(&db, org_id, vec![], false).await;
        let restricted_group = seed_group_with_permissions(&db, org_id, vec![], true).await;
        let admin_group =
            seed_group_with_permissions(&db, org_id, vec!["admin".to_string()], false).await;

        let owner = seed_user(&db, org_id, vec![unrestricted_group]).await;
        let bystander = seed_user(&db, org_id, vec![unrestricted_group]).await;
        let admin = seed_user(&db, org_id, vec![admin_group]).await;
        let grantee = seed_user(&db, org_id, vec![restricted_group]).await;
        let scoped_caller = seed_user(&db, org_id, vec![unrestricted_group]).await;
        let unscoped_caller = seed_user(&db, org_id, vec![restricted_group]).await;

        // A restricted group scoped to `ds_other`; dedicated so `unscoped_caller`'s negative case
        // (no scope over `ds_id`) stays intact.
        let grant_scoped_group = seed_group_with_permissions(&db, org_id, vec![], true).await;
        data_source_group::ActiveModel {
            id: NotSet,
            data_source_id: Set(ds_other),
            group_id: Set(grant_scoped_group),
            created_at: NotSet,
            updated_at: NotSet,
        }
        .insert(&db)
        .await
        .expect("insert data_source_group");
        let grant_scoped_caller = seed_user(&db, org_id, vec![grant_scoped_group]).await;

        // Each case: (label, caller, query owner, data source, visibility, published).
        struct Case<'a> {
            label: &'a str,
            caller: &'a user::Model,
            owner_id: i64,
            data_source_id: i64,
            visibility: &'a str,
            published: bool,
            grant_to: Option<i64>,
        }
        let cases = [
            Case {
                label: "owner",
                caller: &owner,
                owner_id: owner.id,
                data_source_id: ds_id,
                visibility: "restricted",
                published: false,
                grant_to: None,
            },
            Case {
                label: "org admin",
                caller: &admin,
                owner_id: bystander.id,
                data_source_id: ds_id,
                visibility: "restricted",
                published: false,
                grant_to: None,
            },
            Case {
                label: "explicit access_permissions grant",
                caller: &grantee,
                owner_id: bystander.id,
                data_source_id: ds_other,
                visibility: "restricted",
                published: false,
                grant_to: Some(grantee.id),
            },
            Case {
                label: "published workspace, caller has data-source scope",
                caller: &scoped_caller,
                owner_id: bystander.id,
                data_source_id: ds_id,
                visibility: "workspace",
                published: true,
                grant_to: None,
            },
            Case {
                label: "published workspace, caller lacks data-source scope",
                caller: &unscoped_caller,
                owner_id: bystander.id,
                data_source_id: ds_id,
                visibility: "workspace",
                published: true,
                grant_to: None,
            },
            Case {
                label: "published workspace, caller scoped via a real data_source_groups grant",
                caller: &grant_scoped_caller,
                owner_id: bystander.id,
                data_source_id: ds_other,
                visibility: "workspace",
                published: true,
                grant_to: None,
            },
            Case {
                label: "unpublished and unreached",
                caller: &bystander,
                owner_id: owner.id,
                data_source_id: ds_id,
                visibility: "restricted",
                published: false,
                grant_to: None,
            },
        ];

        for case in cases {
            let q = seed_query(
                &db,
                org_id,
                case.data_source_id,
                case.owner_id,
                case.visibility,
                case.published,
            )
            .await;
            seed_result(&db, org_id, case.data_source_id, &q).await;
            if let Some(grantee_id) = case.grant_to {
                grant(&db, q.id, grantee_id, "view").await;
            }

            let rust_says_visible = can_view_query_like(
                &db,
                case.caller,
                "query",
                q.id,
                q.user_id,
                &q.visibility,
                !query_is_published(&q),
                q.data_source_id,
            )
            .await
            .expect("can_view_query_like failed");
            let sql_visible = sql_says_visible(&db, org_id, case.caller.id, q.id).await;

            assert_eq!(
                rust_says_visible, sql_visible,
                "case {:?}: can_view_query_like said {rust_says_visible}, SQL said {sql_visible}",
                case.label
            );
        }
    }

    /// A second org's query with the same `query_hash` and a cached result must never surface in
    /// the first org (the join scopes on both orgs).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // current-thread runtime; lock is test-only
    async fn colliding_query_hash_across_orgs_is_not_a_visible_row() {
        let _max_rows_guard = crate::test_support::hold_max_query_rows_env();
        let Some(db) = test_db().await else { return };

        let org_a_id = seed_org(&db, "qr-authz-collide-a").await;
        let org_b_id = seed_org(&db, "qr-authz-collide-b").await;
        let ds_a = seed_data_source(&db, org_a_id).await;
        let ds_b = seed_data_source(&db, org_b_id).await;
        let group_a = seed_group_with_permissions(&db, org_a_id, vec![], false).await;
        let group_b = seed_group_with_permissions(&db, org_b_id, vec![], false).await;
        let user_a = seed_user(&db, org_a_id, vec![group_a]).await;
        let user_b = seed_user(&db, org_b_id, vec![group_b]).await;

        let shared_text = "SELECT 'shared' AS x";
        let shared_hash = crate::hash::gen_query_hash(shared_text, &json!({}));

        let mut q_a = test_support::new_query(org_a_id, ds_a, user_a.id);
        q_a.query = Set(shared_text.to_string());
        q_a.query_hash = Set(shared_hash.clone());
        q_a.visibility = Set("public".to_string());
        let q_a = test_support::seed_published(q_a, None)
            .insert(&db)
            .await
            .expect("insert query a");
        // Org A has no cached result of its own for this query — only org B does, below.

        let mut q_b = test_support::new_query(org_b_id, ds_b, user_b.id);
        q_b.query = Set(shared_text.to_string());
        q_b.query_hash = Set(shared_hash.clone());
        q_b.visibility = Set("public".to_string());
        let q_b = test_support::seed_published(q_b, None)
            .insert(&db)
            .await
            .expect("insert query b");
        seed_result(&db, org_b_id, ds_b, &q_b).await;

        assert!(
            !sql_says_visible(&db, org_a_id, user_a.id, q_a.id).await,
            "org A must not see a table for query_{} sourced from org B's cached result",
            q_a.id
        );
    }

    /// The join requires `qr.data_source_id` to be one of the query's own data sources; a result
    /// under an unrelated one must not join, even for the owner.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // current-thread runtime; lock is test-only
    async fn a_result_under_an_unrelated_data_source_is_not_a_visible_row() {
        let _max_rows_guard = crate::test_support::hold_max_query_rows_env();
        let Some(db) = test_db().await else { return };

        let org_id = seed_org(&db, "qr-authz-mismatched-ds").await;
        let ds = seed_data_source(&db, org_id).await;
        let ds_unrelated = seed_data_source(&db, org_id).await;
        let group = seed_group_with_permissions(&db, org_id, vec![], false).await;
        let owner = seed_user(&db, org_id, vec![group]).await;

        let q = seed_query(&db, org_id, ds, owner.id, "restricted", false).await;
        // `ds_unrelated` is neither the query's data source nor a fallback; the join's `IN (...)`
        // must exclude it.
        seed_result(&db, org_id, ds_unrelated, &q).await;

        assert!(
            !sql_says_visible(&db, org_id, owner.id, q.id).await,
            "a query_results row from an unrelated data source must not join to the query at all"
        );
    }
}

#[cfg(test)]
mod run_on_sqlite_tests {
    use super::*;

    fn dataset(query_id: i64, columns: &[(&str, &str)], rows: Vec<Value>) -> ResultData {
        ResultData {
            query_id,
            columns: columns
                .iter()
                .map(|(n, t)| ColumnMeta {
                    name: n.to_string(),
                    friendly_type: t.to_string(),
                })
                .collect(),
            rows,
        }
    }

    /// Runs `query` over `datasets` on a fresh connection, uncancelled.
    fn run(datasets: Vec<ResultData>, query: &str) -> Result<QueryExecutionResult> {
        run_on_sqlite(
            open_sqlite_conn().unwrap(),
            datasets,
            query,
            &AtomicBool::new(false),
        )
    }

    #[test]
    fn trailing_semicolon_and_comment_still_run() {
        for q in [
            "SELECT 1 AS n;",
            "SELECT 1 AS n; -- done",
            "SELECT 1 AS n /* x */ ;",
        ] {
            let result = run(vec![], q).unwrap_or_else(|e| panic!("{q}: {e:#}"));
            assert_eq!(result.rows[0]["n"], json!(1));
        }
    }

    #[test]
    fn a_second_statement_is_a_clean_error() {
        let err = run(vec![], "SELECT 1; SELECT 2").unwrap_err();
        assert!(
            format!("{err:#}").contains("Only one SQL statement is allowed"),
            "{err:#}"
        );
    }

    #[test]
    fn null_binds_as_real_sqlite_null_so_is_null_matches() {
        let data = dataset(1, &[("c", "string")], vec![json!({"c": Value::Null})]);
        let result = run(
            vec![data],
            "SELECT COUNT(*) AS n FROM query_1 WHERE c IS NULL",
        )
        .expect("query failed");
        assert_eq!(result.rows[0]["n"], json!(1));
    }

    #[test]
    fn numbers_bind_as_numbers_so_numeric_comparison_works() {
        let data = dataset(
            1,
            &[("n", "integer")],
            vec![json!({"n": 10}), json!({"n": 3})],
        );
        let result =
            run(vec![data], "SELECT COUNT(*) AS c FROM query_1 WHERE n > 5").expect("query failed");
        assert_eq!(result.rows[0]["c"], json!(1));
    }

    #[test]
    fn count_col_excludes_real_nulls_the_way_sql_null_semantics_require() {
        let data = dataset(
            1,
            &[("c", "string")],
            vec![json!({"c": "x"}), json!({"c": Value::Null})],
        );
        let result = run(vec![data], "SELECT COUNT(c) AS n FROM query_1").expect("query failed");
        assert_eq!(
            result.rows[0]["n"],
            json!(1),
            "COUNT(col) must not count NULLs"
        );
    }

    #[test]
    fn a_genuine_string_null_is_distinguishable_from_real_null() {
        let data = dataset(
            1,
            &[("c", "string")],
            vec![json!({"c": "null"}), json!({"c": Value::Null})],
        );
        let result = run(
            vec![data],
            "SELECT COUNT(*) AS n FROM query_1 WHERE c = 'null'",
        )
        .expect("query failed");
        assert_eq!(result.rows[0]["n"], json!(1));
    }

    /// A column whose first row is NULL but a later row is a number must still infer `"integer"`.
    #[test]
    fn column_type_uses_first_non_null_value_not_row_zero() {
        let data = dataset(
            1,
            &[("n", "string")],
            vec![json!({"n": Value::Null}), json!({"n": 5})],
        );
        let result = run(vec![data], "SELECT n FROM query_1 ORDER BY rowid").expect("query failed");
        assert_eq!(
            result
                .columns
                .iter()
                .find(|c| c.name == "n")
                .unwrap()
                .friendly_type,
            "integer"
        );
    }

    #[test]
    fn normal_create_and_select_path_still_works() {
        let data = dataset(
            1,
            &[("name", "string"), ("n", "integer")],
            vec![json!({"name": "a", "n": 1}), json!({"name": "b", "n": 2})],
        );
        let result = run(vec![data], "SELECT * FROM query_1 ORDER BY n").expect("query failed");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0]["name"], json!("a"));
    }

    /// `MAX_ROWS + 1` rows must error naming the cap; exactly `MAX_ROWS` must succeed.
    #[test]
    fn row_cap_errors_one_row_past_the_limit_and_succeeds_exactly_at_it() {
        let over_cap = format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < {}) \
             SELECT x FROM c",
            MAX_ROWS + 1
        );
        let err = run(vec![], &over_cap).expect_err("one row past the cap must error");
        assert!(
            err.to_string().contains(&MAX_ROWS.to_string()),
            "error must name the cap, got: {err}"
        );

        let at_cap = format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < {}) \
             SELECT x FROM c",
            MAX_ROWS
        );
        let result = run(vec![], &at_cap).expect("exactly at the cap must succeed");
        assert_eq!(result.rows.len(), MAX_ROWS);
    }

    /// The interrupt handle must actually stop the blocking thread (asserted on its join result,
    /// not just the outer timeout).
    #[tokio::test]
    async fn interrupt_handle_stops_a_runaway_query_on_the_blocking_thread() {
        let conn = open_sqlite_conn().unwrap();
        let handle = conn.get_interrupt_handle();
        let cancelled = AtomicBool::new(false);
        let task = tokio::task::spawn_blocking(move || {
            run_on_sqlite(
                conn,
                vec![],
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT count(*) FROM c",
                &cancelled,
            )
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        handle.interrupt();
        let joined = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the blocking thread must stop once interrupted");
        assert!(joined.unwrap().is_err());
    }

    /// Exercises `InterruptOnDrop::drop` itself, which the test above bypasses by calling
    /// `handle.interrupt()` directly.
    #[tokio::test]
    async fn dropping_the_interrupt_guard_stops_a_runaway_query() {
        let conn = open_sqlite_conn().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let guard = InterruptOnDrop {
            handle: conn.get_interrupt_handle(),
            cancelled: Arc::clone(&cancelled),
        };
        let task = tokio::task::spawn_blocking(move || {
            run_on_sqlite(
                conn,
                vec![],
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT count(*) FROM c",
                &cancelled,
            )
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(guard);
        let joined = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the blocking thread must stop once the interrupt guard is dropped");
        assert!(joined.unwrap().is_err());
    }

    /// Confirms the defensive-mode call took effect rather than silently no-opping. Deliberately
    /// does not assert that ATTACH of a `file:` URI fails: without `SQLITE_OPEN_URI` SQLite treats
    /// it as a literal filename, which can succeed.
    #[test]
    fn defensive_mode_is_actually_enabled_on_the_connection() {
        let conn = open_sqlite_conn().unwrap();
        assert!(conn
            .db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE)
            .unwrap());
    }

    fn wide_dataset(query_id: i64, rows: usize) -> ResultData {
        dataset(
            query_id,
            &[("a", "integer"), ("b", "string")],
            (0..rows)
                .map(|i| json!({"a": i as i64, "b": format!("row-{i}")}))
                .collect(),
        )
    }

    /// A dataset whose `CREATE TABLE` is a syntax error, so reaching it fails with something other
    /// than the cancellation message.
    fn unloadable_dataset(query_id: i64) -> ResultData {
        dataset(query_id, &[], vec![])
    }

    fn assert_cancelled(err: &anyhow::Error) {
        assert!(
            err.to_string().contains("cancelled"),
            "error must name cancellation, got: {err:#}"
        );
    }

    /// A pre-set flag must bail before the first `CREATE TABLE` (no timing involved).
    #[test]
    fn a_pre_set_cancel_flag_bails_before_any_table_is_created() {
        let err = run_on_sqlite(
            open_sqlite_conn().unwrap(),
            vec![unloadable_dataset(1)],
            "SELECT 1",
            &AtomicBool::new(true),
        )
        .expect_err("a pre-set cancel flag must bail before doing any work");
        assert_cancelled(&err);
    }

    /// Zero-row datasets skip the per-row checkpoint, so only the per-dataset loop check catches a
    /// pre-set flag.
    #[test]
    fn a_pre_set_cancel_flag_with_only_empty_datasets_is_caught_by_the_dataset_loop_checkpoint() {
        let datasets = vec![wide_dataset(1, 0), unloadable_dataset(2)];
        let err = run_on_sqlite(
            open_sqlite_conn().unwrap(),
            datasets,
            "SELECT 1",
            &AtomicBool::new(true),
        )
        .expect_err("a pre-set cancel flag must bail before creating any of the empty tables");
        assert_cancelled(&err);
    }

    /// With a single-thread blocking pool a second query only starts once the first closure
    /// returned; the first returning a cancellation error proves it stopped early.
    #[test]
    fn a_cancelled_run_frees_its_blocking_thread_for_the_next_query() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("build single-blocking-thread runtime");

        rt.block_on(async {
            let conn = open_sqlite_conn().unwrap();
            let cancelled = Arc::new(AtomicBool::new(false));
            let interrupt = InterruptOnDrop {
                handle: conn.get_interrupt_handle(),
                cancelled: Arc::clone(&cancelled),
            };
            let datasets = vec![wide_dataset(1, 1_000_000)];
            let started = Arc::new(AtomicBool::new(false));
            let started_in = Arc::clone(&started);
            let first = tokio::task::spawn_blocking(move || {
                started_in.store(true, Ordering::SeqCst);
                run_on_sqlite(conn, datasets, "SELECT 1", &cancelled)
            });
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            drop(interrupt);

            let second = tokio::time::timeout(
                Duration::from_secs(60),
                tokio::task::spawn_blocking(|| run(vec![wide_dataset(9_999, 1)], "SELECT 1")),
            )
            .await
            .expect("second query starved for the blocking thread the first one should have freed")
            .expect("second query task panicked")
            .expect("second query itself failed");
            assert_eq!(second.rows.len(), 1);

            let first_err = first
                .await
                .expect("first query task panicked")
                .expect_err("the first run must have been cut short, not loaded every row");
            assert_cancelled(&first_err);
        });
    }

    /// Dropping the `InterruptOnDrop` guard mid-population must stop the row-insert loop (join the
    /// handle; expect `Err` before all 1,000,000 rows).
    #[tokio::test]
    async fn cancelling_mid_population_stops_the_insert_loop_far_short_of_the_full_dataset() {
        let conn = open_sqlite_conn().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let guard = InterruptOnDrop {
            handle: conn.get_interrupt_handle(),
            cancelled: Arc::clone(&cancelled),
        };
        let datasets = vec![wide_dataset(1, 1_000_000)];
        let started = Arc::new(AtomicBool::new(false));
        let started_in = Arc::clone(&started);
        conn.update_hook(Some(
            move |_: rusqlite::hooks::Action, _: &str, _: &str, _: i64| {
                started_in.store(true, Ordering::SeqCst);
            },
        ))
        .unwrap();
        let task = tokio::task::spawn_blocking(move || {
            run_on_sqlite(conn, datasets, "SELECT 1", &cancelled)
        });
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        drop(guard);

        let joined = tokio::time::timeout(Duration::from_secs(60), task)
            .await
            .expect("the blocking thread must finish (not hang) once the guard is dropped")
            .expect("blocking thread panicked");

        let err = joined.expect_err("a cancelled population must return an error");
        assert_cancelled(&err);
    }

    /// With no datasets, a pre-set flag is only observable at the checkpoint before the user's
    /// query is prepared.
    #[test]
    fn a_pre_set_flag_with_no_datasets_is_only_caught_by_the_pre_query_checkpoint() {
        let conn = open_sqlite_conn().unwrap();
        run_on_sqlite(conn, vec![], "SELECT 1", &AtomicBool::new(true))
            .expect_err("a pre-set flag with no datasets must still bail before the user query");
    }
}

#[cfg(test)]
mod inject_caller_tests {
    use super::inject_caller;
    use serde_json::json;

    #[test]
    fn query_results_gets_both_org_id_and_user_id() {
        let out = inject_caller(json!({}), "query_results", 7, 9);
        assert_eq!(out["org_id"], 7);
        assert_eq!(out["user_id"], 9);
    }

    #[test]
    fn csv_gets_org_id_only() {
        let out = inject_caller(json!({"upload_id": "abc"}), "csv", 7, 9);
        assert_eq!(out["org_id"], 7);
        assert!(out.get("user_id").is_none(), "csv needs no user identity");
        assert_eq!(out["upload_id"], "abc");
    }

    #[test]
    fn sqlite_gets_org_id_only() {
        let out = inject_caller(json!({"upload_id": "abc"}), "sqlite", 7, 9);
        assert_eq!(out["org_id"], 7);
        assert!(
            out.get("user_id").is_none(),
            "sqlite needs no user identity"
        );
    }

    #[test]
    fn other_connector_types_are_left_untouched() {
        let original = json!({"host": "db.internal", "password": "hunter2"});
        let out = inject_caller(original.clone(), "pg", 7, 9);
        assert_eq!(out, original);
    }
}

#[cfg(test)]
mod query_results_id_scoping_tests {
    use crate::connectors::injected_id;
    use serde_json::json;

    #[test]
    fn org_id_fails_closed_when_caller_forgot_to_inject_it() {
        let err = injected_id(&json!({}), "org_id", "query_results").unwrap_err();
        assert!(
            err.to_string().contains("caller bug"),
            "must name the caller-bug contract, got: {err}"
        );
    }

    #[test]
    fn org_id_reads_the_injected_value() {
        assert_eq!(
            injected_id(&json!({"org_id": 7}), "org_id", "query_results").unwrap(),
            7
        );
    }

    #[test]
    fn user_id_fails_closed_when_caller_forgot_to_inject_it() {
        let err = injected_id(&json!({}), "user_id", "query_results").unwrap_err();
        assert!(
            err.to_string().contains("caller bug"),
            "must name the caller-bug contract, got: {err}"
        );
    }

    #[test]
    fn user_id_reads_the_injected_value() {
        assert_eq!(
            injected_id(&json!({"user_id": 9}), "user_id", "query_results").unwrap(),
            9
        );
    }
}
