//! Query version history (`changes` table) — recording + ordered listing, used by
//! `query_service::publish` and the `GET /api/queries/<id>/version` handler.
//!
//! A row is appended only at a publish point: create/update/fork/revert never call [`record`], so
//! history is the sequence of published versions, not every draft keystroke.
//!
//! ## Snapshot strategy
//!
//! Each change row stores a **full snapshot** of every editable field
//! (`name`/`description`/`query`/`data_source_id`/`options`/`is_archived`) rather than a diff
//! against the previous state. That is simpler to reason about (reverting is just "copy this
//! row's `change` JSON onto the live row", no diff-replay), at the cost of more storage per row,
//! and the column is `jsonb` either way so the row shape on disk doesn't depend on the strategy.
//!
//! `changes` rows are immutable once written: reverting writes a **new** row rather than
//! mutating or deleting any existing one, so history only ever grows (an append-only audit log).

use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, Order, QueryFilter, QueryOrder,
};
use serde::Serialize;
use serde_json::Value;

use arcodash_core::entities::change;

use crate::error::ApiError;

/// Polymorphic `object_type` tag used for every query-related `changes` row.
const QUERY_OBJECT_TYPE: &str = "query";

/// The editable-fields snapshot stored in `changes.change` for a query.
///
/// Mirrors exactly the fields `query_service::create`/`update` can set, so a revert can copy
/// `snapshot` straight back onto the live `queries` row (see [`super::query_service`]'s revert/fork
/// docs).
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct QuerySnapshot {
    pub name: String,
    pub description: Option<String>,
    pub query: String,
    pub data_source_id: i64,
    pub options: Value,
    pub is_archived: bool,
}

/// Record a new `changes` row for `query_id`, capturing `snapshot` as the state being published.
/// Called only by `query_service::publish` — every other write (create/update/fork/revert)
/// leaves history untouched.
pub async fn record(
    db: &impl ConnectionTrait,
    query_id: i64,
    user_id: i64,
    snapshot: &QuerySnapshot,
) -> Result<change::Model, ApiError> {
    let model = change::ActiveModel {
        object_type: Set(QUERY_OBJECT_TYPE.to_string()),
        object_id: Set(query_id),
        user_id: Set(user_id),
        change: Set(serde_json::to_value(snapshot).map_err(|e| {
            ApiError::internal(format!("failed to serialize change snapshot: {e}"))
        })?),
        ..Default::default()
    };
    Ok(model.insert(db).await?)
}

/// All `changes` rows for `query_id`, oldest first — the ordering [`with_version_numbers`] numbers
/// from.
///
/// Callers are expected to have already access-checked `query_id` (mirrors
/// `query_service::get_for_user`'s own convention of checking access before reading).
pub async fn list_for_query(
    db: &impl ConnectionTrait,
    query_id: i64,
) -> Result<Vec<change::Model>, ApiError> {
    Ok(change::Entity::find()
        .filter(change::Column::ObjectType.eq(QUERY_OBJECT_TYPE))
        .filter(change::Column::ObjectId.eq(query_id))
        .order_by(change::Column::Id, Order::Asc)
        .all(db)
        .await?)
}

/// One `changes` row annotated with its 1-indexed version number, computed purely from
/// ordering (no schema column) — the oldest change for a query is version 1, the next is
/// version 2, and so on.
#[derive(Debug, Clone)]
pub struct VersionedChange {
    pub version: i64,
    pub change: change::Model,
}

/// Annotate `changes` (already ordered oldest-first, as [`list_for_query`] returns) with
/// 1-indexed version numbers.
#[must_use]
#[allow(
    clippy::cast_possible_wrap,
    reason = "a query's version count is nowhere near i64::MAX"
)]
pub fn with_version_numbers(changes: Vec<change::Model>) -> Vec<VersionedChange> {
    changes
        .into_iter()
        .enumerate()
        .map(|(i, change)| VersionedChange {
            version: (i as i64) + 1,
            change,
        })
        .collect()
}

/// Fetch a single `changes` row by id, scoped to `query_id`, and parse it into a [`QuerySnapshot`].
///
/// Used by revert. The scoping means a change id belonging to a different query 404s rather than
/// leaking cross-query data.
pub async fn get_snapshot(
    db: &impl ConnectionTrait,
    query_id: i64,
    change_id: i64,
) -> Result<QuerySnapshot, ApiError> {
    let row = change::Entity::find_by_id(change_id)
        .one(db)
        .await?
        .filter(|c| c.object_type == QUERY_OBJECT_TYPE && c.object_id == query_id)
        .ok_or_else(|| ApiError::not_found("change not found"))?;

    parse_snapshot(row)
}

/// Fetch a single `changes` row by its 1-indexed version number and parse its snapshot.
///
/// The number is computed the same way [`with_version_numbers`] does. This is the
/// `{"version": N}` revert-body alternative to `get_snapshot`'s `change_id` lookup.
pub async fn get_snapshot_by_version(
    db: &impl ConnectionTrait,
    query_id: i64,
    version: i64,
) -> Result<QuerySnapshot, ApiError> {
    validate_version(version)?;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "validate_version guarantees version >= 1, and i64 fits usize on 64-bit servers"
    )]
    let row = list_for_query(db, query_id)
        .await?
        .into_iter()
        .nth((version - 1) as usize)
        .ok_or_else(|| ApiError::not_found("version not found"))?;

    parse_snapshot(row)
}

fn parse_snapshot(row: change::Model) -> Result<QuerySnapshot, ApiError> {
    serde_json::from_value(row.change)
        .map_err(|e| ApiError::internal(format!("failed to parse change snapshot: {e}")))
}

/// Pure guard extracted so it can be unit-tested without a DB connection.
fn validate_version(version: i64) -> Result<(), ApiError> {
    if version < 1 {
        return Err(ApiError::bad_request("version must be >= 1"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_version_numbers_assigns_1_indexed_oldest_first() {
        let now = chrono::Utc::now().into();
        let make = |id: i64| change::Model {
            id,
            object_type: QUERY_OBJECT_TYPE.to_string(),
            object_id: 1,
            user_id: 1,
            change: serde_json::json!({}),
            created_at: now,
            updated_at: now,
        };
        let versioned = with_version_numbers(vec![make(10), make(11), make(12)]);
        assert_eq!(versioned[0].version, 1);
        assert_eq!(versioned[0].change.id, 10);
        assert_eq!(versioned[1].version, 2);
        assert_eq!(versioned[2].version, 3);
        assert_eq!(versioned[2].change.id, 12);
    }

    #[test]
    fn with_version_numbers_empty_vec_returns_empty() {
        let versioned = with_version_numbers(vec![]);
        assert!(versioned.is_empty());
    }

    #[test]
    fn get_snapshot_by_version_rejects_zero() {
        let err = validate_version(0).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.message.contains("version must be >= 1"));
    }

    #[test]
    fn get_snapshot_by_version_rejects_negative() {
        let err = validate_version(-5).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn get_snapshot_by_version_accepts_one() {
        assert!(validate_version(1).is_ok());
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let snapshot = QuerySnapshot {
            name: "Test".to_string(),
            description: None,
            query: "SELECT 1".to_string(),
            data_source_id: 1,
            options: serde_json::json!({}),
            is_archived: false,
        };
        let value = serde_json::to_value(&snapshot).unwrap();
        let parsed: QuerySnapshot = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, snapshot);
    }

    #[test]
    fn snapshot_description_none_survives_json_round_trip() {
        // description is Option<String> — must survive None without key or with null
        let json_without_key = serde_json::json!({
            "name": "q", "query": "SELECT 1", "data_source_id": 1,
            "options": {}, "is_archived": false
        });
        let parsed: QuerySnapshot = serde_json::from_value(json_without_key).unwrap();
        assert_eq!(parsed.description, None);
    }

    #[test]
    fn snapshot_description_present_survives_json_round_trip() {
        let snap = QuerySnapshot {
            name: "q".to_string(),
            description: Some("hello".to_string()),
            query: "SELECT 1".to_string(),
            data_source_id: 1,
            options: serde_json::json!({}),
            is_archived: false,
        };
        let v = serde_json::to_value(&snap).unwrap();
        let parsed: QuerySnapshot = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.description, Some("hello".to_string()));
    }
}
