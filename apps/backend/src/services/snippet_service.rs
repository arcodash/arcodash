//! Query snippet business logic — CRUD over `query_snippets`. No HTTP types here;
//! `api/handlers/snippets.rs` maps these to request/response JSON.
//!
//! ## Access semantics
//!
//! Snippets are org-shared query-authoring building blocks: reads (`list_for_org`/`get_for_org`)
//! require `view_snippets`; writes (`create`/`update`/`delete`) require `manage_snippets`. Both
//! gates are org-wide, not per-row — any holder of `manage_snippets` may update or delete **any**
//! snippet in the org, not just their own, since a snippet has no per-object owner check. Gating
//! independently of query authorship (`create_query`) lets an org grant snippet access without
//! also granting query authorship, and vice versa. Admins always pass via
//! [`user_has_permission`]'s admin-implies-everything rule. [`get_for_org`] 404s (rather than
//! 403s) when the snippet belongs to a different org, matching the no-access-vs-doesnt-exist
//! convention used elsewhere in this codebase.

use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, Order, QueryFilter, QueryOrder,
};

use arcodash_core::entities::{group, query_snippet, user};
use arcodash_core::events::record_event;

use crate::authz::permissions::{load_effective_groups, user_has_permission};
use crate::error::ApiError;

/// This module's write gate.
const WRITE_PERMISSION: &str = "manage_snippets";
/// This module's read gate.
const READ_PERMISSION: &str = "view_snippets";

fn check_permission(
    user: &user::Model,
    groups: &[group::Model],
    permission: &str,
) -> Result<(), ApiError> {
    if user_has_permission(user, groups, permission) {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!(
            "{permission} permission required"
        )))
    }
}

async fn require_permission(
    db: &impl ConnectionTrait,
    user: &user::Model,
    permission: &str,
) -> Result<(), ApiError> {
    let groups = load_effective_groups(db, user).await?;
    check_permission(user, &groups, permission)
}

/// `GET /api/query_snippets` — every snippet in the org. Requires `view_snippets` (see module
/// docs); no further per-object restriction beyond org-scoping.
pub async fn list_for_org(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
) -> Result<Vec<query_snippet::Model>, ApiError> {
    require_permission(db, user, READ_PERMISSION).await?;

    Ok(query_snippet::Entity::find()
        .filter(query_snippet::Column::OrgId.eq(org_id))
        .order_by(query_snippet::Column::Trigger, Order::Asc)
        .all(db)
        .await?)
}

/// `GET /api/query_snippets/<id>` — fetch one, org-scoped. Requires `view_snippets`.
pub async fn get_for_org(
    db: &impl ConnectionTrait,
    org_id: i64,
    id: i64,
    user: &user::Model,
) -> Result<query_snippet::Model, ApiError> {
    require_permission(db, user, READ_PERMISSION).await?;

    query_snippet::Entity::find_by_id(id)
        .one(db)
        .await?
        .filter(|s| s.org_id == org_id)
        .ok_or_else(|| ApiError::not_found("snippet not found"))
}

/// `POST /api/query_snippets` — create. Requires `manage_snippets`.
pub async fn create(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    trigger: String,
    description: Option<String>,
    snippet: String,
) -> Result<query_snippet::Model, ApiError> {
    require_permission(db, user, WRITE_PERMISSION).await?;

    let model = query_snippet::ActiveModel {
        org_id: Set(org_id),
        trigger: Set(trigger.clone()),
        description: Set(description),
        snippet: Set(snippet),
        user_id: Set(user.id),
        ..Default::default()
    };

    let created = model.insert(db).await?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "snippet.created",
        Some("snippet"),
        Some(created.id),
        serde_json::json!({ "trigger": trigger }),
    )
    .await?;

    Ok(created)
}

/// `POST /api/query_snippets/<id>` — update trigger/description/body. Requires `manage_snippets`;
/// any member with that permission may edit any org snippet (see module docs).
pub async fn update(
    db: &impl ConnectionTrait,
    user: &user::Model,
    existing: query_snippet::Model,
    trigger: Option<String>,
    description: Option<String>,
    snippet: Option<String>,
) -> Result<query_snippet::Model, ApiError> {
    require_permission(db, user, WRITE_PERMISSION).await?;

    let org_id = existing.org_id;
    let mut active: query_snippet::ActiveModel = existing.into();

    if let Some(trigger) = trigger {
        active.trigger = Set(trigger);
    }
    if let Some(description) = description {
        active.description = Set(Some(description));
    }
    if let Some(snippet) = snippet {
        active.snippet = Set(snippet);
    }

    let updated = active.update(db).await?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "snippet.updated",
        Some("snippet"),
        Some(updated.id),
        serde_json::json!({ "trigger": updated.trigger }),
    )
    .await?;

    Ok(updated)
}

/// `DELETE /api/query_snippets/<id>` — requires `manage_snippets` (see module docs).
pub async fn delete(
    db: &impl ConnectionTrait,
    user: &user::Model,
    existing: query_snippet::Model,
) -> Result<(), ApiError> {
    require_permission(db, user, WRITE_PERMISSION).await?;

    query_snippet::Entity::delete_by_id(existing.id)
        .exec(db)
        .await?;

    record_event(
        db,
        existing.org_id,
        Some(user.id),
        "snippet.deleted",
        Some("snippet"),
        Some(existing.id),
        serde_json::json!({ "trigger": existing.trigger }),
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcodash_core::test_support::{group_model, user_model};

    #[test]
    fn require_write_permission_allows_manage_snippets_group() {
        let group = group_model(1, 1, "editors", vec!["manage_snippets".to_string()]);
        let user = user_model(1, 1, vec![1]);
        assert!(check_permission(&user, &[group], WRITE_PERMISSION).is_ok());
    }

    #[test]
    fn require_write_permission_allows_admin() {
        let group = group_model(1, 1, "admin", vec!["admin".to_string()]);
        let user = user_model(1, 1, vec![1]);
        assert!(check_permission(&user, &[group], WRITE_PERMISSION).is_ok());
    }

    #[test]
    fn require_write_permission_rejects_view_only() {
        let group = group_model(1, 1, "viewers", vec!["view_snippets".to_string()]);
        let user = user_model(1, 1, vec![1]);
        let err = check_permission(&user, &[group], WRITE_PERMISSION).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn require_read_permission_allows_view_snippets() {
        let group = group_model(1, 1, "viewers", vec!["view_snippets".to_string()]);
        let user = user_model(1, 1, vec![1]);
        assert!(check_permission(&user, &[group], READ_PERMISSION).is_ok());
    }

    #[test]
    fn require_read_permission_rejects_no_permission() {
        let group = group_model(1, 1, "nobody", vec![]);
        let user = user_model(1, 1, vec![1]);
        let err = check_permission(&user, &[group], READ_PERMISSION).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
    }
}
