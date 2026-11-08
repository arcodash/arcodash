//! Batch id → display-name resolution, shared by any read-time surface that would otherwise show
//! a raw foreign-key id to a human (audit log, query version history) instead of joining per row.

use std::collections::{HashMap, HashSet};

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use arcodash_core::entities::user;

use crate::db::Db;
use crate::error::ApiError;

/// Resolve every distinct id in `ids` to its user name in one query, scoped to `org_id` so a caller
/// can never resolve (or thereby confirm the existence of) a user from another org.
///
/// An id with no matching row in this org (e.g. a deleted user, or one that belongs to another org)
/// is simply absent from the result map — callers fall back to the raw id.
pub async fn resolve_user_names(
    db: &Db,
    org_id: i64,
    ids: impl IntoIterator<Item = i64>,
) -> Result<HashMap<i64, String>, ApiError> {
    let ids: HashSet<i64> = ids.into_iter().collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(user::Entity::find()
        .filter(user::Column::Id.is_in(ids))
        .filter(user::Column::OrgId.eq(org_id))
        .all(db)
        .await?
        .into_iter()
        .map(|u| (u.id, u.name))
        .collect())
}
