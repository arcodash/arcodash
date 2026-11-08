//! Widget CRUD — business logic for `/api/widgets*`.
//!
//! A widget has no object-access of its own — every action inherits its parent dashboard's
//! access: reads need `view_dashboard` + Viewer
//! reach, create/update/delete need `edit_dashboard` + Editor reach.
//!
//! House rule for any `find_by_id`/`update_many`/`delete_by_id` keyed only on a request-supplied
//! id: it must also be scoped to the caller's org, either with a SQL `.filter(OrgId.eq(org_id))`
//! (or, for a table with no `org_id` column of its own, a filter on the org-scoped parent id —
//! see [`dashboard_exists`]) or a post-fetch `org_id`/parent comparison in Rust. `authorize`'s
//! admin short-circuit grants `ObjectRole::Owner` on any id without checking it exists or belongs
//! to the caller's org, so a bare `find_by_id` ahead of `authorize` is reachable cross-org by an
//! admin even though every non-admin path is still correctly scoped.

use chrono::Utc;
use sea_orm::sea_query::Expr;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter,
    Statement, TransactionTrait,
};
use serde_json::Value;

use arcodash_core::entities::{
    access_permission, api_key, change, dashboard, embed, favorite, monitor, query, schedule, user,
    visualization, widget,
};
use arcodash_core::events::record_event;

use crate::authz::authorize::{self, Action, InheritedVerb, QueryVerb};
use crate::error::ApiError;
use crate::services::query_service;

/// Bare existence+org-scope check, no capability/reach gate — `authorize`'s admin short-circuit
/// treats any id as `ObjectRole::Owner` without ever checking existence (fine once a real row is
/// already in hand, not for a bare id straight from a request body), so a mutation whose only
/// object reference is such an id needs this ahead of `authorize` to turn a would-be
/// foreign-key error into a clean 404. Deliberately does NOT require `view_dashboard` — that's
/// not part of the widget action's spec (only `edit_dashboard` is), and folding it in here would
/// make widget creation more restrictive than plain dashboard editing.
async fn dashboard_exists(
    db: &impl ConnectionTrait,
    org_id: i64,
    dashboard_id: i64,
) -> Result<(), ApiError> {
    dashboard::Entity::find_by_id(dashboard_id)
        .filter(dashboard::Column::OrgId.eq(org_id))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("dashboard not found"))?;
    Ok(())
}

/// Editor reach + `edit_dashboard` on the parent dashboard: every widget mutation is an edit of it.
async fn authorize_edit(
    db: &impl ConnectionTrait,
    user: &user::Model,
    dashboard_id: i64,
) -> Result<(), ApiError> {
    authorize::authorize(
        db,
        user,
        Action::Widget {
            verb: InheritedVerb::Edit,
            dashboard_id,
        },
    )
    .await?;
    Ok(())
}

/// Loads widget `id` for a mutation. `authorize`'s admin short-circuit never runs the
/// dashboard-org lookup (it grants `Owner` on any id without checking existence), so
/// `dashboard_exists` is the actual org gate for an admin caller.
async fn get_for_edit(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    id: i64,
) -> Result<widget::Model, ApiError> {
    let w = widget::Entity::find_by_id(id)
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("widget not found"))?;
    dashboard_exists(db, org_id, w.dashboard_id).await?;
    authorize_edit(db, user, w.dashboard_id).await?;
    Ok(w)
}

/// Row below the lowest tile (`row + sizeY`, sizeY defaulting to 2); mirrors the Add-widget
/// dialog so AI-created and manual tiles stack alike.
fn next_row<'a>(options: impl Iterator<Item = &'a Value>) -> i64 {
    options
        .map(|o| {
            let p = &o["position"];
            p["row"].as_i64().unwrap_or(0) + p["sizeY"].as_i64().unwrap_or(2)
        })
        .fold(0, i64::max)
}

/// Default `options` for a new 3-wide tile placed under the dashboard's existing widgets.
pub async fn next_position(
    db: &impl ConnectionTrait,
    dashboard_id: i64,
) -> Result<Value, ApiError> {
    let widgets = widget::Entity::find()
        .filter(widget::Column::DashboardId.eq(dashboard_id))
        .all(db)
        .await?;
    let row = next_row(widgets.iter().map(|w| &w.options));
    Ok(serde_json::json!({ "position": { "col": 0, "row": row, "sizeX": 3, "sizeY": 2 } }))
}

/// `POST /api/widgets` — create a widget on a dashboard.
pub async fn create(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    dashboard_id: i64,
    visualization_id: Option<i64>,
    text: Option<String>,
    width: i32,
    options: Value,
) -> Result<widget::Model, ApiError> {
    dashboard_exists(db, org_id, dashboard_id).await?;
    authorize_edit(db, user, dashboard_id).await?;

    // Verify the visualization belongs to a query in this org.
    if let Some(viz_id) = visualization_id {
        let viz = visualization::Entity::find_by_id(viz_id)
            .one(db)
            .await?
            .ok_or_else(|| ApiError::not_found("visualization not found"))?;
        // Reuses the org+access check from query_service — also verifies data-source access.
        let viz_query = query_service::get_for_user(db, org_id, user, viz.query_id).await?;
        query_service::require_published(&viz_query)?;
    }

    let now = Utc::now();
    let model = widget::ActiveModel {
        dashboard_id: Set(dashboard_id),
        visualization_id: Set(visualization_id),
        text: Set(text),
        width: Set(width),
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
        "widget.created",
        Some("widget"),
        Some(created.id),
        serde_json::json!({ "dashboard_id": dashboard_id }),
    )
    .await?;

    Ok(created)
}

/// `POST /api/widgets/:id` — update position/options/text.
///
/// `options` is merged into the stored jsonb, not assigned wholesale: `dashboard_service::
/// save_layout` writes the tile's `"position"` into this same column, so an unconditional
/// overwrite here would wipe the dashboard layout for this tile on any unrelated options edit.
/// `"position"` is stripped from the caller's patch first since it is server-owned — written
/// only by `save_layout`.
pub async fn update(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    id: i64,
    text: Option<String>,
    width: Option<i32>,
    mut options: Option<Value>,
) -> Result<widget::Model, ApiError> {
    let dashboard_id = get_for_edit(db, org_id, user, id).await?.dashboard_id;

    if let Some(obj) = options.as_mut().and_then(Value::as_object_mut) {
        obj.remove("position");
    }

    let mut q = widget::Entity::update_many()
        .filter(widget::Column::Id.eq(id))
        .filter(widget::Column::DashboardId.eq(dashboard_id));
    if let Some(t) = text {
        q = q.col_expr(widget::Column::Text, Expr::value(Some(t)));
    }
    if let Some(width) = width {
        q = q.col_expr(widget::Column::Width, Expr::value(width));
    }
    if let Some(o) = options {
        q = q.col_expr(
            widget::Column::Options,
            Expr::cust_with_values(
                "CASE WHEN jsonb_typeof(options) = 'object' THEN options ELSE '{}'::jsonb END \
                 || $1::jsonb",
                [o],
            ),
        );
    }
    q.col_expr(widget::Column::UpdatedAt, Expr::value(Utc::now()))
        .exec(db)
        .await?;

    let updated = widget::Entity::find_by_id(id)
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("widget not found"))?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "widget.updated",
        Some("widget"),
        Some(updated.id),
        serde_json::json!({ "dashboard_id": dashboard_id }),
    )
    .await?;

    Ok(updated)
}

/// `DELETE /api/widgets/:id`.
pub async fn delete(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    org_id: i64,
    user: &user::Model,
    id: i64,
) -> Result<(), ApiError> {
    let w = get_for_edit(db, org_id, user, id).await?;
    let dashboard_id = w.dashboard_id;
    widget::Entity::delete_many()
        .filter(widget::Column::Id.eq(id))
        .filter(widget::Column::DashboardId.eq(dashboard_id))
        .exec(db)
        .await?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "widget.deleted",
        Some("widget"),
        Some(w.id),
        serde_json::json!({ "dashboard_id": dashboard_id }),
    )
    .await?;

    if let Some(viz_id) = w.visualization_id {
        cleanup_ai_widget_query_isolated(db, user, viz_id).await;
    }

    Ok(())
}

/// Runs [`cleanup_ai_widget_query`] in its own nested transaction and swallows failures, so a
/// failed statement can never abort the caller's transaction.
pub(crate) async fn cleanup_ai_widget_query_isolated(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    user: &user::Model,
    viz_id: i64,
) {
    let cleanup = async {
        let tx = db.begin().await?;
        cleanup_ai_widget_query(&tx, user, viz_id).await?;
        tx.commit().await?;
        Ok::<_, ApiError>(())
    };
    if let Err(e) = cleanup.await {
        tracing::warn!(visualization_id = viz_id, error = %e.message, "ai widget cleanup failed");
    }
}

/// Tag carried by every query created through `POST /api/dashboards/:id/ai_widget`.
pub const AI_WIDGET_TAG: &str = "ai-widget";

/// Removes the query behind a just-deleted AI widget (its visualization cascades) when nothing
/// else uses it: tagged [`AI_WIDGET_TAG`], one visualization, no other widget on it, no
/// monitor/schedule/favorite/embed/grant/API key, never edited (a single `changes` row, the
/// creation publish, and draft columns equal to the published ones), and the caller may delete it.
async fn cleanup_ai_widget_query(
    db: &impl ConnectionTrait,
    user: &user::Model,
    viz_id: i64,
) -> Result<(), ApiError> {
    let Some(viz) = visualization::Entity::find_by_id(viz_id).one(db).await? else {
        return Ok(());
    };
    let Some(q) = query::Entity::find_by_id(viz.query_id).one(db).await? else {
        return Ok(());
    };
    let tagged = q
        .tags
        .as_ref()
        .is_some_and(|t| t.iter().any(|t| t == AI_WIDGET_TAG));
    let unedited = q.published_change_id.is_some()
        && !q.is_archived
        && q.published_name.as_deref() == Some(q.name.as_str())
        && q.published_description == q.description
        && q.published_query.as_deref() == Some(q.query.as_str())
        && q.published_data_source_id == Some(q.data_source_id)
        && q.published_options.as_ref() == Some(&q.options);
    if !tagged || !unedited {
        return Ok(());
    }

    let id = q.id;
    let t = "query";
    let in_use = visualization::Entity::find()
        .filter(visualization::Column::QueryId.eq(id))
        .count(db)
        .await?
        != 1
        || widget::Entity::find()
            .filter(widget::Column::VisualizationId.eq(viz_id))
            .count(db)
            .await?
            > 0
        || db
            .query_one_raw(Statement::from_sql_and_values(
                db.get_database_backend(),
                "SELECT 1 FROM dashboards WHERE published_snapshot->'widgets' \
                 @> jsonb_build_array(jsonb_build_object('visualization_id', $1::bigint)) LIMIT 1",
                [viz_id.into()],
            ))
            .await?
            .is_some()
        || change::Entity::find()
            .filter(change::Column::ObjectType.eq(t))
            .filter(change::Column::ObjectId.eq(id))
            .count(db)
            .await?
            != 1
        || monitor::Entity::find()
            .filter(monitor::Column::QueryId.eq(id))
            .count(db)
            .await?
            > 0
        || schedule::Entity::find()
            .filter(schedule::Column::EntityType.eq(t))
            .filter(schedule::Column::EntityId.eq(id))
            .count(db)
            .await?
            > 0
        || favorite::Entity::find()
            .filter(favorite::Column::ObjectType.eq(t))
            .filter(favorite::Column::ObjectId.eq(id))
            .count(db)
            .await?
            > 0
        || embed::Entity::find()
            .filter(embed::Column::ObjectType.eq(t))
            .filter(embed::Column::ObjectId.eq(id))
            .count(db)
            .await?
            > 0
        || access_permission::Entity::find()
            .filter(access_permission::Column::ObjectType.eq(t))
            .filter(access_permission::Column::ObjectId.eq(id))
            .count(db)
            .await?
            > 0
        || api_key::Entity::find()
            .filter(api_key::Column::ObjectType.eq(t))
            .filter(api_key::Column::ObjectId.eq(id))
            .count(db)
            .await?
            > 0;
    if in_use {
        return Ok(());
    }

    // Skip silently without delete rights; `query_service::delete` authorizes again itself.
    if authorize::authorize(
        db,
        user,
        Action::Query {
            verb: QueryVerb::Delete,
            id,
        },
    )
    .await
    .is_err()
    {
        return Ok(());
    }
    query_service::delete(db, user, q).await
}

#[cfg(test)]
mod tests {
    use super::next_row;
    use serde_json::json;

    #[test]
    fn next_row_stacks_below_lowest_tile() {
        assert_eq!(next_row(std::iter::empty()), 0);
        let opts = [
            json!({"position": {"row": 0, "sizeY": 3}}),
            json!({"position": {"row": 4}}),
            json!({}),
        ];
        assert_eq!(next_row(opts.iter()), 6);
    }
}
