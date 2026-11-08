//! `/api/dashboards*` — CRUD for dashboards, plus favorites and tag listing.

use crate::extract::{Json, Path};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use arcodash_core::entities::{dashboard, favorite, group, query, user, visualization, widget};

use crate::api::handlers::embeds::serve_public_embed;
use crate::api::handlers::visualizations::VisualizationResponse;
use crate::auth::{CurrentOrg, CurrentUser, OptionalCurrentUser};
use crate::authz::authorize::{self, Action, Caller, DashboardVerb};
use crate::db::Db;
use crate::error::ApiError;
use crate::services::{dashboard_service, embed_service, monitor_service};
use crate::state::AppState;

/// The `caller` block for dashboard `id`, via the Edit-tier `evaluate(...)` the mutation gates use.
/// `can_share` needs the `create_embed` capability and being creator or admin; an owner-tier ACL
/// grant is not enough.
async fn dashboard_caller(
    db: &Db,
    user: &arcodash_core::entities::user::Model,
    id: i64,
    owner_id: i64,
) -> Result<Caller, ApiError> {
    let groups = crate::authz::permissions::load_effective_groups(db, user).await?;
    let is_admin = crate::authz::permissions::is_admin(&groups);
    let eval = authorize::evaluate_with_groups(
        db,
        user,
        &groups,
        is_admin,
        Action::Dashboard {
            verb: DashboardVerb::Edit,
            id,
        },
    )
    .await?;
    let has_create_embed =
        crate::authz::permissions::user_has_permission(user, &groups, "create_embed");
    Ok(Caller::from(eval).with_share(has_create_embed, is_admin || owner_id == user.id))
}

/// Batched Edit-tier authz for a page of dashboards: one [`authorize::prefetch_roles`] covers every
/// row; `with_share` still applies per row (it needs each `owner_id`).
async fn dashboard_edit_evals(
    db: &Db,
    user: &user::Model,
    groups: &[group::Model],
    is_admin: bool,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, authorize::EvaluateResult>, ApiError> {
    let roles = authorize::prefetch_roles(db, user, groups, is_admin, "dashboard", ids).await?;
    let mut out = std::collections::HashMap::with_capacity(ids.len());
    for id in ids {
        let eval = authorize::evaluate_with_roles(
            db,
            user,
            groups,
            is_admin,
            &roles,
            Action::Dashboard {
                verb: DashboardVerb::Edit,
                id: *id,
            },
        )
        .await?;
        out.insert(*id, eval);
    }
    Ok(out)
}

/// A widget's underlying query without its SQL text, which a dashboard tile doesn't need.
#[derive(Debug, Serialize)]
pub struct QuerySummary {
    pub id: i64,
    pub name: String,
    pub data_source_id: i64,
    /// Parameter tokens referenced by the served SQL, sorted for a stable payload.
    pub parameter_names: Vec<String>,
    /// Served `options` (parameter definitions included), so the param bar can resolve each
    /// `{{token}}` without a second fetch.
    pub options: Value,
}

impl From<query::Model> for QuerySummary {
    fn from(q: query::Model) -> Self {
        // A dashboard tile always shows the served (published once published) content, regardless
        // of viewer.
        let resolved = crate::services::query_service::resolve_serving_content(&q);
        let mut parameter_names: Vec<String> =
            crate::services::parameters::referenced_names(&resolved.query)
                .into_iter()
                .collect();
        parameter_names.sort();
        Self {
            id: q.id,
            name: resolved.name,
            data_source_id: resolved.data_source_id,
            parameter_names,
            options: resolved.options,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct VizWithQuery {
    #[serde(flatten)]
    pub viz: VisualizationResponse,
    pub query: Option<QuerySummary>,
}

#[derive(Debug, Serialize)]
pub struct WidgetResponse {
    pub id: i64,
    pub dashboard_id: i64,
    pub visualization_id: Option<i64>,
    pub text: Option<String>,
    pub width: i32,
    pub options: Value,
    pub visualization: Option<VizWithQuery>,
    /// Pre-loaded cached result data; only populated on the public dashboard endpoint so anonymous
    /// viewers can render widgets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_result_data: Option<Value>,
    /// Tile freshness inputs; absent for a text widget or a query with no result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<dashboard_service::WidgetFreshness>,
    /// Monitor tiles from `widget.options.monitors`, in configured order; `None` if absent or
    /// malformed; ids the viewer can't see are dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitors: Option<Vec<monitor_service::MonitorTile>>,
    /// Whether the viewer can read this widget's query results (as `/api/queries/:id/results`);
    /// `true` for a text widget. Lets the frontend tell "never run" from "you can't see this".
    pub viewer_can_read_data: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[allow(clippy::too_many_arguments)]
fn widget_response(
    w: widget::Model,
    viz: Option<visualization::Model>,
    q: Option<query::Model>,
    query_result_data: Option<Value>,
    freshness: Option<dashboard_service::WidgetFreshness>,
    viewer_can_read_data: bool,
    monitors: Option<Vec<monitor_service::MonitorTile>>,
) -> WidgetResponse {
    let visualization = viz.map(|v| VizWithQuery {
        viz: VisualizationResponse::from(v),
        query: q.map(QuerySummary::from),
    });
    WidgetResponse {
        id: w.id,
        dashboard_id: w.dashboard_id,
        visualization_id: w.visualization_id,
        text: w.text,
        width: w.width,
        options: w.options,
        visualization,
        query_result_data,
        freshness,
        monitors,
        viewer_can_read_data,
        created_at: w.created_at.with_timezone(&Utc),
        updated_at: w.updated_at.with_timezone(&Utc),
    }
}

#[derive(Debug, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "response DTO; each bool is an independent flag in the JSON wire format"
)]
pub struct DashboardResponse {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub user_id: i64,
    pub visibility: String,
    pub layout: Value,
    pub dashboard_filters_enabled: bool,
    pub is_archived: bool,
    pub published: bool,
    pub has_unpublished_changes: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_change_summary: Option<dashboard_service::ChangeSummary>,
    pub tags: Vec<String>,
    pub options: Value,
    pub is_favorite: bool,
    pub widgets: Vec<WidgetResponse>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Owner display name; `None` only if the owning user row is missing. Populated by
    /// `dashboard_response`'s callers.
    pub owner_name: Option<String>,
    pub caller: Caller,
}

/// The `published`/`has_unpublished_changes`/`pending_change_summary` triple; the summary is only
/// computed for a caller who can edit AND has a pending publish.
async fn lifecycle_fields(
    db: &Db,
    d: &dashboard::Model,
    can_edit: bool,
) -> Result<(bool, bool, Option<dashboard_service::ChangeSummary>), ApiError> {
    if !dashboard_service::is_published(d) {
        return Ok((false, false, None));
    }
    let widgets = dashboard_service::load_widgets_with_viz(db, d.id).await?;
    let pending = dashboard_service::has_unpublished_changes(d, &widgets);
    if !can_edit || !pending {
        return Ok((true, pending, None));
    }
    let snapshot = d
        .published_snapshot
        .clone()
        .ok_or_else(|| ApiError::internal("published dashboard missing published_snapshot"))?;
    let snapshot: dashboard_service::DashboardSnapshot = serde_json::from_value(snapshot)
        .map_err(|e| ApiError::internal(format!("stored dashboard snapshot is malformed: {e}")))?;
    let summary = dashboard_service::change_summary(&widgets, &snapshot);
    Ok((true, pending, Some(summary)))
}

/// Strips `secret_token` (the anonymous-share bearer token) from a dashboard's `options` before any
/// authenticated response; only the `share` responses return it.
fn public_options(mut options: Value) -> Value {
    if let Some(map) = options.as_object_mut() {
        map.remove("secret_token");
    }
    options
}

#[allow(clippy::too_many_arguments)]
async fn dashboard_response(
    db: &Db,
    d: dashboard::Model,
    widgets: Vec<WidgetResponse>,
    is_favorite: bool,
    caller: Caller,
    published: bool,
    has_unpublished_changes: bool,
    pending_change_summary: Option<dashboard_service::ChangeSummary>,
) -> Result<DashboardResponse, ApiError> {
    let owner_name = user::Entity::find_by_id(d.user_id)
        .one(db)
        .await?
        .map(|u| u.name);
    Ok(DashboardResponse {
        id: d.id,
        slug: d.slug,
        name: d.name,
        user_id: d.user_id,
        visibility: d.visibility,
        layout: d.layout,
        dashboard_filters_enabled: d.dashboard_filters_enabled,
        is_archived: d.is_archived,
        published,
        has_unpublished_changes,
        pending_change_summary,
        tags: d.tags.unwrap_or_default(),
        options: public_options(d.options),
        is_favorite,
        widgets,
        created_at: d.created_at.with_timezone(&Utc),
        updated_at: d.updated_at.with_timezone(&Utc),
        owner_name,
        caller,
    })
}

/// The full response after a mutation: the live draft graph plus favorite/caller/lifecycle.
async fn draft_dashboard_json(
    db: &Db,
    user: &user::Model,
    d: dashboard::Model,
) -> Result<Json<DashboardResponse>, ApiError> {
    let widgets = build_widget_responses(db, d.id, user).await?;
    let fav = is_fav(db, user.id, d.id).await?;
    let caller = dashboard_caller(db, user, d.id, d.user_id).await?;
    let (published, pending, summary) = lifecycle_fields(db, &d, caller.can_edit).await?;
    Ok(Json(
        dashboard_response(db, d, widgets, fav, caller, published, pending, summary).await?,
    ))
}

/// Owner display names for a page of dashboards in one query.
async fn owner_names_for(
    db: &Db,
    user_ids: impl Iterator<Item = i64>,
) -> Result<std::collections::HashMap<i64, String>, ApiError> {
    let ids: std::collections::HashSet<i64> = user_ids.collect();
    Ok(user::Entity::find()
        .filter(user::Column::Id.is_in(ids))
        .all(db)
        .await?
        .into_iter()
        .map(|u| (u.id, u.name))
        .collect())
}

#[derive(Debug, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "response DTO; each bool is an independent flag in the JSON wire format"
)]
pub struct DashboardListItem {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub user_id: i64,
    pub visibility: String,
    pub is_archived: bool,
    pub published: bool,
    pub has_unpublished_changes: bool,
    pub tags: Vec<String>,
    pub is_favorite: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Owner display name; `None` only if the owning user row is missing. Populated by
    /// list/favorites handlers.
    pub owner_name: Option<String>,
    pub caller: Caller,
}

impl From<dashboard::Model> for DashboardListItem {
    fn from(d: dashboard::Model) -> Self {
        Self {
            published: dashboard_service::is_published(&d),
            id: d.id,
            slug: d.slug,
            name: d.name,
            user_id: d.user_id,
            visibility: d.visibility,
            is_archived: d.is_archived,
            has_unpublished_changes: false,
            tags: d.tags.unwrap_or_default(),
            is_favorite: false,
            created_at: d.created_at.with_timezone(&Utc),
            updated_at: d.updated_at.with_timezone(&Utc),
            owner_name: None, // populated by handlers that look up the owner's name
            caller: Caller::default(),
        }
    }
}

async fn build_widget_responses(
    db: &Db,
    dashboard_id: i64,
    user: &user::Model,
) -> Result<Vec<WidgetResponse>, ApiError> {
    let items = dashboard_service::load_widgets_with_viz(db, dashboard_id).await?;
    widget_responses_with_freshness(db, items, user).await
}

/// Like [`build_widget_responses`] but walking the published snapshot: the graph every
/// non-editor/public/embed read sees.
async fn build_published_widget_responses(
    db: &Db,
    d: &dashboard::Model,
    user: &user::Model,
) -> Result<Vec<WidgetResponse>, ApiError> {
    let items = dashboard_service::load_published_widgets_with_viz(db, d).await?;
    widget_responses_with_freshness(db, items, user).await
}

/// Builds every widget's response, attaching freshness and data-source access in one batched round
/// trip each.
async fn widget_responses_with_freshness(
    db: &Db,
    items: Vec<dashboard_service::WidgetWithViz>,
    user: &user::Model,
) -> Result<Vec<WidgetResponse>, ApiError> {
    let query_ids: Vec<i64> = items
        .iter()
        .filter_map(|i| i.viz_and_query.as_ref().map(|(v, _)| v.query_id))
        .collect();
    let fresh = dashboard_service::load_widget_freshness(db, &query_ids).await?;
    let access = dashboard_service::load_viewer_query_access(db, user, &query_ids).await?;

    // One visibility-filtered load and one enrichment call for all monitor tiles, sliced back per
    // widget in configured order.
    let monitor_ids: Vec<i64> = items
        .iter()
        .filter_map(|i| i.widget.options.get("monitors"))
        .filter_map(|v| v.as_array())
        .flatten()
        .filter_map(sea_orm::JsonValue::as_i64)
        .collect();
    let monitor_tiles: std::collections::HashMap<i64, monitor_service::MonitorTile> =
        if monitor_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            let pairs = monitor_service::list_for_user(
                db,
                user.org_id,
                user,
                None,
                None,
                None,
                Some(&monitor_ids),
            )
            .await?;
            let monitors: Vec<_> = pairs.into_iter().map(|(m, _)| m).collect();
            monitor_service::tile_data(db, &monitors).await?
        };

    Ok(items
        .into_iter()
        .map(|item| {
            let monitors = item
                .widget
                .options
                .get("monitors")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_i64)
                        .filter_map(|id| monitor_tiles.get(&id).cloned())
                        .collect::<Vec<_>>()
                });
            let (viz, q, freshness, can_read) = match item.viz_and_query {
                Some((v, q)) => {
                    let freshness = fresh.get(&v.query_id).cloned();
                    // No entry can't happen here; default to visible rather than falsely
                    // restricting.
                    let can_read = access.get(&v.query_id).copied().unwrap_or(true);
                    (Some(v), q, freshness, can_read)
                }
                None => (None, None, None, true),
            };
            widget_response(item.widget, viz, q, None, freshness, can_read, monitors)
        })
        .collect())
}

/// Fills in `has_unpublished_changes` (left `false` for a never-published dashboard); `published`
/// is already set in `From`.
async fn list_item_pending(db: &Db, d: &dashboard::Model) -> Result<bool, ApiError> {
    if !dashboard_service::is_published(d) {
        return Ok(false);
    }
    let widgets = dashboard_service::load_widgets_with_viz(db, d.id).await?;
    Ok(dashboard_service::has_unpublished_changes(d, &widgets))
}

async fn is_fav(db: &Db, user_id: i64, dashboard_id: i64) -> Result<bool, ApiError> {
    Ok(favorite::Entity::find()
        .filter(favorite::Column::UserId.eq(user_id))
        .filter(favorite::Column::ObjectType.eq("dashboard"))
        .filter(favorite::Column::ObjectId.eq(dashboard_id))
        .one(db)
        .await?
        .is_some())
}

#[derive(Debug, Deserialize, Default)]
pub struct ListDashboardsQuery {
    #[serde(default)]
    pub include_archived: bool,
    pub q: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub filter: Option<String>,
}

/// `GET /api/dashboards`: excludes archived unless `?include_archived=true`.
pub async fn list(
    db: Db,
    CurrentUser(user): CurrentUser,
    Query(params): Query<ListDashboardsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    super::validate_list_filter(params.filter.as_deref())?;

    let dashboards = dashboard_service::list(
        &db,
        user.org_id,
        &user,
        params.include_archived,
        params.q.filter(|s| !s.is_empty()),
        Some(params.tags).filter(|t| !t.is_empty()),
        params.filter.as_deref(),
    )
    .await?;

    let fav_ids: std::collections::HashSet<i64> = favorite::Entity::find()
        .filter(favorite::Column::UserId.eq(user.id))
        .filter(favorite::Column::ObjectType.eq("dashboard"))
        .all(&db)
        .await?
        .into_iter()
        .map(|f| f.object_id)
        .collect();

    Ok(Json(
        list_items(&db, &user, dashboards, |id| fav_ids.contains(&id)).await?,
    ))
}

/// List rows for `dashboards`, each with its pending-changes flag, owner name and `caller` block.
async fn list_items(
    db: &Db,
    user: &user::Model,
    dashboards: Vec<dashboard::Model>,
    is_favorite: impl Fn(i64) -> bool,
) -> Result<Vec<DashboardListItem>, ApiError> {
    // Groups/admin status are per-user: load once and reuse across every row's `caller` block.
    let groups = crate::authz::permissions::load_effective_groups(db, user).await?;
    let is_admin = crate::authz::permissions::is_admin(&groups);

    let owner_names = owner_names_for(db, dashboards.iter().map(|d| d.user_id)).await?;
    let has_create_embed =
        crate::authz::permissions::user_has_permission(user, &groups, "create_embed");

    let ids: Vec<i64> = dashboards.iter().map(|d| d.id).collect();
    let mut evals = dashboard_edit_evals(db, user, &groups, is_admin, &ids).await?;

    let mut items: Vec<DashboardListItem> = Vec::with_capacity(dashboards.len());
    for d in dashboards {
        let has_unpublished_changes = list_item_pending(db, &d).await?;
        let owner_name = owner_names.get(&d.user_id).cloned();
        let is_owner = d.user_id == user.id;
        // Always present (every id was passed to `dashboard_edit_evals`); the fallback only guards
        // against the lists drifting.
        let eval = evals.remove(&d.id).unwrap_or(authorize::EvaluateResult {
            object_role: None,
            can_edit: false,
        });
        items.push(DashboardListItem {
            has_unpublished_changes,
            is_favorite: is_favorite(d.id),
            owner_name,
            caller: Caller::from(eval).with_share(has_create_embed, is_admin || is_owner),
            ..DashboardListItem::from(d)
        });
    }
    Ok(items)
}

#[derive(Debug, Deserialize)]
pub struct CreateDashboardRequest {
    pub name: String,
}

/// `POST /api/dashboards`
pub async fn create(
    db: Db,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<CreateDashboardRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let d = dashboard_service::create(&db, user.org_id, &user, payload.name).await?;
    let widgets = build_widget_responses(&db, d.id, &user).await?;
    let caller = dashboard_caller(&db, &user, d.id, d.user_id).await?;
    Ok(Json(
        dashboard_response(&db, d, widgets, false, caller, false, false, None).await?,
    ))
}

/// `GET /api/dashboards/tags`
pub async fn tags(db: Db, CurrentUser(user): CurrentUser) -> Result<impl IntoResponse, ApiError> {
    let tags = dashboard_service::list_tags(&db, user.org_id).await?;
    Ok(Json(serde_json::json!({ "tags": tags })))
}

/// `GET /api/dashboards/favorites`
pub async fn favorites(
    db: Db,
    CurrentUser(user): CurrentUser,
) -> Result<impl IntoResponse, ApiError> {
    let dashboards = dashboard_service::list_favorites(&db, user.org_id, &user).await?;
    Ok(Json(list_items(&db, &user, dashboards, |_| true).await?))
}

#[derive(Debug, Deserialize, Default)]
pub struct GetDashboardQuery {
    /// Request the live draft graph; honored only for a caller who can edit, otherwise the
    /// published snapshot is served.
    #[serde(default)]
    pub draft: bool,
}

/// `GET /api/dashboards/:slug`: serves the live draft for an editor who asked (`?draft=true`) or
/// whose dashboard was never published; everyone else gets the published snapshot.
///
/// The one seam deciding which graph a read returns.
pub async fn get(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(slug): Path<String>,
    Query(params): Query<GetDashboardQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let d = dashboard_service::get_by_slug(&db, user.org_id, &user, &slug).await?;
    let fav = is_fav(&db, user.id, d.id).await?;
    let caller = dashboard_caller(&db, &user, d.id, d.user_id).await?;
    let widgets = if caller.can_edit && (params.draft || !dashboard_service::is_published(&d)) {
        build_widget_responses(&db, d.id, &user).await?
    } else {
        build_published_widget_responses(&db, &d, &user).await?
    };
    let (published, pending, summary) = lifecycle_fields(&db, &d, caller.can_edit).await?;
    Ok(Json(
        dashboard_response(&db, d, widgets, fav, caller, published, pending, summary).await?,
    ))
}

/// `is_published`/`published` are deliberately not fields: publish state changes only via
/// `/publish` and `/unpublish`. `deny_unknown_fields` turns a client sending either into a 422.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UpdateDashboardRequest {
    pub name: Option<String>,
    pub tags: Option<Vec<String>>,
    pub options: Option<Value>,
    pub is_archived: Option<bool>,
    pub dashboard_filters_enabled: Option<bool>,
    pub layout: Option<Value>,
}

/// `POST /api/dashboards/:slug`
pub async fn update(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(slug): Path<String>,
    Json(payload): Json<UpdateDashboardRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let d = dashboard_service::update(
        &db,
        user.org_id,
        &user,
        &slug,
        payload.name,
        payload.tags,
        payload.options,
        payload.is_archived,
        payload.dashboard_filters_enabled,
        payload.layout,
    )
    .await?;
    draft_dashboard_json(&db, &user, d).await
}

/// `POST /api/dashboards/:slug/publish`: overwrite `published_snapshot` with the current draft.
pub async fn publish(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(slug): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let d = dashboard_service::get_by_slug(&db, user.org_id, &user, &slug).await?;
    let d = dashboard_service::publish(&db, &user, d).await?;
    draft_dashboard_json(&db, &user, d).await
}

/// `POST /api/dashboards/:slug/unpublish` — return a live dashboard to draft.
pub async fn unpublish(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(slug): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let d = dashboard_service::get_by_slug(&db, user.org_id, &user, &slug).await?;
    let d = dashboard_service::unpublish(&db, &user, d).await?;
    draft_dashboard_json(&db, &user, d).await
}

/// `POST /api/dashboards/:slug/discard`: overwrite the live draft from `published_snapshot`.
pub async fn discard(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(slug): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let d = dashboard_service::get_by_slug(&db, user.org_id, &user, &slug).await?;
    let d = dashboard_service::discard_changes(&db, &user, d).await?;
    draft_dashboard_json(&db, &user, d).await
}

#[derive(Debug, Deserialize)]
pub struct SaveLayoutRequest {
    pub positions: Vec<dashboard_service::LayoutPosition>,
}

/// `PUT /api/dashboards/:slug/layout`: batched, atomic position write for a multi-tile drag.
pub async fn save_layout(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(slug): Path<String>,
    Json(payload): Json<SaveLayoutRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let widgets =
        dashboard_service::save_layout(&db, user.org_id, &user, &slug, payload.positions).await?;
    Ok(Json(
        widgets
            .into_iter()
            .map(crate::api::handlers::widgets::WidgetResponse::from)
            .collect::<Vec<_>>(),
    ))
}

/// `DELETE /api/dashboards/:slug` — archives (soft-delete).
pub async fn delete(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(slug): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    dashboard_service::archive(&db, user.org_id, &user, &slug).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/dashboards/:id/favorite`
pub async fn star(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    dashboard_service::star(&db, user.org_id, &user, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/dashboards/:id/favorite`
pub async fn unstar(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    dashboard_service::unstar(&db, user.org_id, &user, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/dashboards/:slug/share` — generate (or return existing) public secret token.
pub async fn share(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(slug): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let token = dashboard_service::share(&db, &state, user.org_id, &user, &slug).await?;
    Ok(share_json(&state, &token))
}

/// `PUT /api/dashboards/:slug/share`: rotate the public token, invalidating any earlier link.
pub async fn rotate_share(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(slug): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let token = dashboard_service::rotate_share(&db, &state, user.org_id, &user, &slug).await?;
    Ok(share_json(&state, &token))
}

fn share_json(state: &AppState, token: &str) -> Json<Value> {
    let public_url = format!("{}/public/dashboards/{}", state.config.base_url, token);
    Json(serde_json::json!({ "secret_token": token, "public_url": public_url }))
}

/// `DELETE /api/dashboards/:slug/share` — revoke the public token.
pub async fn revoke_share(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(slug): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    dashboard_service::revoke_share(&db, user.org_id, &user, &slug).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/dashboards/public/:token`: deprecated in favor of `/api/embed/{token}`.
///
/// Goes through `embed_service::resolve_public_embed`, the one guard every anonymous read uses;
/// `Some(EmbedObjectType::Dashboard)` rejects token type confusion. The body comes from
/// `embed_service::serialize_public`, never the full `dashboard_response`, which would expose
/// widget SQL and `options`. Access depends on `state.public_dashboard_access`: `Anonymous` needs
/// no auth; `OrgMemberOnly` serves `anon_authorized` links to anyone and requires org membership
/// otherwise (401). Every other failure returns a uniform 404, so the guard is never an oracle.
pub async fn public_get(
    State(state): State<AppState>,
    db: Db,
    CurrentOrg(org): CurrentOrg,
    OptionalCurrentUser(optional_current_user): OptionalCurrentUser,
    Path(token): Path<String>,
) -> impl IntoResponse {
    serve_public_embed(
        &db,
        &state,
        &org,
        &token,
        Some(embed_service::EmbedObjectType::Dashboard),
        optional_current_user.as_ref().map(|u| u.org_id),
    )
    .await
}
