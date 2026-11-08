//! Atomic writes to `users.details` (jsonb).
//!
//! Mirrors `org_settings.rs`'s pattern for `organizations.settings`: every writer of this column
//! must go through here rather than cloning the row's `details`, mutating the clone, and writing
//! the whole column back — a read-modify-write off a caller-held snapshot silently drops whatever a
//! concurrent writer (an invitation resend racing a password reset, say) wrote to a different key
//! in between the read and the write.

use chrono::Utc;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, UpdateMany};
use serde_json::Value;

use crate::entities::user;

fn require_row(rows_affected: u64, user_id: i64) -> Result<(), DbErr> {
    if rows_affected == 0 {
        return Err(DbErr::RecordNotFound(format!("user {user_id} not found")));
    }
    Ok(())
}

fn merge_query(user_id: i64, patch: Value) -> Result<UpdateMany<user::Entity>, DbErr> {
    if !patch.is_object() {
        return Err(DbErr::Custom(format!(
            "users.details patch must be a JSON object, got {patch}"
        )));
    }
    Ok(user::Entity::update_many()
        .col_expr(
            user::Column::Details,
            Expr::cust_with_values(
                "CASE WHEN jsonb_typeof(details) = 'object' THEN details ELSE '{}'::jsonb END \
                 || $1::jsonb",
                [patch],
            ),
        )
        .col_expr(user::Column::UpdatedAt, Expr::value(Utc::now()))
        .filter(user::Column::Id.eq(user_id)))
}

/// Merge `patch`'s top-level keys into `users.details` in one statement, against the live row
/// rather than a caller-held snapshot.
///
/// Keys absent from `patch` are left untouched — this never replaces the column wholesale.
pub async fn merge_user_details<C: ConnectionTrait>(
    db: &C,
    user_id: i64,
    patch: Value,
) -> Result<(), DbErr> {
    let result = merge_query(user_id, patch)?.exec(db).await?;
    require_row(result.rows_affected, user_id)
}

fn remove_keys_query(user_id: i64, keys: &[&str]) -> UpdateMany<user::Entity> {
    let keys: Vec<String> = keys.iter().map(std::string::ToString::to_string).collect();
    user::Entity::update_many()
        .col_expr(
            user::Column::Details,
            Expr::cust_with_values("details - $1::text[]", [keys]),
        )
        .col_expr(user::Column::UpdatedAt, Expr::value(Utc::now()))
        .filter(user::Column::Id.eq(user_id))
}

/// Remove one or more top-level keys from `users.details` — the `||` merge above can only add or
/// overwrite keys, never delete one, so a clear needs Postgres's `-` (delete-keys) jsonb operator.
///
/// A no-op (no statement issued) for an empty key list.
pub async fn remove_user_detail_keys<C: ConnectionTrait>(
    db: &C,
    user_id: i64,
    keys: &[&str],
) -> Result<(), DbErr> {
    if keys.is_empty() {
        return Ok(());
    }
    let result = remove_keys_query(user_id, keys).exec(db).await?;
    require_row(result.rows_affected, user_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_db;
    use sea_orm::{DbBackend, QueryTrait};

    /// Confirms the exact SQL shape sent to Postgres — a single `UPDATE ... SET details =
    /// details || $1` statement, never a `SELECT` first. Built (not executed) so this needs no
    /// live connection.
    #[test]
    fn merge_user_details_uses_jsonb_concat_not_a_read_then_write() {
        let sql = merge_query(7, serde_json::json!({ "reset_token": "abc" }))
            .unwrap()
            .build(DbBackend::Postgres)
            .to_string()
            .to_lowercase();
        assert!(sql.starts_with("update"), "sql: {sql}");
        assert!(sql.contains("jsonb_typeof(details)"), "sql: {sql}");
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
    fn remove_user_detail_keys_uses_jsonb_delete_keys_operator() {
        let sql = remove_keys_query(7, &["reset_token", "reset_token_expires_at"])
            .build(DbBackend::Postgres)
            .to_string()
            .to_lowercase();
        assert!(sql.starts_with("update"), "sql: {sql}");
        assert!(sql.contains("details - "), "sql: {sql}");
    }

    async fn seed_user(db: &sea_orm::DatabaseConnection) -> i64 {
        use crate::test_support::{new_organization, new_user};
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};
        let suffix = uuid::Uuid::new_v4();
        let mut org = new_organization();
        org.name = Set(format!("user-details-test-{suffix}"));
        org.slug = Set(format!("user-details-test-{suffix}"));
        let org_id = org.insert(db).await.unwrap().id;
        let mut user = new_user(org_id);
        user.email = Set(format!("user-details-test-{suffix}@test.local"));
        user.api_key_hash = Set(crate::hash::hash_api_key(&suffix.to_string()));
        user.insert(db).await.unwrap().id
    }

    #[tokio::test]
    async fn merge_user_details_errors_when_the_row_is_gone() {
        let Some(db) = test_db().await else { return };
        let err = merge_user_details(&db, -1, serde_json::json!({ "reset_token": "abc" }))
            .await
            .unwrap_err();
        assert!(matches!(err, DbErr::RecordNotFound(_)), "err: {err:?}");
    }

    #[tokio::test]
    async fn remove_user_detail_keys_errors_when_the_row_is_gone() {
        let Some(db) = test_db().await else { return };
        let err = remove_user_detail_keys(&db, -1, &["reset_token"])
            .await
            .unwrap_err();
        assert!(matches!(err, DbErr::RecordNotFound(_)), "err: {err:?}");
    }

    /// The `-` jsonb operator matches the row, not the key: clearing a key that was never set is a
    /// legitimate no-op against a real row and must not become an error under the not-found guard.
    #[tokio::test]
    async fn remove_user_detail_keys_succeeds_when_the_key_was_never_present() {
        let Some(db) = test_db().await else { return };
        let user_id = seed_user(&db).await;

        remove_user_detail_keys(&db, user_id, &["reset_token"])
            .await
            .unwrap();
    }
}
