//! `/api/roles*` and `/api/permissions` — roles CRUD, group↔role links, permission catalog.

use crate::extract::{Json, Path};
use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, ModelTrait,
    PaginatorTrait, QueryFilter, TransactionTrait,
};
use serde::{Deserialize, Serialize};

use arcodash_core::entities::{group_role, role, user};
use arcodash_core::events::record_event;
use arcodash_core::permissions_catalog::is_valid_permission;

use crate::auth::{CurrentOrg, CurrentUser};
use crate::authz::permissions::{
    admin_conferring_group_ids, load_effective_groups, require_admin_remains,
    require_admin_survived, require_can_grant, require_can_revoke, require_permission,
    user_has_permission,
};
use crate::db::Db;
use crate::error::{is_unique_violation, ApiError};

/// Reads (`list`/`get`/`list_permissions`): `view_roles` OR `manage_groups` — a groups-manager
/// must be able to read the role list to attach roles to a group.
async fn require_read(db: &Db, caller: &CurrentUser) -> Result<(), ApiError> {
    let groups = load_effective_groups(db, &caller.0).await?;
    if user_has_permission(&caller.0, &groups, "view_roles")
        || user_has_permission(&caller.0, &groups, "manage_groups")
    {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "missing the view_roles or manage_groups permission",
        ))
    }
}

async fn require_manage(db: &Db, caller: &CurrentUser) -> Result<(), ApiError> {
    require_permission(db, &caller.0, "manage_roles").await
}

/// `GET /api/permissions` — the fixed catalog for the role editor.
pub async fn list_permissions(
    db: Db,
    caller: CurrentUser,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_read(&db, &caller).await?;
    Ok(Json(arcodash_core::permissions_catalog::catalog()))
}

// ── Response types ──────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct RoleResponse {
    pub id: i64,
    pub name: String,
    pub r#type: String,
    pub permissions: Vec<String>,
    pub group_count: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn role_response(r: role::Model, group_count: u64) -> RoleResponse {
    RoleResponse {
        id: r.id,
        name: r.name,
        r#type: r.r#type,
        permissions: r.permissions,
        group_count,
        created_at: r.created_at.into(),
        updated_at: r.updated_at.into(),
    }
}

async fn to_resp(db: &Db, r: role::Model) -> Result<RoleResponse, ApiError> {
    let group_count = group_count(db, r.id).await?;
    Ok(role_response(r, group_count))
}

// ── Helpers ─────────────────────────────────────────────────────────────────

async fn group_count(db: &Db, role_id: i64) -> Result<u64, ApiError> {
    Ok(group_role::Entity::find()
        .filter(group_role::Column::RoleId.eq(role_id))
        .count(db)
        .await?)
}

fn validate_perms(perms: &[String]) -> Result<(), ApiError> {
    perms
        .iter()
        .find(|p| !is_valid_permission(p))
        .map_or(Ok(()), |p| {
            Err(ApiError::bad_request(format!("unknown permission: {p}")))
        })
}

fn name_conflict(e: sea_orm::DbErr) -> ApiError {
    if is_unique_violation(&e) {
        ApiError::conflict("a role with that name already exists in this organization")
    } else {
        ApiError::from(e)
    }
}

pub(crate) async fn find_role(db: &Db, org_id: i64, id: i64) -> Result<role::Model, ApiError> {
    role::Entity::find_by_id(id)
        .filter(role::Column::OrgId.eq(org_id))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("role not found"))
}

// ── Handlers ────────────────────────────────────────────────────────────────

pub async fn list(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_read(&db, &caller).await?;
    let roles = role::Entity::find()
        .filter(role::Column::OrgId.eq(org.id))
        .all(&db)
        .await?;
    // One query for every role's `group_count` instead of `to_resp`'s per-role COUNT — the
    // links are grouped in memory since there's no per-role total in the schema to select.
    let role_ids: Vec<i64> = roles.iter().map(|r| r.id).collect();
    let mut counts: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();
    for link in group_role::Entity::find()
        .filter(group_role::Column::RoleId.is_in(role_ids))
        .all(&db)
        .await?
    {
        *counts.entry(link.role_id).or_insert(0) += 1;
    }
    let out = roles
        .into_iter()
        .map(|r| {
            let group_count = counts.get(&r.id).copied().unwrap_or(0);
            role_response(r, group_count)
        })
        .collect::<Vec<_>>();
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct CreateRoleBody {
    pub name: String,
    #[serde(default)]
    pub permissions: Vec<String>,
}

pub async fn create(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Json(body): Json<CreateRoleBody>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_manage(&db, &caller).await?;
    validate_perms(&body.permissions)?;
    require_can_grant(&db, &caller.0, &body.permissions).await?;
    let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
    let r = role::ActiveModel {
        org_id: Set(org.id),
        name: Set(body.name),
        permissions: Set(body.permissions),
        r#type: Set("custom".to_string()),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&db)
    .await
    .map_err(name_conflict)?;

    record_event(
        &db,
        org.id,
        Some(caller.0.id),
        "role.created",
        Some("role"),
        Some(r.id),
        serde_json::json!({ "name": r.name }),
    )
    .await?;

    Ok(Json(to_resp(&db, r).await?))
}

pub async fn get(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_read(&db, &caller).await?;
    let r = find_role(&db, org.id, id).await?;
    Ok(Json(to_resp(&db, r).await?))
}

#[derive(Deserialize)]
pub struct UpdateRoleBody {
    pub name: Option<String>,
    pub permissions: Option<Vec<String>>,
}

/// Applies `name`/`permissions` to `r`, then records `role.updated`.
async fn save_role(
    db: &impl ConnectionTrait,
    org_id: i64,
    caller_id: i64,
    r: role::Model,
    body: UpdateRoleBody,
) -> Result<role::Model, ApiError> {
    let mut active: role::ActiveModel = r.into();
    if let Some(n) = body.name {
        active.name = Set(n);
    }
    if let Some(p) = body.permissions {
        active.permissions = Set(p);
    }
    active.updated_at = Set(chrono::Utc::now().into());
    let r = active.update(db).await.map_err(name_conflict)?;
    record_event(
        db,
        org_id,
        Some(caller_id),
        "role.updated",
        Some("role"),
        Some(r.id),
        serde_json::json!({ "name": r.name }),
    )
    .await?;
    Ok(r)
}

/// Whether any OTHER role attached to `group_id` (besides `excluding_role_id`) itself confers
/// admin — i.e. whether the group would still confer admin after that one role stops doing so.
pub(crate) async fn group_still_confers_admin_via_other_roles(
    db: &impl ConnectionTrait,
    group_id: i64,
    excluding_role_id: i64,
) -> Result<bool, ApiError> {
    let other_role_ids: Vec<i64> = group_role::Entity::find()
        .filter(group_role::Column::GroupId.eq(group_id))
        .filter(group_role::Column::RoleId.ne(excluding_role_id))
        .all(db)
        .await?
        .into_iter()
        .map(|l| l.role_id)
        .collect();
    if other_role_ids.is_empty() {
        return Ok(false);
    }
    Ok(role::Entity::find()
        .filter(role::Column::Id.is_in(other_role_ids))
        .all(db)
        .await?
        .iter()
        .any(|r| r.permissions.iter().any(|p| p == "admin")))
}

pub async fn update(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path(id): Path<i64>,
    Json(body): Json<UpdateRoleBody>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_manage(&db, &caller).await?;
    let r = find_role(&db, org.id, id).await?;
    if r.r#type == "builtin" {
        return Err(ApiError::forbidden("built-in roles cannot be edited"));
    }
    if let Some(ref p) = body.permissions {
        validate_perms(p)?;
        require_can_grant(&db, &caller.0, p).await?;
        // `require_can_grant` bounds what's being added; it passes trivially when `p` is a
        // subset of `r.permissions` (nothing new to grant), which would let a `manage_roles`
        // holder who isn't an admin strip `admin` (or any other key) from a role by sending a
        // narrower list. Bound removal the same way, on the set difference only — keeping or
        // reordering a permission must not require holding it.
        let revoked: Vec<String> = r
            .permissions
            .iter()
            .filter(|old| !p.contains(old))
            .cloned()
            .collect();
        require_can_revoke(&db, &caller.0, &revoked).await?;
    }

    // A role update that drops `admin` from its permissions can strip admin-conferring status
    // from every group attached to it — the same privilege-escalation surface
    // `user_service::update`/`groups::remove_role` already guard, gated the same way: an
    // org-scoped advisory lock held for the whole check-write-recheck sequence so a concurrent
    // admin-removal elsewhere can't race it into zero admins.
    let removes_admin = body.permissions.as_ref().is_some_and(|new_perms| {
        r.permissions.iter().any(|p| p == "admin") && !new_perms.iter().any(|p| p == "admin")
    });

    if !removes_admin {
        let r = save_role(&db, org.id, caller.0.id, r, body).await?;
        return Ok(Json(to_resp(&db, r).await?));
    }

    let txn = db.begin().await?;
    txn.execute_raw(sea_orm::Statement::from_sql_and_values(
        sea_orm::DbBackend::Postgres,
        "SELECT pg_advisory_xact_lock($1)",
        [org.id.into()],
    ))
    .await?;

    let attached_group_ids: Vec<i64> = group_role::Entity::find()
        .filter(group_role::Column::RoleId.eq(id))
        .all(&txn)
        .await?
        .into_iter()
        .map(|l| l.group_id)
        .collect();

    let mut admin_groups_after = admin_conferring_group_ids(&txn, org.id).await?;
    for group_id in &attached_group_ids {
        if !admin_groups_after.contains(group_id) {
            continue;
        }
        if !group_still_confers_admin_via_other_roles(&txn, *group_id, id).await? {
            admin_groups_after.retain(|g| g != group_id);
        }
    }

    let had_admin_before =
        require_admin_remains(&txn, org.id, &admin_groups_after, |u: &user::Model| {
            Some(u.groups.clone())
        })
        .await?;

    // The event lands in `txn`, so a failed recheck rolls it back with the write.
    let r = save_role(&txn, org.id, caller.0.id, r, body).await?;
    require_admin_survived(&txn, org.id, had_admin_before).await?;
    txn.commit().await?;

    Ok(Json(to_resp(&db, r).await?))
}

pub async fn delete(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_manage(&db, &caller).await?;
    let r = find_role(&db, org.id, id).await?;
    if r.r#type == "builtin" {
        return Err(ApiError::forbidden("built-in roles cannot be deleted"));
    }
    if group_count(&db, r.id).await? > 0 {
        return Err(ApiError::conflict("role is still assigned to groups"));
    }
    let (role_id, name) = (r.id, r.name.clone());
    r.delete(&db).await?;

    record_event(
        &db,
        org.id,
        Some(caller.0.id),
        "role.deleted",
        Some("role"),
        Some(role_id),
        serde_json::json!({ "name": name }),
    )
    .await?;

    Ok(Json(serde_json::json!({ "ok": true })))
}
