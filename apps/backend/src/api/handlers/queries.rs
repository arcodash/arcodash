//! `/api/queries*` and `/api/query_results*`: CRUD, execute, the parameter engine, version history,
//! draft/publish, archive and tags.

use std::sync::Arc;

use axum::extract::{Extension, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use arcodash_core::entities::{favorite, group, query, query_result, user};
use arcodash_core::events::record_event;

use crate::api::handlers::jobs::JobResponse;
use crate::api::handlers::runs::RunResponse;
use crate::api::handlers::visualizations::VisualizationResponse;
use crate::auth::CurrentUser;
use crate::authz::authorize::{self, Action, Caller, QueryVerb};
use crate::db::Db;
use crate::error::ApiError;
use crate::extract::{Json, Path};
use crate::middleware::rate_limit::UserKeyedLimiter;
use crate::services::query_service::RevertTarget;
use crate::services::result_service::ResultFormat;
use crate::services::{
    change_service, display_names, query_service, result_service, run_service,
    visualization_service,
};
use crate::state::AppState;

/// The `caller` block for query `id`: Edit-tier authz summary plus query-only execute visibility.
/// Loads groups once and delegates to [`query_caller_with_groups`].
pub(crate) async fn query_caller(
    db: &Db,
    user: &user::Model,
    id: i64,
    data_source_id: i64,
) -> Result<Caller, ApiError> {
    let groups = crate::authz::permissions::load_effective_groups(db, user).await?;
    let is_admin = crate::authz::permissions::is_admin(&groups);
    query_caller_with_groups(db, user, &groups, is_admin, id, data_source_id).await
}

/// [`query_caller`] over already-loaded `groups`/`is_admin`; list responses use
/// [`query_callers_with_groups`] instead.
async fn query_caller_with_groups(
    db: &Db,
    user: &user::Model,
    groups: &[group::Model],
    is_admin: bool,
    id: i64,
    data_source_id: i64,
) -> Result<Caller, ApiError> {
    let edit = authorize::evaluate_with_groups(
        db,
        user,
        groups,
        is_admin,
        Action::Query {
            verb: QueryVerb::Edit,
            id,
        },
    )
    .await?;
    let capability_ok = authorize::evaluate_with_groups(
        db,
        user,
        groups,
        is_admin,
        Action::Query {
            verb: QueryVerb::Execute,
            id,
        },
    )
    .await?
    .can_edit;
    // Same execute-tier gate the run itself applies, else the Run button could be enabled for a
    // caller whose run 404s.
    let data_source_ok =
        query_service::require_data_source_execute_access(db, user, groups, data_source_id)
            .await
            .is_ok();
    Ok(Caller::from(edit).with_execute(capability_ok, data_source_ok))
}

/// Batched [`query_caller_with_groups`] for list responses: one [`authorize::prefetch_roles`] call
/// covers every row's checks and data-source reach is checked in two `IN (...)` queries, so cost is
/// fixed regardless of row count.
pub(crate) async fn query_callers_with_groups(
    db: &Db,
    user: &user::Model,
    groups: &[group::Model],
    is_admin: bool,
    rows: &[(i64, i64)],
) -> Result<std::collections::HashMap<i64, Caller>, ApiError> {
    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
    let roles = authorize::prefetch_roles(db, user, groups, is_admin, "query", &ids).await?;

    let ds_ids: std::collections::HashSet<i64> = rows.iter().map(|(_, ds_id)| *ds_id).collect();
    let (existing_ds, dsgs) = if ds_ids.is_empty() {
        (std::collections::HashSet::new(), Vec::new())
    } else {
        let existing: std::collections::HashSet<i64> =
            arcodash_core::entities::data_source::Entity::find()
                .filter(
                    arcodash_core::entities::data_source::Column::Id.is_in(ds_ids.iter().copied()),
                )
                .filter(arcodash_core::entities::data_source::Column::OrgId.eq(user.org_id))
                .all(db)
                .await?
                .into_iter()
                .map(|d| d.id)
                .collect();
        let dsgs = arcodash_core::entities::data_source_group::Entity::find()
            .filter(arcodash_core::entities::data_source_group::Column::DataSourceId.is_in(ds_ids))
            .all(db)
            .await?;
        (existing, dsgs)
    };

    let mut out = std::collections::HashMap::with_capacity(rows.len());
    for (id, ds_id) in rows {
        // The data source must exist in the caller's org and (absent admin) be reachable by an
        // execute-granting group.
        let data_source_ok = existing_ds.contains(ds_id)
            && (is_admin
                || crate::authz::data_source::any_group_grants_with_reach(
                    user,
                    groups,
                    "execute_query",
                    *ds_id,
                    &dsgs,
                ));
        let edit = authorize::evaluate_with_roles(
            db,
            user,
            groups,
            is_admin,
            &roles,
            Action::Query {
                verb: QueryVerb::Edit,
                id: *id,
            },
        )
        .await?;
        let capability_ok = authorize::evaluate_with_roles(
            db,
            user,
            groups,
            is_admin,
            &roles,
            Action::Query {
                verb: QueryVerb::Execute,
                id: *id,
            },
        )
        .await?
        .can_edit;
        out.insert(
            *id,
            Caller::from(edit).with_execute(capability_ok, data_source_ok),
        );
    }
    Ok(out)
}

/// The single-query response: `caller` block plus the owner's display name.
async fn query_json(
    db: &Db,
    user: &user::Model,
    q: query::Model,
) -> Result<Json<QueryResponse>, ApiError> {
    let mut resp = QueryResponse::from(q);
    resp.caller = query_caller(db, user, resp.id, resp.data_source_id).await?;
    resp.owner_name = user::Entity::find_by_id(resp.user_id)
        .one(db)
        .await?
        .map(|u| u.name);
    Ok(Json(resp))
}

/// Query JSON as returned to clients.
#[derive(Debug, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "response DTO; each bool is an independent flag in the JSON wire format"
)]
pub struct QueryResponse {
    pub id: i64,
    pub data_source_id: i64,
    pub name: String,
    pub description: Option<String>,
    pub query: String,
    pub query_hash: String,
    pub user_id: i64,
    pub visibility: String,
    pub latest_query_data_id: Option<i64>,
    pub is_archived: bool,
    /// `true` iff the query is live: published and not since taken back to draft.
    pub published: bool,
    /// `true` when `published` and the draft differs from the published columns.
    pub has_unpublished_changes: bool,
    /// Draft-row concurrency token; required as `expected_version` on
    /// `update`/`publish`/`restore`/`discard`.
    pub working_version: i32,
    /// `{parameters: [...], max_age?: ..., ...}` (see `QueryOptions`), returned as raw JSON so
    /// clients round-trip it unchanged.
    pub options: Value,
    pub tags: Vec<String>,
    pub is_favorite: bool,
    /// `"New"` / `"Success"` / `"Failed"`, set by the worker after each execution of this saved
    /// query.
    pub last_execution_status: String,
    pub last_execution_at: Option<DateTime<Utc>>,
    pub row_count: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Display name of the owning user; `None` if the user row is gone. Populated by handlers,
    /// never by `From<query::Model>`.
    pub owner_name: Option<String>,
    /// Server-computed authz summary ([`query_caller`]); every handler overwrites the no-access
    /// default.
    pub caller: Caller,
}

impl From<query::Model> for QueryResponse {
    fn from(q: query::Model) -> Self {
        let published = query_service::is_published(&q);
        let has_unpublished_changes = query_service::has_unpublished_changes(&q);
        let working_version = q.working_version;
        // Draft-first resolution is safe only because every caller has already overlaid published
        // content for non-editors; a new call site must redo the editorship check (see
        // `resolve_latest_query_data_id`).
        let latest_query_data_id = query_service::resolve_latest_query_data_id(&q);
        Self {
            id: q.id,
            data_source_id: q.data_source_id,
            name: q.name,
            description: q.description,
            query: q.query,
            query_hash: q.query_hash,
            user_id: q.user_id,
            visibility: q.visibility,
            latest_query_data_id,
            is_archived: q.is_archived,
            published,
            has_unpublished_changes,
            working_version,
            options: q.options,
            tags: q.tags.unwrap_or_default(),
            is_favorite: false,
            last_execution_status: q.last_execution_status,
            last_execution_at: q.last_execution_at.map(|t| t.with_timezone(&Utc)),
            row_count: q.row_count,
            created_at: q.created_at.with_timezone(&Utc),
            updated_at: q.updated_at.with_timezone(&Utc),
            owner_name: None,
            caller: Caller::default(),
        }
    }
}

/// Query result JSON as returned to clients; single-result endpoints nest it as `{"query_result":
/// {...}}` ([`QueryResultEnvelope`]).
#[derive(Debug, Serialize)]
pub struct QueryResultResponse {
    pub id: i64,
    pub data_source_id: i64,
    pub query: String,
    pub query_hash: String,
    pub data: Option<Value>,
    pub runtime: Option<f64>,
    pub retrieved_at: DateTime<Utc>,
}

impl From<query_result::Model> for QueryResultResponse {
    fn from(r: query_result::Model) -> Self {
        Self {
            id: r.id,
            data_source_id: r.data_source_id,
            query: r.query,
            query_hash: r.query_hash,
            data: r.data,
            runtime: r.runtime,
            retrieved_at: r.retrieved_at.with_timezone(&Utc),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct QueryResultEnvelope {
    pub query_result: QueryResultResponse,
}

/// Execute responses: a cache hit (`{"query_result": ...}`), an enqueued job (`{"job": {...,
/// "queued": true}}`) or a job parked behind the data source's concurrency cap (`queued: false`).
fn execute_outcome_response(outcome: query_service::ExecuteOutcome) -> impl IntoResponse {
    match outcome {
        query_service::ExecuteOutcome::Cached(result) => Json(serde_json::json!({
            "query_result": QueryResultResponse::from(*result),
        })),
        query_service::ExecuteOutcome::Enqueued { job_id } => queued_job(&job_id, true),
        query_service::ExecuteOutcome::Parked { job_id } => queued_job(&job_id, false),
    }
}

fn queued_job(job_id: &str, queued: bool) -> Json<Value> {
    Json(serde_json::json!({
        "job": { "id": job_id, "status": 1, "query_result_id": null, "error": null, "queued": queued },
    }))
}

/// `serde_urlencoded` can't collect repeated or bracketed keys into a `Vec<String>` (they silently
/// yield an empty vec or a 400), so tags use one comma-separated param.
fn deserialize_comma_separated<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<String> = Option::deserialize(deserializer)?;
    Ok(raw
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQueriesQuery {
    /// Include archived queries (excluded by default).
    #[serde(default)]
    pub include_archived: bool,
    /// Case-insensitive substring search against name, description, and query text.
    pub q: Option<String>,
    /// Tag filter: queries must contain ALL tags (`@>`); comma-separated, e.g. `?tags=foo,bar`.
    #[serde(default, deserialize_with = "deserialize_comma_separated")]
    pub tags: Vec<String>,
    /// `all` (default) | `by-me` | `shared-with-me` — narrows the list beyond plain visibility.
    pub filter: Option<String>,
}

/// `GET /api/queries` — scoped to the caller's data-source access. Supports `?include_archived=`,
/// `?q=`, `?tags=` and `?filter=all|by-me|shared-with-me`.
pub async fn list(
    db: Db,
    CurrentUser(user): CurrentUser,
    Query(params): Query<ListQueriesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    super::validate_list_filter(params.filter.as_deref())?;

    let queries = query_service::list_for_user(
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
        .filter(favorite::Column::ObjectType.eq("query"))
        .all(&db)
        .await?
        .into_iter()
        .map(|f| f.object_id)
        .collect();

    // Groups/admin status are per-user: load once for every row's `caller` block.
    let groups = crate::authz::permissions::load_effective_groups(&db, &user).await?;
    let is_admin = crate::authz::permissions::is_admin(&groups);

    let owner_ids: std::collections::HashSet<i64> = queries.iter().map(|q| q.user_id).collect();
    let owner_names: std::collections::HashMap<i64, String> = user::Entity::find()
        .filter(user::Column::Id.is_in(owner_ids))
        .all(&db)
        .await?
        .into_iter()
        .map(|u| (u.id, u.name))
        .collect();

    let rows: Vec<(i64, i64)> = queries.iter().map(|q| (q.id, q.data_source_id)).collect();
    let mut callers = query_callers_with_groups(&db, &user, &groups, is_admin, &rows).await?;

    let response: Vec<QueryResponse> = queries
        .into_iter()
        .map(|q| QueryResponse {
            is_favorite: fav_ids.contains(&q.id),
            owner_name: owner_names.get(&q.user_id).cloned(),
            caller: callers.remove(&q.id).unwrap_or_default(),
            ..QueryResponse::from(q)
        })
        .collect();

    Ok(Json(response))
}

/// `GET /api/queries/tags` — distinct tags across non-archived queries, org-wide (a tag name is not
/// object content).
pub async fn tags(db: Db, CurrentUser(user): CurrentUser) -> Result<impl IntoResponse, ApiError> {
    let tags = query_service::list_tags(&db, user.org_id).await?;
    Ok(Json(serde_json::json!({ "tags": tags })))
}

#[derive(Debug, Deserialize)]
pub struct CreateQueryRequest {
    pub data_source_id: i64,
    pub name: String,
    pub description: Option<String>,
    pub query: String,
    /// `{parameters: [...], ...}` — optional; omitted means "no parameter definitions yet".
    pub options: Option<Value>,
    /// Optional tags applied at creation. Omitted = no tags.
    pub tags: Option<Vec<String>>,
}

/// `POST /api/queries` — create.
pub async fn create(
    db: Db,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<CreateQueryRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let q = query_service::create(
        &db,
        user.org_id,
        &user,
        payload.data_source_id,
        payload.name,
        payload.description,
        payload.query,
        payload.options,
        payload.tags,
    )
    .await?;
    query_json(&db, &user, q).await
}

/// `GET /api/queries/<id>` — editors (and queries never published) see the draft columns; everyone
/// else, including capability-less Editor-grant recipients, sees the published columns.
pub async fn get(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let mut q = query_service::get_for_user(&db, user.org_id, &user, id).await?;
    let eval = authorize::evaluate(
        &db,
        &user,
        Action::Query {
            verb: QueryVerb::Edit,
            id,
        },
    )
    .await?;
    if !eval.can_edit {
        query_service::overlay_serving_content(&mut q);
    }
    let is_fav = favorite::Entity::find()
        .filter(favorite::Column::UserId.eq(user.id))
        .filter(favorite::Column::ObjectType.eq("query"))
        .filter(favorite::Column::ObjectId.eq(id))
        .one(&db)
        .await?
        .is_some();
    let Json(mut resp) = query_json(&db, &user, q).await?;
    resp.is_favorite = is_fav;
    Ok(Json(resp))
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateQueryRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub query: Option<String>,
    pub data_source_id: Option<i64>,
    /// Replaces the whole `options` JSON when present (parameter definitions, not just runtime
    /// values).
    pub options: Option<Value>,
    /// Archive (`Some(true)`) or unarchive (`Some(false)`); `None` leaves it untouched.
    pub is_archived: Option<bool>,
    /// Optimistic-concurrency token: must match the row's `working_version` or the update is
    /// rejected with 409.
    pub expected_version: Option<i32>,
    /// Replaces the whole tag array when present; `None` leaves tags untouched.
    pub tags: Option<Vec<String>>,
}

/// `POST /api/queries/<id>` — update (draft edit + archive/unarchive).
pub async fn update(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateQueryRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = query_service::get_for_user(&db, user.org_id, &user, id).await?;
    let updated = query_service::update(
        &db,
        &user,
        existing,
        payload.name,
        payload.description,
        payload.query,
        payload.data_source_id,
        payload.options,
        payload.is_archived,
        payload.expected_version,
        payload.tags,
    )
    .await?;
    query_json(&db, &user, updated).await
}

#[derive(Debug, Deserialize)]
pub struct LifecycleRequest {
    pub expected_version: i32,
}

/// `POST /api/queries/<id>/publish` — publish the current draft. Requires Editor-or-above.
pub async fn publish(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<LifecycleRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = query_service::get_for_user(&db, user.org_id, &user, id).await?;
    let updated = query_service::publish(&db, &user, existing, payload.expected_version).await?;
    query_json(&db, &user, updated).await
}

/// `POST /api/queries/<id>/unpublish` — return a live query to draft. Requires Editor-or-above.
pub async fn unpublish(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<LifecycleRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = query_service::get_for_user(&db, user.org_id, &user, id).await?;
    let updated = query_service::unpublish(&db, &user, existing, payload.expected_version).await?;
    query_json(&db, &user, updated).await
}

#[derive(Debug, Deserialize)]
pub struct RestoreQueryRequest {
    pub change_id: i64,
    pub expected_version: i32,
}

/// `POST /api/queries/<id>/restore` — promote a past publish point to the published version and
/// reset the draft. Requires Editor-or-above.
pub async fn restore(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<RestoreQueryRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = query_service::get_for_user(&db, user.org_id, &user, id).await?;
    let updated = query_service::restore(
        &db,
        &user,
        existing,
        payload.change_id,
        payload.expected_version,
    )
    .await?;
    query_json(&db, &user, updated).await
}

/// `POST /api/queries/<id>/discard` — reset the draft columns back to the published version.
pub async fn discard(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<LifecycleRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = query_service::get_for_user(&db, user.org_id, &user, id).await?;
    let updated =
        query_service::discard_changes(&db, &user, existing, payload.expected_version).await?;
    query_json(&db, &user, updated).await
}

/// `POST /api/queries/<id>/refresh` — force-enqueue now, bypassing schedule. Runs the served
/// content (published once published), not a pending edit.
pub async fn refresh(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let q = query_service::get_for_user(&db, user.org_id, &user, id).await?;
    let resolved = query_service::resolve_serving_content(&q);
    let outcome = query_service::execute(
        &db,
        state.queue.as_ref(),
        state.conveyor.as_deref(),
        state.result_reader(),
        &state.redis,
        user.org_id,
        &user,
        resolved.data_source_id,
        resolved.query,
        resolved.options,
        Value::Null,
        Some(0),
        Some(q.id),
        None,
        resolved.repoint_published,
        state.config.max_query_rows,
    )
    .await?;
    Ok(execute_outcome_response(outcome))
}

/// `POST /api/queries/:id/run` — manual refresh through the runs pipeline (inserts a `runs` row for
/// history and CSV download). Execute-tier reach, so a Viewer may trigger it.
pub async fn run(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    authorize::authorize(
        &db,
        &user,
        Action::Query {
            verb: QueryVerb::Execute,
            id,
        },
    )
    .await?;
    let q = query_service::get_for_user(&db, user.org_id, &user, id).await?;
    let resolved = query_service::resolve_serving_content(&q);
    if !crate::services::parameters::referenced_names(&resolved.query).is_empty() {
        return Err(ApiError::bad_request(
            "this query has parameters; run it from the query editor (which supplies parameter \
             values) — the refresh endpoint does not support parameters",
        ));
    }
    query_service::ensure_run_data_source_access(&db, &user, resolved.data_source_id, id).await?;
    let (run, already) = run_service::run_now(
        &db,
        state.queue.clone(),
        state.conveyor.clone(),
        state.redis.clone(),
        state.conveyor_db.clone(),
        user.org_id,
        user.id,
        "query",
        id,
        Some(resolved.data_source_id),
    )
    .await?;
    let code = if already {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((code, Json(RunResponse::from_model(run))))
}

/// `GET /api/queries/:id/consumers` — impact counts (tiles/dashboards/monitors depending on this
/// query), for a "used in N places" line before a risky edit.
pub async fn consumers(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    // Same gate as every other read (`get_for_user` adds the data-source axis), so a caller who
    // 404s on the query can't enumerate its consumers.
    query_service::get_for_user(&db, user.org_id, &user, id).await?;
    let c = query_service::consumers(&db, user.org_id, id).await?;
    Ok(Json(c))
}

/// `DELETE /api/queries/<id>`.
pub async fn delete(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = query_service::get_for_user(&db, user.org_id, &user, id).await?;
    query_service::delete(&db, &user, existing).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// One `changes` row for `GET /api/queries/<id>/version`, annotated with its 1-indexed version
/// number (computed at read time).
#[derive(Debug, Serialize)]
pub struct VersionResponse {
    pub version: i64,
    pub change_id: i64,
    pub user_id: i64,
    /// Author display name; `None` if the user was deleted.
    pub user_name: Option<String>,
    pub change: Value,
    pub created_at: DateTime<Utc>,
}

impl From<change_service::VersionedChange> for VersionResponse {
    fn from(v: change_service::VersionedChange) -> Self {
        Self {
            version: v.version,
            change_id: v.change.id,
            user_id: v.change.user_id,
            user_name: None,
            change: v.change.change,
            created_at: v.change.created_at.with_timezone(&Utc),
        }
    }
}

/// `GET /api/queries/<id>/version` — full version history, oldest first, with version number,
/// snapshot, author and timestamp. Read access.
pub async fn versions(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = query_service::get_for_user(&db, user.org_id, &user, id).await?;
    let versions = query_service::versions(&db, &existing).await?;
    let user_names = display_names::resolve_user_names(
        &db,
        user.org_id,
        versions.iter().map(|v| v.change.user_id),
    )
    .await?;
    let response: Vec<VersionResponse> = versions
        .into_iter()
        .map(|v| {
            let mut r = VersionResponse::from(v);
            r.user_name = user_names.get(&r.user_id).cloned();
            r
        })
        .collect();
    Ok(Json(response))
}

#[derive(Debug, Deserialize)]
pub struct RevertQueryRequest {
    /// Supply exactly one of `change_id` (the `changes.id`) or `version` (the 1-indexed number
    /// shown in the UI); both is a 400.
    pub change_id: Option<i64>,
    pub version: Option<i64>,
    /// Optimistic-concurrency token, as in [`UpdateQueryRequest::expected_version`]; mismatch is a
    /// 409.
    pub expected_version: Option<i32>,
}

/// `POST /api/queries/<id>/revert` — apply a historical snapshot onto the live row. Requires
/// `edit_query`; records a new `changes` entry.
pub async fn revert(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<RevertQueryRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = query_service::get_for_user(&db, user.org_id, &user, id).await?;

    let target = match (payload.change_id, payload.version) {
        (Some(change_id), None) => RevertTarget::ChangeId(change_id),
        (None, Some(version)) => RevertTarget::Version(version),
        (None, None) => {
            return Err(ApiError::bad_request(
                "either \"change_id\" or \"version\" is required",
            ))
        }
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request(
                "supply only one of \"change_id\" or \"version\", not both",
            ))
        }
    };

    let updated =
        query_service::revert(&db, &user, existing, target, payload.expected_version).await?;
    query_json(&db, &user, updated).await
}

/// `POST /api/queries/<id>/fork` — copy the source's published content (draft if the caller can
/// edit it) into a new draft `"Copy of <name>"`. Requires `create_query` plus Viewer reach on the
/// source.
pub async fn fork(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let source = query_service::get_for_user(&db, user.org_id, &user, id).await?;
    let forked = query_service::fork(&db, user.org_id, &user, &source).await?;
    query_json(&db, &user, forked).await
}

#[derive(Debug, Deserialize, Default)]
pub struct ExecuteQueryRequest {
    #[serde(default)]
    pub parameters: Value,
    pub max_age: Option<i64>,
}

/// `true` for `Value::Null` or an empty object, both meaning "no parameter values supplied".
fn parameters_are_empty(parameters: &Value) -> bool {
    match parameters {
        Value::Null => true,
        Value::Object(m) => m.is_empty(),
        _ => false,
    }
}

/// `POST /api/queries/<id>/results` — execute the saved query (cache-or-enqueue) with runtime
/// `parameters` against the served content (see [`refresh`]), never the caller's pending edit.
pub async fn execute_saved(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    body: Option<Json<ExecuteQueryRequest>>,
) -> Result<impl IntoResponse, ApiError> {
    let q = query_service::get_for_user(&db, user.org_id, &user, id).await?;
    let resolved = query_service::resolve_serving_content(&q);
    let payload = body.map(|Json(b)| b).unwrap_or_default();

    // The published pointer tracks the canonical (unparameterized) run, so only a run with no
    // caller-supplied values may repoint it.
    let repoint_published = resolved.repoint_published && parameters_are_empty(&payload.parameters);

    let outcome = query_service::execute(
        &db,
        state.queue.as_ref(),
        state.conveyor.as_deref(),
        state.result_reader(),
        &state.redis,
        user.org_id,
        &user,
        resolved.data_source_id,
        resolved.query,
        resolved.options,
        payload.parameters,
        payload.max_age,
        Some(q.id),
        None,
        repoint_published,
        state.config.max_query_rows,
    )
    .await?;

    Ok(execute_outcome_response(outcome))
}

#[derive(Debug, Deserialize, Default)]
pub struct LatestResultQuery {
    /// Dashboard/viewer surfaces want the published pointer even for editors, so a widget never
    /// shows an editor's scratch run.
    #[serde(default)]
    pub published_only: bool,
}

/// `GET /api/queries/<id>/results` — latest result without executing.
///
/// Reads the pointer directly (`query_hash` differs between unrendered and rendered SQL). Editors
/// get the draft-first pointer; non-editors only the published one, so they never see rows from a
/// draft run alongside the published SQL. `published_only=true` opts an editor out of draft-first
/// (dashboard widgets).
pub async fn latest_result(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Query(params): Query<LatestResultQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let q = query_service::get_for_user(&db, user.org_id, &user, id).await?;
    let eval = authorize::evaluate(
        &db,
        &user,
        Action::Query {
            verb: QueryVerb::Edit,
            id,
        },
    )
    .await?;
    let result_id = if eval.can_edit && !params.published_only {
        query_service::resolve_latest_query_data_id(&q)
    } else {
        query_service::resolve_serving_content(&q).latest_query_data_id
    }
    .ok_or_else(|| ApiError::not_found("no results yet for this query"))?;
    let result =
        result_service::get_with_data(&db, state.result_reader(), user.org_id, &user, result_id)
            .await?;

    Ok(Json(QueryResultEnvelope {
        query_result: QueryResultResponse::from(result),
    }))
}

/// Ad-hoc execute has no saved row, so callers holding parameter definitions client-side pass them
/// inline via `options` (same shape as `options.parameters`).
///
/// If omitted, `{{ }}` tokens fail with a "no parameter named ..." render error.
#[derive(Debug, Deserialize)]
pub struct AdHocExecuteRequest {
    pub data_source_id: i64,
    pub query: String,
    #[serde(default)]
    pub parameters: Value,
    #[serde(default)]
    pub options: Value,
    pub max_age: Option<i64>,
    /// Associates the run with a saved query: friendlier ACL errors, and if
    /// `query`/`data_source_id` match the row's draft, binds execution state to it (see
    /// `query_service::execute`). Diverging content stays unbound.
    pub query_id: Option<i64>,
}

/// `POST /api/query_results` — ad-hoc execute, not tied to a saved query; parameter definitions
/// come from the body's `options`. Binding to `query_id` is decided by `query_service::execute`.
pub async fn execute_ad_hoc(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<AdHocExecuteRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let outcome = query_service::execute(
        &db,
        state.queue.as_ref(),
        state.conveyor.as_deref(),
        state.result_reader(),
        &state.redis,
        user.org_id,
        &user,
        payload.data_source_id,
        payload.query,
        payload.options,
        payload.parameters,
        payload.max_age,
        None,
        payload.query_id,
        false,
        state.config.max_query_rows,
    )
    .await?;

    Ok(execute_outcome_response(outcome))
}

#[derive(Debug, Deserialize)]
pub struct ParameterValuesQuery {
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct ParameterValueOptionResponse {
    pub name: String,
    pub value: Value,
}

/// `GET /api/queries/<id>/parameter_values?name=<param_name>` — option list for a Query-based
/// Dropdown parameter: `{"values": [...]}` once the source query has a cached result, else `{"job":
/// {...}}` to poll.
pub async fn parameter_values(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<ParameterValuesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let outcome = query_service::parameter_values(
        &db,
        state.queue.as_ref(),
        state.conveyor.as_deref(),
        state.result_reader(),
        &state.redis,
        user.org_id,
        &user,
        id,
        &q.name,
        state.config.max_query_rows,
    )
    .await?;

    Ok(match outcome {
        query_service::ParameterValuesOutcome::Options(options) => Json(serde_json::json!({
            "values": options
                .into_iter()
                .map(|o| ParameterValueOptionResponse { name: o.name, value: o.value })
                .collect::<Vec<_>>(),
        })),
        query_service::ParameterValuesOutcome::Enqueued { job_id } => Json(serde_json::json!({
            "job": JobResponse {
                id: job_id,
                status: 1,
                query_result_id: None,
                error: None,
            },
        })),
    })
}

/// `GET /api/queries/<id>/visualizations` — list all visualizations for a query.
pub async fn list_visualizations(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    query_service::get_for_user(&db, user.org_id, &user, id).await?;
    let vizs = visualization_service::list_for_query(&db, id).await?;
    let response: Vec<VisualizationResponse> = vizs.into_iter().map(Into::into).collect();
    Ok(Json(response))
}

/// `GET /api/query_results/{id}` and `.{format}` — unified handler: a path param containing `.` is
/// a download (csv/tsv/xlsx/json), else the JSON result.
///
/// Axum 0.8 forbids two params in one segment. Only the download branch is rate-limited
/// (`download_limiter`): `route_layer` can't gate half a merged route, and the JSON branch is hit
/// by every job-backed widget.
pub async fn get_or_download_result(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id_str): Path<String>,
    Extension(download_limiter): Extension<Arc<UserKeyedLimiter>>,
) -> Result<Response, ApiError> {
    let parse_id = |raw: &str| {
        raw.parse::<i64>()
            .map_err(|_| ApiError::bad_request(format!("invalid query result id: {raw}")))
    };
    if let Some((raw_id, format_ext)) = id_str.split_once('.') {
        if download_limiter.check_key(&user.id).is_err() {
            return Err(ApiError::too_many_requests(
                "too many requests, please try again later",
            ));
        }
        let id = parse_id(raw_id)?;
        let result =
            result_service::get_with_data(&db, state.result_reader(), user.org_id, &user, id)
                .await?;
        let fmt = match format_ext {
            "csv" => ResultFormat::Csv,
            "tsv" => ResultFormat::Tsv,
            "xlsx" => ResultFormat::Xlsx,
            "json" => ResultFormat::Json,
            _ => {
                return Err(ApiError::bad_request(format!(
                    "unsupported format: {format_ext}"
                )))
            }
        };
        let data = result
            .data
            .ok_or_else(|| ApiError::not_found("result has no data"))?;
        // Computed before the move below: cloning a 10k-row `Value` would cost more than it saves.
        let row_count = data
            .get("rows")
            .and_then(Value::as_array)
            .map_or(0, std::vec::Vec::len);
        let (bytes, content_type) = result_service::format_result_blocking(data, fmt).await?;
        record_event(
            &db,
            user.org_id,
            Some(user.id),
            "query.result_downloaded",
            Some("query"),
            Some(id),
            serde_json::json!({ "format": format_ext, "result_id": id, "row_count": row_count }),
        )
        .await?;
        let disposition = format!("attachment; filename=\"query_result_{id}.{format_ext}\"");
        return Response::builder()
            .header(CONTENT_TYPE, content_type)
            .header(CONTENT_DISPOSITION, disposition)
            .body(axum::body::Body::from(bytes))
            .map_err(|e| ApiError::internal(format!("response build: {e}")));
    }

    let id = parse_id(&id_str)?;
    let result =
        result_service::get_with_data(&db, state.result_reader(), user.org_id, &user, id).await?;
    Ok(Json(QueryResultEnvelope {
        query_result: QueryResultResponse::from(result),
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_tags(json: serde_json::Value) -> Vec<String> {
        #[derive(Deserialize)]
        struct W {
            #[serde(default, deserialize_with = "deserialize_comma_separated")]
            tags: Vec<String>,
        }
        serde_json::from_value::<W>(json).unwrap().tags
    }

    #[test]
    fn comma_separated_tags_splits_trims_and_drops_empties() {
        assert_eq!(
            parse_tags(serde_json::json!({"tags": "foo, bar ,,baz"})),
            vec!["foo", "bar", "baz"]
        );
    }

    #[test]
    fn comma_separated_tags_defaults_to_empty_when_the_field_is_absent() {
        assert_eq!(parse_tags(serde_json::json!({})), Vec::<String>::new());
    }

    #[test]
    fn comma_separated_tags_handles_a_single_value_with_no_comma() {
        assert_eq!(
            parse_tags(serde_json::json!({"tags": "solo"})),
            vec!["solo"]
        );
    }
}
