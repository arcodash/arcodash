//! Atomic writes to `organizations.settings` (jsonb). Every writer must go through here: a
//! read-modify-write off a caller-held snapshot drops a concurrent writer's changes to other keys.

use chrono::Utc;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, UpdateMany};
use serde_json::Value;

use crate::entities::organization;

/// `UPDATE organizations SET settings = <settings>, updated_at = now() WHERE id = org_id`.
fn update_settings(org_id: i64, settings: Expr) -> UpdateMany<organization::Entity> {
    organization::Entity::update_many()
        .col_expr(organization::Column::Settings, settings)
        .col_expr(organization::Column::UpdatedAt, Expr::value(Utc::now()))
        .filter(organization::Column::Id.eq(org_id))
}

fn ensure_object(patch: &Value) -> Result<(), DbErr> {
    if patch.is_object() {
        return Ok(());
    }
    Err(DbErr::Custom(format!(
        "organizations.settings patch must be a JSON object, got {patch}"
    )))
}

fn not_found(org_id: i64) -> DbErr {
    DbErr::RecordNotFound(format!("organization {org_id} not found"))
}

/// Runs `query`, mapping "no row updated" to `RecordNotFound`.
async fn exec_existing<C: ConnectionTrait>(
    db: &C,
    org_id: i64,
    query: UpdateMany<organization::Entity>,
) -> Result<(), DbErr> {
    if query.exec(db).await?.rows_affected == 0 {
        return Err(not_found(org_id));
    }
    Ok(())
}

/// Runs `query` with `RETURNING`, mapping "no row updated" to `RecordNotFound`.
async fn exec_returning<C: ConnectionTrait>(
    db: &C,
    org_id: i64,
    query: UpdateMany<organization::Entity>,
) -> Result<organization::Model, DbErr> {
    let rows = query.exec_with_returning(db).await?;
    rows.into_iter().next().ok_or_else(|| not_found(org_id))
}

fn merge_query(org_id: i64, patch: Value) -> Result<UpdateMany<organization::Entity>, DbErr> {
    ensure_object(&patch)?;
    Ok(update_settings(
        org_id,
        Expr::cust_with_values(
            "CASE WHEN jsonb_typeof(settings) = 'object' THEN settings ELSE '{}'::jsonb END \
             || $1::jsonb",
            [patch],
        ),
    ))
}

/// Merge `patch`'s top-level keys into `organizations.settings` in one statement against the live
/// row; absent keys are untouched.
pub async fn merge_org_settings<C: ConnectionTrait>(
    db: &C,
    org_id: i64,
    patch: Value,
) -> Result<(), DbErr> {
    exec_existing(db, org_id, merge_query(org_id, patch)?).await
}

#[allow(
    clippy::similar_names,
    reason = "`path` and `patch` are the domain terms for where and what to merge"
)]
fn merge_query_at(
    org_id: i64,
    path: &[&str],
    patch: Value,
) -> Result<UpdateMany<organization::Entity>, DbErr> {
    if path.is_empty() {
        return Err(DbErr::Custom(
            "organizations.settings merge path must not be empty".to_string(),
        ));
    }
    ensure_object(&patch)?;
    // `jsonb_set(..., create_missing = true)` only creates the FINAL path element and silently
    // no-ops if an earlier segment is missing. Built bottom-up instead: merge `patch` at the full
    // path, then re-wrap one level up via `COALESCE(..., '{}') || jsonb_build_object(key, inner)`.
    // `path` elements reach the query only as array data ($1) and Rust-computed integer literals,
    // never as SQL text, so a caller-supplied segment can't inject.
    let n = path.len();
    let mut expr = "COALESCE(settings #> $1::text[], '{}'::jsonb) || $2::jsonb".to_string();
    for i in (1..n).rev() {
        expr = format!(
            "COALESCE(settings #> ($1::text[])[1:{i}], '{{}}'::jsonb) || \
             jsonb_build_object(($1::text[])[{}], {expr})",
            i + 1
        );
    }
    let sql = format!(
        "CASE WHEN jsonb_typeof(settings) = 'object' THEN settings ELSE '{{}}'::jsonb END || \
         jsonb_build_object(($1::text[])[1], {expr})"
    );
    let path: Vec<String> = path.iter().map(std::string::ToString::to_string).collect();
    Ok(update_settings(
        org_id,
        Expr::cust_with_values(
            sql,
            [
                sea_orm::sea_query::Value::from(path),
                sea_orm::sea_query::Value::from(patch),
            ],
        ),
    ))
}

/// Merge `patch` into the nested object at `path` (e.g. `["ai"]`), leaving sibling keys untouched;
/// unlike [`merge_org_settings`]'s shallow `||`, which replaces the whole nested object.
#[allow(
    clippy::similar_names,
    reason = "`path` and `patch` are the domain terms for where and what to merge"
)]
pub async fn merge_org_settings_at<C: ConnectionTrait>(
    db: &C,
    org_id: i64,
    path: &[&str],
    patch: Value,
) -> Result<(), DbErr> {
    exec_existing(db, org_id, merge_query_at(org_id, path, patch)?).await
}

/// [`merge_org_settings_at`], returning the post-write row via `UPDATE ... RETURNING`.
#[allow(
    clippy::similar_names,
    reason = "`path` and `patch` are the domain terms for where and what to merge"
)]
pub async fn merge_org_settings_at_returning<C: ConnectionTrait>(
    db: &C,
    org_id: i64,
    path: &[&str],
    patch: Value,
) -> Result<organization::Model, DbErr> {
    exec_returning(db, org_id, merge_query_at(org_id, path, patch)?).await
}

/// Like [`merge_org_settings`] but only writes if `settings->key` still equals `expected`
/// (compare-and-swap). An absent key never matches. Returns `Ok(false)` (not `RecordNotFound`) on a
/// lost race or vanished row.
pub async fn merge_org_settings_if_key_matches<C: ConnectionTrait>(
    db: &C,
    org_id: i64,
    key: &str,
    expected: &Value,
    patch: Value,
) -> Result<bool, DbErr> {
    let result = merge_query(org_id, patch)?
        .filter(Expr::cust_with_values(
            "settings->$1::text = $2::jsonb",
            [
                sea_orm::Value::from(key),
                sea_orm::Value::from(expected.clone()),
            ],
        ))
        .exec(db)
        .await?;
    Ok(result.rows_affected == 1)
}

/// Like [`merge_org_settings`], returning the post-write row via `UPDATE ... RETURNING`.
pub async fn merge_org_settings_returning<C: ConnectionTrait>(
    db: &C,
    org_id: i64,
    patch: Value,
) -> Result<organization::Model, DbErr> {
    exec_returning(db, org_id, merge_query(org_id, patch)?).await
}

fn remove_key_query(org_id: i64, key: &str) -> UpdateMany<organization::Entity> {
    update_settings(
        org_id,
        Expr::cust_with_values("settings - $1::text", [key.to_string()]),
    )
}

/// Remove one top-level key; `||` can't delete, so this uses jsonb `-`.
pub async fn remove_org_setting_key<C: ConnectionTrait>(
    db: &C,
    org_id: i64,
    key: &str,
) -> Result<(), DbErr> {
    exec_existing(db, org_id, remove_key_query(org_id, key)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_db;
    use sea_orm::{DbBackend, QueryTrait};

    /// Exact SQL shape: a single `UPDATE ... SET settings = settings || $1`, never a `SELECT`
    /// first. Built, not executed.
    #[test]
    fn merge_org_settings_uses_jsonb_concat_not_a_read_then_write() {
        let sql = merge_query(
            7,
            serde_json::json!({ "retention": { "status": "enabled" } }),
        )
        .unwrap()
        .build(DbBackend::Postgres)
        .to_string()
        .to_lowercase();
        assert!(sql.starts_with("update"), "sql: {sql}");
        assert!(sql.contains("jsonb_typeof(settings)"), "sql: {sql}");
        assert!(sql.contains("|| "), "sql: {sql}");
        assert!(!sql.contains("select"), "must not read-then-write: {sql}");
    }

    #[test]
    fn merge_query_rejects_a_non_object_patch() {
        assert!(merge_query(7, serde_json::json!([])).is_err());
        assert!(merge_query(7, serde_json::json!("x")).is_err());
        assert!(merge_query(7, serde_json::json!(null)).is_err());
    }

    #[test]
    fn merge_query_at_uses_jsonb_build_object_not_a_read_then_write() {
        let sql = merge_query_at(7, &["ai"], serde_json::json!({ "b": 3 }))
            .unwrap()
            .build(DbBackend::Postgres)
            .to_string()
            .to_lowercase();
        assert!(sql.starts_with("update"), "sql: {sql}");
        assert!(sql.contains("jsonb_build_object("), "sql: {sql}");
        assert!(!sql.contains("select"), "must not read-then-write: {sql}");
    }

    #[test]
    fn merge_query_at_rejects_an_empty_path() {
        assert!(merge_query_at(7, &[], serde_json::json!({ "a": 1 })).is_err());
    }

    #[test]
    fn merge_query_at_rejects_a_non_object_patch() {
        assert!(merge_query_at(7, &["ai"], serde_json::json!([])).is_err());
        assert!(merge_query_at(7, &["ai"], serde_json::json!("x")).is_err());
        assert!(merge_query_at(7, &["ai"], serde_json::json!(null)).is_err());
    }

    #[test]
    fn remove_org_setting_key_uses_jsonb_delete_key_operator() {
        let sql = remove_key_query(7, "disabled_at")
            .build(DbBackend::Postgres)
            .to_string()
            .to_lowercase();
        assert!(sql.starts_with("update"), "sql: {sql}");
        assert!(sql.contains("settings - "), "sql: {sql}");
    }

    async fn org_row(db: &sea_orm::DatabaseConnection, org_id: i64) -> organization::Model {
        organization::Entity::find_by_id(org_id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    async fn seed_org(db: &sea_orm::DatabaseConnection, settings: Value) -> i64 {
        use sea_orm::ActiveModelTrait;
        use sea_orm::ActiveValue::Set;
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(Utc::now());
        let suffix = uuid::Uuid::new_v4();
        organization::ActiveModel {
            name: Set(format!("org-settings-test-{suffix}")),
            slug: Set(format!("org-settings-test-{suffix}")),
            settings: Set(settings),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
        .id
    }

    /// Deep-merge semantics a shallow `||` can't give: merging at a nested path leaves siblings
    /// untouched.
    #[tokio::test]
    async fn merge_org_settings_at_merges_the_nested_object_without_touching_siblings() {
        let Some(db) = test_db().await else { return };
        let org_id = seed_org(
            &db,
            serde_json::json!({ "ai": { "a": 1, "b": 2 }, "retention": { "x": 1 } }),
        )
        .await;

        merge_org_settings_at(&db, org_id, &["ai"], serde_json::json!({ "b": 3 }))
            .await
            .unwrap();

        let row = org_row(&db, org_id).await;
        assert_eq!(row.settings["ai"]["a"], serde_json::json!(1));
        assert_eq!(row.settings["ai"]["b"], serde_json::json!(3));
        assert_eq!(row.settings["retention"], serde_json::json!({ "x": 1 }));
    }

    /// `create_missing` only creates the final key; a two-level path with a missing first segment
    /// must still be created.
    #[tokio::test]
    async fn merge_org_settings_at_creates_a_missing_intermediate_container() {
        let Some(db) = test_db().await else { return };
        let org_id = seed_org(&db, serde_json::json!({ "retention": { "x": 1 } })).await;

        merge_org_settings_at(
            &db,
            org_id,
            &["ai", "openai"],
            serde_json::json!({ "model": "x" }),
        )
        .await
        .unwrap();

        let row = org_row(&db, org_id).await;
        assert_eq!(
            row.settings["ai"]["openai"]["model"],
            serde_json::json!("x")
        );
        assert_eq!(row.settings["retention"], serde_json::json!({ "x": 1 }));

        // A second provider merged alongside must not clobber the first.
        merge_org_settings_at(
            &db,
            org_id,
            &["ai", "gemini"],
            serde_json::json!({ "model": "y" }),
        )
        .await
        .unwrap();
        let row = org_row(&db, org_id).await;
        assert_eq!(
            row.settings["ai"]["openai"]["model"],
            serde_json::json!("x")
        );
        assert_eq!(
            row.settings["ai"]["gemini"]["model"],
            serde_json::json!("y")
        );
    }

    /// `merge_org_settings`, `merge_org_settings_at` and `remove_org_setting_key` must surface (via
    /// a `rows_affected` guard) a write against a deleted org.
    #[tokio::test]
    async fn merge_and_remove_report_not_found_for_a_nonexistent_org() {
        let Some(db) = test_db().await else { return };
        let missing_org_id = -1;

        assert!(matches!(
            merge_org_settings(&db, missing_org_id, serde_json::json!({ "a": 1 })).await,
            Err(DbErr::RecordNotFound(_))
        ));
        assert!(matches!(
            merge_org_settings_at(&db, missing_org_id, &["ai"], serde_json::json!({ "a": 1 }))
                .await,
            Err(DbErr::RecordNotFound(_))
        ));
        assert!(matches!(
            remove_org_setting_key(&db, missing_org_id, "some_key").await,
            Err(DbErr::RecordNotFound(_))
        ));
    }

    /// A real org still merges and removes successfully.
    #[tokio::test]
    async fn merge_and_remove_succeed_for_a_real_org() {
        let Some(db) = test_db().await else { return };
        let org_id = seed_org(&db, serde_json::json!({ "keep_me": true })).await;

        merge_org_settings(&db, org_id, serde_json::json!({ "a": 1 }))
            .await
            .unwrap();
        remove_org_setting_key(&db, org_id, "a").await.unwrap();

        let row = org_row(&db, org_id).await;
        assert_eq!(row.settings["keep_me"], serde_json::json!(true));
        assert!(row.settings.get("a").is_none());
    }

    /// A matching `expected` snapshot updates and reports success.
    #[tokio::test]
    async fn merge_org_settings_if_key_matches_writes_on_a_matching_expectation() {
        let Some(db) = test_db().await else { return };
        let current = serde_json::json!({ "status": "enabled", "version": "v1" });
        let org_id = seed_org(&db, serde_json::json!({ "retention": current })).await;

        let applied = merge_org_settings_if_key_matches(
            &db,
            org_id,
            "retention",
            &current,
            serde_json::json!({ "retention": { "status": "disabled", "version": "v1" } }),
        )
        .await
        .unwrap();
        assert!(applied);

        let row = org_row(&db, org_id).await;
        assert_eq!(
            row.settings["retention"]["status"],
            serde_json::json!("disabled")
        );
    }

    /// A stale `expected` snapshot reports failure and leaves the row byte-identical.
    #[tokio::test]
    async fn merge_org_settings_if_key_matches_leaves_the_row_untouched_on_a_stale_expectation() {
        let Some(db) = test_db().await else { return };
        let current = serde_json::json!({ "status": "enabled", "version": "v1" });
        let org_id = seed_org(&db, serde_json::json!({ "retention": current })).await;
        let stale_expectation = serde_json::json!({ "status": "on_hold", "version": "v1" });

        let applied = merge_org_settings_if_key_matches(
            &db,
            org_id,
            "retention",
            &stale_expectation,
            serde_json::json!({ "retention": { "status": "disabled", "version": "v1" } }),
        )
        .await
        .unwrap();
        assert!(!applied);

        let row = org_row(&db, org_id).await;
        assert_eq!(
            row.settings["retention"], current,
            "a lost CAS must leave the row byte-identical, not partially written"
        );
    }

    /// An org with no `retention` record always loses the race rather than satisfying an
    /// expectation of `null`.
    #[tokio::test]
    async fn merge_org_settings_if_key_matches_fails_closed_when_no_record_exists() {
        let Some(db) = test_db().await else { return };
        let org_id = seed_org(&db, serde_json::json!({})).await;

        let applied = merge_org_settings_if_key_matches(
            &db,
            org_id,
            "retention",
            &serde_json::Value::Null,
            serde_json::json!({ "retention": { "status": "enabled" } }),
        )
        .await
        .unwrap();
        assert!(!applied);
    }
}
