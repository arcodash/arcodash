//! Visualization CRUD — business logic for `/api/visualizations*`.
//!
//! A visualization has no object-access of its own — every action inherits its parent query's
//! access: reads need `view_query` + Viewer reach,
//! create/update/delete need `edit_query` + Editor reach, routed through
//! `authz::authorize::Action::Visualization`.

use chrono::Utc;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter,
};
use serde_json::Value;

use arcodash_core::entities::{query, user, visualization, widget};
use arcodash_core::events::record_event;

use crate::authz::authorize::{self, Action, InheritedVerb};
use crate::error::ApiError;
use crate::services::query_service;

/// Bare existence+org-scope check, no capability/reach gate — `authorize`'s admin short-circuit
/// treats any id as `ObjectRole::Owner` without ever checking existence (fine once a real row is
/// already in hand, not for a bare id straight from a request body), so a mutation whose only
/// object reference is such an id needs this ahead of `authorize` to turn a would-be
/// foreign-key error into a clean 404. Deliberately does NOT require `view_query` — that's not
/// part of the visualization action's spec (only `edit_query` is), and folding it in here would
/// make visualization creation more restrictive than plain query editing.
async fn query_exists(
    db: &impl ConnectionTrait,
    org_id: i64,
    query_id: i64,
) -> Result<(), ApiError> {
    query::Entity::find_by_id(query_id)
        .filter(query::Column::OrgId.eq(org_id))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("query not found"))?;
    Ok(())
}

async fn authorize_viz(
    db: &impl ConnectionTrait,
    user: &user::Model,
    verb: InheritedVerb,
    query_id: i64,
) -> Result<(), ApiError> {
    authorize::authorize(db, user, Action::Visualization { verb, query_id }).await?;
    Ok(())
}

/// [`get_for_user`] plus the `edit_query` + Editor-reach gate update/delete need.
async fn get_for_edit(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    id: i64,
) -> Result<visualization::Model, ApiError> {
    let viz = get_for_user(db, org_id, user, id).await?;
    authorize_viz(db, user, InheritedVerb::Edit, viz.query_id).await?;
    Ok(viz)
}

/// Get a visualization by id, also checking the caller can view its parent query.
pub async fn get_for_user(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    id: i64,
) -> Result<visualization::Model, ApiError> {
    let viz = visualization::Entity::find_by_id(id)
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("visualization not found"))?;

    // Access check via parent query — 404s on no-access, same as query endpoints.
    query_service::get_for_user(db, org_id, user, viz.query_id).await?;
    authorize_viz(db, user, InheritedVerb::View, viz.query_id).await?;
    Ok(viz)
}

/// Load all visualizations for a query (no auth check — caller must have already verified
/// access to the parent query). Used by `GET /api/queries/:id/visualizations`.
pub async fn list_for_query(
    db: &impl ConnectionTrait,
    query_id: i64,
) -> Result<Vec<visualization::Model>, ApiError> {
    Ok(visualization::Entity::find()
        .filter(visualization::Column::QueryId.eq(query_id))
        .all(db)
        .await?)
}

/// `POST /api/visualizations` — create.
pub async fn create(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    query_id: i64,
    viz_type: String,
    name: String,
    description: Option<String>,
    options: Value,
) -> Result<visualization::Model, ApiError> {
    query_exists(db, org_id, query_id).await?;
    // Adding a visualization is an edit of the parent query's content — Editor reach + edit_query,
    // not just View.
    authorize_viz(db, user, InheritedVerb::Edit, query_id).await?;

    let now = Utc::now();
    let model = visualization::ActiveModel {
        query_id: Set(query_id),
        r#type: Set(viz_type),
        name: Set(name),
        description: Set(description),
        options: Set(options),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
        ..Default::default()
    };

    let created = model.insert(db).await?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "visualization.created",
        Some("visualization"),
        Some(created.id),
        serde_json::json!({ "query_id": query_id }),
    )
    .await?;

    Ok(created)
}

/// `POST /api/visualizations/:id` — update name/type/description/options. Requires `edit_query`
/// + Editor reach on the parent query.
pub async fn update(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    id: i64,
    viz_type: Option<String>,
    name: Option<String>,
    description: Option<String>,
    options: Option<Value>,
) -> Result<visualization::Model, ApiError> {
    let viz = get_for_edit(db, org_id, user, id).await?;

    let query_id = viz.query_id;
    let mut active: visualization::ActiveModel = viz.into();
    if let Some(t) = viz_type {
        active.r#type = Set(t);
    }
    if let Some(n) = name {
        active.name = Set(n);
    }
    if let Some(d) = description {
        active.description = Set(Some(d));
    }
    if let Some(o) = options {
        active.options = Set(o);
    }
    active.updated_at = Set(Utc::now().into());

    let updated = active.update(db).await?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "visualization.updated",
        Some("visualization"),
        Some(updated.id),
        serde_json::json!({ "query_id": query_id }),
    )
    .await?;

    Ok(updated)
}

/// `DELETE /api/visualizations/:id`. Requires `edit_query` + Editor reach on the parent query.
///
/// The cascade (`ON DELETE CASCADE` on `widgets.visualization_id`) can silently drop tiles from
/// dashboards this caller doesn't own and was never asked to affect — the delete itself isn't
/// gated on that, but it doesn't happen invisibly: the dropped count is logged and carried on the
/// audit event.
pub async fn delete(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    id: i64,
) -> Result<(), ApiError> {
    let viz = get_for_edit(db, org_id, user, id).await?;

    let query_id = viz.query_id;
    let cascaded_widget_count = widget::Entity::find()
        .filter(widget::Column::VisualizationId.eq(id))
        .count(db)
        .await?;

    visualization::Entity::delete_by_id(id).exec(db).await?;

    if cascaded_widget_count > 0 {
        tracing::warn!(
            visualization_id = id,
            cascaded_widget_count,
            "visualization delete cascaded to widgets on other dashboards"
        );
    }

    record_event(
        db,
        org_id,
        Some(user.id),
        "visualization.deleted",
        Some("visualization"),
        Some(id),
        serde_json::json!({ "query_id": query_id, "cascaded_widget_count": cascaded_widget_count }),
    )
    .await?;

    Ok(())
}
