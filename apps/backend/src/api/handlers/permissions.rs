//! `/api/{queries,dashboards}/{id}/acl` — per-object access control.

use crate::extract::{Json, Path};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use arcodash_core::entities::user;

use crate::auth::{CurrentOrg, CurrentUser};
use crate::db::Db;
use crate::error::ApiError;
use crate::services::{dashboard_service, monitor_service, permission_service, query_service};

#[derive(Debug, Serialize)]
pub struct PermissionResponse {
    pub id: i64,
    pub object_type: String,
    pub object_id: i64,
    pub access_type: String,
    pub grantor_id: i64,
    pub grantee_id: Option<i64>,
    pub grantee_group_id: Option<i64>,
    pub created_at: DateTime<Utc>,
}

impl From<arcodash_core::entities::access_permission::Model> for PermissionResponse {
    fn from(m: arcodash_core::entities::access_permission::Model) -> Self {
        Self {
            id: m.id,
            object_type: m.object_type,
            object_id: m.object_id,
            access_type: m.access_type,
            grantor_id: m.grantor_id,
            grantee_id: m.grantee_id,
            grantee_group_id: m.grantee_group_id,
            created_at: m.created_at.into(),
        }
    }
}

/// `user_id` is accepted as an alias for `grantee_id` (the older wire name), so callers that still
/// send it keep working.
///
/// Exactly one of `grantee_id`/ `grantee_group_id` must be set; validated in
/// `permission_service::grant`.
#[derive(Debug, Deserialize)]
pub struct GrantBody {
    #[serde(alias = "user_id", default)]
    pub grantee_id: Option<i64>,
    pub grantee_group_id: Option<i64>,
    pub access_type: String,
}

#[derive(Debug, Deserialize)]
pub struct SetVisibilityBody {
    pub visibility: String,
}

async fn list_acl(
    db: &Db,
    org_id: i64,
    object_type: &str,
    object_id: i64,
) -> Result<Json<Vec<PermissionResponse>>, ApiError> {
    let perms = permission_service::list(db, org_id, object_type, object_id).await?;
    Ok(Json(
        perms.into_iter().map(PermissionResponse::from).collect(),
    ))
}

async fn grant_acl(
    db: &Db,
    org_id: i64,
    user: &user::Model,
    object_type: &str,
    object_id: i64,
    body: GrantBody,
) -> Result<Json<PermissionResponse>, ApiError> {
    let perm = permission_service::grant(
        db,
        org_id,
        user,
        object_type,
        object_id,
        body.grantee_id,
        body.grantee_group_id,
        &body.access_type,
    )
    .await?;
    Ok(Json(PermissionResponse::from(perm)))
}

// ── Query ACL ──────────────────────────────────────────────────────────────────

pub async fn list_query_acl(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path(query_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    // Verify query is accessible.
    query_service::get_for_user(&db, org.id, &user, query_id).await?;
    list_acl(&db, org.id, "query", query_id).await
}

pub async fn grant_query_acl(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path(query_id): Path<i64>,
    Json(body): Json<GrantBody>,
) -> Result<impl IntoResponse, ApiError> {
    query_service::get_for_user(&db, org.id, &user, query_id).await?;
    grant_acl(&db, org.id, &user, "query", query_id, body).await
}

pub async fn revoke_query_acl(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path((query_id, perm_id)): Path<(i64, i64)>,
) -> Result<impl IntoResponse, ApiError> {
    permission_service::revoke(&db, org.id, &user, "query", query_id, perm_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Dashboard ACL ──────────────────────────────────────────────────────────────

pub async fn list_dashboard_acl(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path(dashboard_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    // `permission_service::list` doesn't filter by org itself (by design — see its own
    // comment); without this check any authenticated user in any org could list who has
    // access to any dashboard in the whole system by its numeric id. 404s if the dashboard
    // isn't in this org or isn't visible to `user`, same convention as `list_query_acl`'s
    // `get_for_user` call above.
    dashboard_service::get_by_id(&db, org.id, &user, dashboard_id).await?;
    list_acl(&db, org.id, "dashboard", dashboard_id).await
}

pub async fn grant_dashboard_acl(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path(dashboard_id): Path<i64>,
    Json(body): Json<GrantBody>,
) -> Result<impl IntoResponse, ApiError> {
    grant_acl(&db, org.id, &user, "dashboard", dashboard_id, body).await
}

pub async fn revoke_dashboard_acl(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path((dashboard_id, perm_id)): Path<(i64, i64)>,
) -> Result<impl IntoResponse, ApiError> {
    permission_service::revoke(&db, org.id, &user, "dashboard", dashboard_id, perm_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Monitor ACL ────────────────────────────────────────────────────────────────

pub async fn list_monitor_acl(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path(monitor_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    // Verify monitor is accessible, same convention as `list_query_acl`/`list_dashboard_acl`.
    monitor_service::get_for_user(&db, org.id, &user, monitor_id).await?;
    list_acl(&db, org.id, "monitor", monitor_id).await
}

pub async fn grant_monitor_acl(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path(monitor_id): Path<i64>,
    Json(body): Json<GrantBody>,
) -> Result<impl IntoResponse, ApiError> {
    monitor_service::get_for_user(&db, org.id, &user, monitor_id).await?;
    grant_acl(&db, org.id, &user, "monitor", monitor_id, body).await
}

pub async fn revoke_monitor_acl(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path((monitor_id, perm_id)): Path<(i64, i64)>,
) -> Result<impl IntoResponse, ApiError> {
    permission_service::revoke(&db, org.id, &user, "monitor", monitor_id, perm_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Visibility ───────────────────────────────────────────────────────────────

/// `PUT /api/queries/{id}/visibility`
pub async fn set_query_visibility(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path(query_id): Path<i64>,
    Json(body): Json<SetVisibilityBody>,
) -> Result<impl IntoResponse, ApiError> {
    permission_service::set_visibility(&db, org.id, &user, "query", query_id, &body.visibility)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/dashboards/{id}/visibility`
pub async fn set_dashboard_visibility(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path(dashboard_id): Path<i64>,
    Json(body): Json<SetVisibilityBody>,
) -> Result<impl IntoResponse, ApiError> {
    permission_service::set_visibility(
        &db,
        org.id,
        &user,
        "dashboard",
        dashboard_id,
        &body.visibility,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/monitors/{id}/visibility`
pub async fn set_monitor_visibility(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path(monitor_id): Path<i64>,
    Json(body): Json<SetVisibilityBody>,
) -> Result<impl IntoResponse, ApiError> {
    permission_service::set_visibility(&db, org.id, &user, "monitor", monitor_id, &body.visibility)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
