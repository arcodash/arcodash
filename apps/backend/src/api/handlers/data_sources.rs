//! `/api/data_sources*` handlers.
//!
//! Create needs a group that grants `create_data_source` and is unrestricted;
//! edit/delete/test/pause/resume/schema-refresh need `edit_data_source` plus reach (via
//! `authz::authorize::authorize`); reads are scoped via `services::data_source_service`.
//!
//! Responses never include `encrypted_options` or decrypted secrets.

use arcodash_core::org_storage::{StorageArea, StorageError};
use axum::extract::{Multipart, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use arcodash_core::entities::{data_source, data_source_group, data_source_upload, group, user};
use arcodash_core::events::record_event;
use arcodash_core::storage;

use super::encryption_key;
use crate::auth::CurrentUser;
use crate::authz::authorize::{authorize, evaluate, Action, DataSourceVerb};
use crate::authz::data_source::any_unrestricted_group_grants;
use crate::authz::permissions::require_permission;
use crate::db::Db;
use crate::error::ApiError;
use crate::extract::{Json, Path};
use crate::services::upload_service;
use crate::services::{data_source_service, object_cleanup};
use crate::state::AppState;

/// Server-side copy for the create block: no group both grants `create_data_source` and is
/// unrestricted.
const DATA_SOURCE_CREATE_BLOCKED: &str = "You can't add a data source right now because your \
    access is limited to specific data sources. Ask a workspace admin to widen your access.";

/// Data source JSON for clients; deliberately has no field for `encrypted_options`/raw secrets.
#[derive(Debug, Serialize)]
pub struct DataSourceResponse {
    pub id: i64,
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub schema: Option<Value>,
    pub schema_refreshed_at: Option<DateTime<Utc>>,
    /// Human-readable connector name (e.g. `"PostgreSQL"` for `"pg"`), rendered instead of the raw
    /// `type` key.
    pub type_display_name: String,
    pub paused: bool,
    pub pause_reason: Option<String>,
    /// Independent of `paused`/`pause_reason`: `"unknown"`, `"ok"` or `"failing"`.
    pub health_state: String,
    pub health_error: Option<String>,
    pub health_checked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Non-secret options; only populated on GET /:id for admins (for the edit form).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
    /// Server-computed authz summary (see [`ds_caller`]); defaults to no-access and every handler
    /// overwrites it.
    pub caller: crate::authz::authorize::Caller,
    /// A profile build is pending (set by `list`; false elsewhere).
    pub profiling: bool,
    /// The last profile build recorded an error.
    pub profile_failed: bool,
}

impl From<data_source::Model> for DataSourceResponse {
    fn from(ds: data_source::Model) -> Self {
        // query_results' schema depends on the asking user (see
        // `data_source_service::live_schema_for_query_results`), so the shared `schema` column must
        // not be served or leak across users.
        let schema = if ds.r#type == "query_results" {
            None
        } else {
            crate::services::data_source_service::normalized_schema_json(ds.schema)
        };
        Self {
            id: ds.id,
            name: ds.name,
            type_display_name: display_name(&ds.r#type).to_string(),
            type_name: ds.r#type,
            schema,
            schema_refreshed_at: ds.schema_refreshed_at.map(|t| t.with_timezone(&Utc)),
            paused: ds.paused_at.is_some(),
            pause_reason: ds.pause_reason,
            health_state: ds.health_state,
            health_error: ds.health_error,
            health_checked_at: ds.health_checked_at.map(|t| t.with_timezone(&Utc)),
            created_at: ds.created_at.with_timezone(&Utc),
            updated_at: ds.updated_at.with_timezone(&Utc),
            options: None,
            caller: crate::authz::authorize::Caller::default(),
            profiling: false,
            profile_failed: ds.ai_profile_last_error.is_some(),
        }
    }
}

/// The `caller` block for data source `id`, via the Edit-tier `evaluate(...)`; `can_share` stays
/// unset.
async fn ds_caller(
    db: &Db,
    user: &user::Model,
    id: i64,
) -> Result<crate::authz::authorize::Caller, ApiError> {
    let eval = evaluate(
        db,
        user,
        Action::DataSource {
            verb: DataSourceVerb::Edit,
            id,
        },
    )
    .await?;
    Ok(eval.into())
}

/// The single-source response with its `caller` block.
async fn ds_json(
    db: &Db,
    user: &user::Model,
    ds: data_source::Model,
) -> Result<Json<DataSourceResponse>, ApiError> {
    let caller = ds_caller(db, user, ds.id).await?;
    Ok(Json(DataSourceResponse {
        caller,
        ..DataSourceResponse::from(ds)
    }))
}

/// [`find_in_org`] followed by the `verb` gate.
async fn find_authorized(
    db: &Db,
    user: &user::Model,
    id: i64,
    verb: DataSourceVerb,
) -> Result<data_source::Model, ApiError> {
    let ds = find_in_org(db, user.org_id, id).await?;
    authorize(db, user, Action::DataSource { verb, id }).await?;
    Ok(ds)
}

/// `GET /api/data_sources` envelope: the list plus a server-computed `can_create` signal.
#[derive(Debug, Serialize)]
pub struct ListDataSourcesResponse {
    pub data_sources: Vec<DataSourceResponse>,
    pub can_create: bool,
}

/// `GET /api/data_sources` — list scoped to the current user's access.
pub async fn list(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
) -> Result<impl IntoResponse, ApiError> {
    let data_sources = data_source_service::list_for_user(&db, user.org_id, &user).await?;
    // Groups/admin status are per-user: load once and reuse across every row's `caller` block.
    let groups = crate::authz::permissions::load_effective_groups(&db, &user).await?;
    let is_admin = crate::authz::permissions::is_admin(&groups);
    let can_create =
        is_admin || any_unrestricted_group_grants(&user, &groups, "create_data_source");

    let pending: Vec<Option<String>> = if data_sources.is_empty() {
        Vec::new()
    } else {
        let mut cmd = redis::cmd("MGET");
        for ds in &data_sources {
            cmd.arg(arcodash_core::models::ai_profile_pending_key(ds.id));
        }
        cmd.query_async(&mut state.redis.clone())
            .await
            .unwrap_or_default()
    };

    let mut data_sources_resp: Vec<DataSourceResponse> = Vec::with_capacity(data_sources.len());
    for (i, ds) in data_sources.into_iter().enumerate() {
        let mut r = DataSourceResponse::from(ds);
        r.profiling = pending.get(i).is_some_and(Option::is_some);
        let eval = crate::authz::authorize::evaluate_with_groups(
            &db,
            &user,
            &groups,
            is_admin,
            Action::DataSource {
                verb: DataSourceVerb::Edit,
                id: r.id,
            },
        )
        .await?;
        r.caller = eval.into();
        data_sources_resp.push(r);
    }
    Ok(Json(ListDataSourcesResponse {
        data_sources: data_sources_resp,
        can_create,
    }))
}

#[derive(Debug, Deserialize)]
pub struct CreateDataSourceRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub options: Value,
}

/// `POST /api/data_sources`: requires `create_data_source` AND an unrestricted group granting it,
/// since the new source needs an unrestricted group to live on.
pub async fn create(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<CreateDataSourceRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let groups = crate::authz::permissions::load_effective_groups(&db, &user).await?;
    let is_admin = crate::authz::permissions::is_admin(&groups);
    if !is_admin {
        if !crate::authz::permissions::user_has_permission(&user, &groups, "create_data_source") {
            return Err(ApiError::forbidden(
                "missing the create_data_source permission",
            ));
        }
        if !any_unrestricted_group_grants(&user, &groups, "create_data_source") {
            return Err(ApiError::forbidden(DATA_SOURCE_CREATE_BLOCKED));
        }
    }
    let key = encryption_key(&state)?;

    let name = payload.name.clone();

    // Verify a referenced upload's org ownership BEFORE the insert: the request transaction commits
    // regardless of response status.
    let upload_id = data_source_service::verify_upload_ownership(
        &db,
        user.org_id,
        &payload.options,
        &payload.type_name,
    )
    .await?;

    let ds = data_source_service::create(
        &db,
        &state.connectors,
        &key,
        user.org_id,
        payload.name,
        payload.type_name,
        payload.options,
    )
    .await?;
    if let Some(upload_id) = upload_id {
        data_source_service::link_upload(&db, upload_id, ds.id).await?;
    }

    record_event(
        &db,
        user.org_id,
        Some(user.id),
        "data_source.created",
        Some("data_source"),
        Some(ds.id),
        json!({ "name": name }),
    )
    .await?;

    ds_json(&db, &user, ds).await
}

/// `GET /api/data_sources/<id>` — access-checked.
pub async fn get(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let ds = data_source_service::get_for_user(&db, user.org_id, &user, id).await?;
    let caller = ds_caller(&db, &user, id).await?;
    let options = if caller.can_edit {
        let key = encryption_key(&state)?;
        let mut options =
            data_source_service::options_without_secrets(&state.connectors, &key, &ds)?;
        enrich_with_upload_filename(&db, &ds.r#type, &mut options).await;
        Some(options)
    } else {
        None
    };
    Ok(Json(DataSourceResponse {
        options,
        caller,
        ..DataSourceResponse::from(ds)
    }))
}

/// For `csv`/`sqlite`, adds a display-only `upload_filename` next to `upload_id`. Best-effort: a
/// missing or bad `upload_id` leaves `options` untouched. `strip_to_schema` drops it on the next
/// save.
async fn enrich_with_upload_filename(db: &Db, type_name: &str, options: &mut Value) {
    if type_name != "csv" && type_name != "sqlite" {
        return;
    }
    let Some(upload_id) = options.get("upload_id").and_then(Value::as_str) else {
        return;
    };
    let Ok(uuid) = Uuid::parse_str(upload_id) else {
        return;
    };
    if let Ok(Some(row)) = data_source_upload::Entity::find_by_id(uuid).one(db).await {
        if let Some(obj) = options.as_object_mut() {
            obj.insert("upload_filename".to_string(), json!(row.filename));
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateDataSourceRequest {
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub options: Option<Value>,
}

/// `POST /api/data_sources/<id>` — update, requires `edit_data_source`.
pub async fn update(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateDataSourceRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = find_authorized(&db, &user, id, DataSourceVerb::Edit).await?;
    let key = encryption_key(&state)?;

    // Resolve what `update()` will persist and verify any referenced upload's org ownership BEFORE
    // it runs, so a cross-org `upload_id` is rejected before the row is written. `plan_update` is
    // pure, so recomputing is cheap.
    let (final_type, merged_options) = data_source_service::plan_update(
        &state.connectors,
        &key,
        &existing,
        payload.type_name.as_deref(),
        payload.options.as_ref(),
    )?;
    let upload_id = match &merged_options {
        Some(merged) => {
            data_source_service::verify_upload_ownership(&db, user.org_id, merged, &final_type)
                .await?
        }
        None => None,
    };

    let updated = data_source_service::update(
        &db,
        &state.connectors,
        &key,
        existing,
        payload.name,
        payload.type_name,
        payload.options,
    )
    .await?;

    if let Some(upload_id) = upload_id {
        data_source_service::link_upload(&db, upload_id, updated.id).await?;
    }

    record_event(
        &db,
        user.org_id,
        Some(user.id),
        "data_source.updated",
        Some("data_source"),
        Some(updated.id),
        json!({ "name": updated.name }),
    )
    .await?;

    ds_json(&db, &user, updated).await
}

/// `DELETE /api/data_sources/<id>` — requires `edit_data_source` (edit implies delete).
pub async fn delete(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = find_authorized(&db, &user, id, DataSourceVerb::Delete).await?;
    let (ds_id, name, org_id) = (existing.id, existing.name.clone(), existing.org_id);
    let upload_keys = object_cleanup::upload_keys(&db, ds_id).await?;
    data_source_service::delete(&db, existing).await?;
    db.after_commit(Box::pin(object_cleanup::run(
        state.storage.clone(),
        state.db.clone(),
        org_id,
        object_cleanup::DeletedObject::DataSource {
            id: ds_id,
            upload_keys,
        },
    )))
    .await;

    record_event(
        &db,
        user.org_id,
        Some(user.id),
        "data_source.deleted",
        Some("data_source"),
        Some(ds_id),
        json!({ "name": name }),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct TestDataSourceRequest {
    /// Inline-options form: test a not-yet-saved configuration.
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub options: Option<Value>,
    /// By-id form: test an already-saved data source's stored (decrypted) options.
    pub id: Option<i64>,
}

/// `POST /api/data_sources/test`: inline `{ type, options }` or `{ id }`.
pub async fn test(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<TestDataSourceRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(id) = payload.id {
        let ds = find_authorized(&db, &user, id, DataSourceVerb::Test).await?;
        let key = encryption_key(&state)?;
        data_source_service::test_by_id(&db, &state.connectors, &key, &ds, user.id).await?;
        return Ok(Json(json!({ "message": "Connection successful" })));
    }

    // Inline form: no saved row to check reach against, so capability-only like a create.
    require_permission(&db, &user, "edit_data_source").await?;
    let (Some(type_name), Some(options)) = (payload.type_name, payload.options) else {
        return Err(ApiError::bad_request(
            "either 'id' or both 'type' and 'options' are required",
        ));
    };

    data_source_service::test(
        &state.connectors,
        &type_name,
        user.org_id,
        user.id,
        &options,
    )
    .await?;
    Ok(Json(json!({ "message": "Connection successful" })))
}

#[derive(Debug, Deserialize, Default)]
pub struct SchemaQuery {
    #[serde(default)]
    pub refresh: bool,
}

/// `GET /api/data_sources/<id>/schema`: the cached schema plus `refreshing`/`refresh_error`.
///
/// Never calls a connector; `?refresh=true` or an empty cache queues a worker profile job. An
/// explicit refresh by a caller lacking `edit_data_source` reach is denied; an implicit one on a
/// cache miss is skipped so a Viewer can still read the cache. `query_results` is exempt: its table
/// set depends on the caller's access, so it is always computed live
/// ([`data_source_service::live_schema_for_query_results`]).
pub async fn schema(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<SchemaQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let ds = data_source_service::get_for_user(&db, user.org_id, &user, id).await?;

    if ds.r#type == "query_results" {
        let key = encryption_key(&state)?;
        let schema = data_source_service::live_schema_for_query_results(
            &state.connectors,
            &key,
            &ds,
            user.id,
        )
        .await?;
        return Ok(Json(json!({
            "schema": serde_json::to_value(&schema)
                .map_err(|_| ApiError::internal("failed to serialize query_results schema"))?,
            "refreshing": false,
            "refresh_error": null,
        })));
    }

    if q.refresh {
        authorize(
            &db,
            &user,
            Action::DataSource {
                verb: DataSourceVerb::RefreshSchema,
                id,
            },
        )
        .await?;
    }
    // Probing whether the caller may refresh, not gating an action: `evaluate` (unlike `authorize`)
    // records no authz denial metric.
    let should_refresh = q.refresh
        || (ds.schema.is_none()
            && evaluate(
                &db,
                &user,
                Action::DataSource {
                    verb: DataSourceVerb::RefreshSchema,
                    id,
                },
            )
            .await?
            .can_edit);

    if should_refresh {
        let enqueued = data_source_service::enqueue_profile(
            &state,
            ds.org_id,
            id,
            arcodash_core::models::ProfileData::Changed,
        )
        .await;
        if q.refresh {
            enqueued?;
        }
    }

    let refreshing = arcodash_core::models::ai_profile_phase(&mut state.redis.clone(), id).await
        == Some("schema");
    let refresh_error = ds
        .ai_profile_last_error
        .as_deref()
        .and_then(|e| e.strip_prefix("schema refresh failed: "));

    Ok(Json(json!({
        "schema": data_source_service::normalized_schema_json(ds.schema)
            .unwrap_or(json!({ "tables": [] })),
        "refreshing": refreshing,
        "refresh_error": refresh_error,
    })))
}

/// Sample values are customer data, so the profile is admin-only.
async fn require_admin(db: &Db, user: &user::Model) -> Result<(), ApiError> {
    let groups = crate::authz::permissions::load_effective_groups(db, user).await?;
    if crate::authz::permissions::is_admin(&groups) {
        Ok(())
    } else {
        Err(ApiError::forbidden("admin access required"))
    }
}

/// `GET /api/data_sources/<id>/ai_profile`.
///
/// `profile` is `null` until first built; `supported`: type can be sampled; `building`: a build is
/// pending in Redis; `current`: profile covers every cached table and column with no failed
/// samples.
pub async fn ai_profile(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&db, &user).await?;
    let ds = data_source_service::get_for_user(&db, user.org_id, &user, id).await?;
    let current = super::ai::context::profile_is_complete(&ds);
    let supported = state.connectors.get(&ds.r#type).is_some()
        && arcodash_core::connectors::sample_table_sql(&ds.r#type, "t", 1).is_some();
    let phase = arcodash_core::models::ai_profile_phase(&mut state.redis.clone(), id).await;
    let storage = state.storage.resolve(&state.db, user.org_id).await?;
    let profile = match arcodash_core::ai::profile_values::load_ai_profile(&storage, &ds).await {
        Ok(p) => p,
        Err(e) => {
            arcodash_core::ai::narrative::report_unreachable(
                &*state.storage,
                &state.db,
                user.org_id,
                &e,
            )
            .await;
            return Err(e.into());
        }
    };
    let last_error = ds.ai_profile_last_error;
    let ds_type = ds.r#type;
    Ok(Json(json!({
        "profile": profile,
        "built_at": ds.ai_profile_built_at.map(|t| t.with_timezone(&Utc)),
        "supported": supported,
        "building": phase.is_some(),
        "current": current,
        "phase": phase,
        "last_error": last_error,
        "metered": arcodash_core::models::is_metered(&ds_type),
    })))
}

/// `POST /api/data_sources/<id>/ai_profile/rebuild` — 202 when queued, 409 if a build is pending.
pub async fn rebuild_ai_profile(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&db, &user).await?;
    data_source_service::get_for_user(&db, user.org_id, &user, id).await?;
    let queued = data_source_service::enqueue_profile(
        &state,
        user.org_id,
        id,
        arcodash_core::models::ProfileData::All,
    )
    .await?;
    let (status, body) = if queued {
        (StatusCode::ACCEPTED, "queued")
    } else {
        (StatusCode::CONFLICT, "already_queued")
    };
    Ok((status, Json(json!({ "status": body }))))
}

#[derive(Debug, Deserialize, Default)]
pub struct PauseRequest {
    pub reason: Option<String>,
}

/// `POST /api/data_sources/<id>/pause` — requires `edit_data_source`, optional `{ "reason": "..."
/// }`.
pub async fn pause(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    body: Option<Json<PauseRequest>>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = find_authorized(&db, &user, id, DataSourceVerb::Pause).await?;
    // Operator-supplied but unbounded on the wire; same bound as `user.login_failed`'s
    // `attempted_email`.
    let bounded_reason = body
        .and_then(|Json(b)| b.reason)
        .map(|r| r.chars().take(500).collect::<String>());
    let updated = data_source_service::pause(&db, existing, bounded_reason.clone()).await?;

    record_event(
        &db,
        user.org_id,
        Some(user.id),
        "data_source.paused",
        Some("data_source"),
        Some(id),
        json!({ "reason": bounded_reason }),
    )
    .await?;

    ds_json(&db, &user, updated).await
}

/// `POST /api/data_sources/<id>/resume` — requires `edit_data_source`.
pub async fn resume(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = find_authorized(&db, &user, id, DataSourceVerb::Resume).await?;
    let updated = data_source_service::resume(&db, existing).await?;

    record_event(
        &db,
        user.org_id,
        Some(user.id),
        "data_source.resumed",
        Some("data_source"),
        Some(id),
        json!({}),
    )
    .await?;

    ds_json(&db, &user, updated).await
}

/// One row of `GET /api/data_sources/<id>/groups`; `has_access` folds in that an unrestricted group
/// reaches every source.
#[derive(Debug, Serialize)]
pub struct DataSourceGroupAccess {
    pub group_id: i64,
    pub group_name: String,
    pub restricted: bool,
    pub has_access: bool,
}

/// Which of the org's groups can reach this data source. Read-gated on `view_groups`; writes stay
/// on the group-keyed endpoints.
pub async fn list_groups(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    require_permission(&db, &user, "view_groups").await?;
    find_in_org(&db, user.org_id, id).await?;

    let groups = group::Entity::find()
        .filter(group::Column::OrgId.eq(user.org_id))
        .all(&db)
        .await?;
    let assigned: std::collections::HashSet<i64> = data_source_group::Entity::find()
        .filter(data_source_group::Column::DataSourceId.eq(id))
        .all(&db)
        .await?
        .into_iter()
        .map(|row| row.group_id)
        .collect();

    let out = groups
        .into_iter()
        .map(|g| DataSourceGroupAccess {
            group_id: g.id,
            group_name: g.name,
            restricted: g.data_source_access_restricted,
            has_access: !g.data_source_access_restricted || assigned.contains(&g.id),
        })
        .collect::<Vec<_>>();
    Ok(Json(out))
}

#[derive(Debug, Serialize)]
pub struct DataSourceTypeResponse {
    #[serde(rename = "type")]
    pub type_name: &'static str,
    pub name: &'static str,
    pub configuration_schema: Value,
}

/// `GET /api/data_sources/types` — static connector metadata, no auth required.
pub async fn types(State(state): State<AppState>) -> impl IntoResponse {
    let types: Vec<DataSourceTypeResponse> = state
        .connectors
        .all()
        .map(|c| DataSourceTypeResponse {
            type_name: c.type_name(),
            name: display_name(c.type_name()),
            configuration_schema: c.configuration_schema(),
        })
        .collect();

    Json(types)
}

/// Human-readable connector name for `GET /api/data_sources/types`; a fixed lookup, purely
/// presentational.
fn display_name(type_name: &str) -> &'static str {
    match type_name {
        "pg" => "PostgreSQL",
        "redshift" => "Amazon Redshift",
        "cockroachdb" => "CockroachDB",
        "mysql" => "MySQL",
        "mariadb" => "MariaDB",
        "tidb" => "TiDB",
        "clickhouse" => "ClickHouse",
        "es" => "Elasticsearch",
        "mongodb" => "MongoDB",
        "sqlite" => "SQLite",
        "query_results" => "Query Results",
        "mssql" => "SQL Server",
        "bigquery" => "Google BigQuery",
        "snowflake" => "Snowflake",
        "athena" => "Amazon Athena",
        "databricks" => "Databricks",
        "prometheus" => "Prometheus",
        "influxdb" => "InfluxDB",
        "csv" => "CSV",
        "json" => "JSON",
        _ => "Unknown",
    }
}

/// Mutation handlers pre-fetch the row with a plain org-scoped lookup (they already passed the
/// `edit_data_source` gate).
async fn find_in_org(db: &Db, org_id: i64, id: i64) -> Result<data_source::Model, ApiError> {
    data_source::Entity::find_by_id(id)
        .one(db)
        .await?
        .filter(|d| d.org_id == org_id)
        .ok_or_else(|| ApiError::not_found("data source not found"))
}

#[derive(Debug, Serialize)]
pub struct UploadDataSourceResponse {
    pub upload_id: String,
    pub filename: String,
    pub byte_size: i64,
    pub content_type: String,
    pub columns: Vec<Value>,
    pub preview_rows: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// `POST /api/data_sources/uploads`: validates and stores a CSV/SQLite file, returning an
/// `upload_id` for the connector's options. Requires `edit_data_source` (no saved source to check
/// reach against).
pub async fn upload(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ApiError> {
    require_permission(&db, &user, "edit_data_source").await?;

    let max_bytes = upload_service::max_upload_bytes();
    let mut type_name: Option<String> = None;
    let mut delimiter: Option<String> = None;
    let mut filename: Option<String> = None;
    let mut content_type: Option<String> = None;
    let mut file_bytes: Option<axum::body::Bytes> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(format!("invalid multipart body: {e}")))?
    {
        match field.name().unwrap_or("") {
            "type" => {
                type_name = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| ApiError::bad_request(e.to_string()))?,
                );
            }
            "delimiter" => {
                delimiter = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| ApiError::bad_request(e.to_string()))?,
                );
            }
            "file" => {
                filename = field.file_name().map(str::to_string);
                content_type = field.content_type().map(str::to_string);
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError::bad_request(e.to_string()))?;
                // Belt-and-suspenders over the route's `DefaultBodyLimit`: only the file field's
                // size matters.
                if bytes.len() as u64 > max_bytes {
                    return Err(ApiError::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!("file exceeds the {max_bytes}-byte upload limit"),
                    ));
                }
                file_bytes = Some(bytes);
            }
            _ => {}
        }
    }

    let type_name = type_name.ok_or_else(|| ApiError::bad_request("missing 'type' field"))?;
    if type_name != "csv" && type_name != "sqlite" {
        return Err(ApiError::bad_request(
            "type must be either 'csv' or 'sqlite'",
        ));
    }
    let bytes = file_bytes.ok_or_else(|| ApiError::bad_request("missing 'file' field"))?;
    let filename = filename
        .filter(|f| !f.trim().is_empty())
        .unwrap_or_else(|| "file".to_string());
    let content_type = content_type.unwrap_or_else(|| "application/octet-stream".to_string());
    let delimiter = delimiter.unwrap_or_else(|| ",".to_string());

    // Build the preview from bytes in memory: a connector round trip would race the open request
    // transaction that inserts this upload's row.
    let (columns, preview_rows, warning) = match type_name.as_str() {
        "csv" => {
            let (columns, rows) = upload_service::parse_csv_preview(&bytes, &delimiter)
                .map_err(|e| ApiError::bad_request(format!("could not parse CSV: {e:#}")))?;
            (columns, rows, None)
        }
        "sqlite" => upload_service::parse_sqlite_preview(&bytes)
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?,
        _ => unreachable!("checked above"),
    };

    let upload_id = Uuid::new_v4();
    let object_key = storage::upload_rel_key(&type_name, upload_id, &filename);
    let stored = match state.storage.resolve(&state.db, user.org_id).await {
        Ok(org_storage) => {
            org_storage
                .put(
                    StorageArea::Uploads,
                    &object_key,
                    bytes.clone(),
                    &content_type,
                )
                .await
        }
        Err(e) => Err(e),
    };
    if let Err(e) = stored {
        if matches!(e, StorageError::Unreachable { .. }) {
            state
                .storage
                .report_failure(&state.db, user.org_id, &e)
                .await;
        }
        return Err(e.into());
    }

    #[allow(
        clippy::cast_possible_wrap,
        reason = "upload size is capped by max_upload_bytes, far below i64::MAX"
    )]
    let byte_size = bytes.len() as i64;
    let active = data_source_upload::ActiveModel {
        id: Set(upload_id),
        org_id: Set(user.org_id),
        object_key: Set(object_key),
        filename: Set(filename.clone()),
        byte_size: Set(byte_size),
        content_type: Set(content_type.clone()),
        created_by: Set(Some(user.id)),
        data_source_id: Set(None),
        created_at: Set(chrono::Utc::now().into()),
    };
    active.insert(&db).await?;

    Ok(Json(UploadDataSourceResponse {
        upload_id: upload_id.to_string(),
        filename,
        byte_size,
        content_type,
        columns,
        preview_rows,
        warning,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every real connector needs a display name, or it maps to "Unknown" and is filtered out of
    /// the New Data Source picker.
    #[test]
    fn real_connectors_have_display_names() {
        for type_name in [
            "pg",
            "redshift",
            "cockroachdb",
            "mysql",
            "mariadb",
            "tidb",
            "clickhouse",
            "es",
            "mongodb",
            "sqlite",
            "query_results",
            "mssql",
            "bigquery",
            "snowflake",
            "athena",
            "databricks",
            "prometheus",
            "influxdb",
            "csv",
            "json",
        ] {
            assert_ne!(
                display_name(type_name),
                "Unknown",
                "{type_name} is missing a display_name entry"
            );
        }
    }

    #[test]
    fn unknown_type_falls_back_to_unknown() {
        assert_eq!(display_name("not_a_real_connector"), "Unknown");
    }
}
