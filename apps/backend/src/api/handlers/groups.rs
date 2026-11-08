//! `/api/groups*` — group CRUD, member management, data-source assignment.

use crate::extract::{Json, Path};
use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, ModelTrait,
    QueryFilter, TransactionTrait,
};
use serde::{Deserialize, Serialize};

use arcodash_core::entities::{data_source, data_source_group, group, group_role, role, user};
use arcodash_core::events::record_event;

use crate::api::handlers::roles::{find_role, group_still_confers_admin_via_other_roles};
use crate::auth::{CurrentOrg, CurrentUser};
use crate::authz::data_source::{in_scope, postures};
use crate::authz::permissions::{
    admin_conferring_group_ids, is_admin, load_effective_groups, require_admin_remains,
    require_admin_survived, require_can_grant, require_can_grant_groups, require_can_revoke,
    require_permission,
};
use crate::db::Db;
use crate::error::{is_unique_violation, ApiError};

#[derive(Serialize)]
pub struct GroupResponse {
    pub id: i64,
    pub name: String,
    pub r#type: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn to_resp(g: group::Model) -> GroupResponse {
    GroupResponse {
        id: g.id,
        name: g.name,
        r#type: g.r#type,
        created_at: g.created_at.into(),
        updated_at: g.updated_at.into(),
    }
}

async fn find_group(db: &Db, org_id: i64, id: i64) -> Result<group::Model, ApiError> {
    group::Entity::find_by_id(id)
        .filter(group::Column::OrgId.eq(org_id))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("group not found"))
}

async fn find_org_user(db: &Db, org_id: i64, id: i64) -> Result<user::Model, ApiError> {
    user::Entity::find_by_id(id)
        .filter(user::Column::OrgId.eq(org_id))
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("user not found"))
}

fn name_conflict(e: sea_orm::DbErr) -> ApiError {
    if is_unique_violation(&e) {
        ApiError::conflict("a group with that name already exists in this organization")
    } else {
        ApiError::from(e)
    }
}

/// `insert_scoped` that treats a unique violation (a concurrent duplicate) as "already there":
/// `Ok(true)` only when this call inserted the row.
async fn insert_if_absent<A>(db: &Db, active: A) -> Result<bool, ApiError>
where
    A: ActiveModelTrait + sea_orm::ActiveModelBehavior + Send + 'static,
    <A::Entity as EntityTrait>::Model: sea_orm::IntoActiveModel<A> + Send,
{
    match crate::db::insert_scoped(db, active).await {
        Ok(_) => Ok(true),
        Err(e) if is_unique_violation(&e) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

async fn require_view(db: &Db, caller: &CurrentUser) -> Result<(), ApiError> {
    require_permission(db, &caller.0, "view_groups").await
}

async fn require_manage(db: &Db, caller: &CurrentUser) -> Result<(), ApiError> {
    require_permission(db, &caller.0, "manage_groups").await
}

/// Runs a write that could strip the org's last admin inside one advisory-locked transaction, so
/// two concurrent removals can't each see "an admin remains" and leave zero. The lock is
/// transaction-scoped (`pg_advisory_xact_lock`), hence this begins and holds the transaction for
/// the whole check-write-recheck. `admin_groups_after` adjusts the admin-conferring group list for
/// this change (inside the lock, since callers may query other tables); `projected_groups` is
/// `require_admin_remains`'s per-user post-write projection.
#[allow(clippy::too_many_arguments)]
async fn write_admin_gated_change(
    db: &Db,
    org_id: i64,
    actor_id: i64,
    admin_groups_after: impl for<'a> FnOnce(
        &'a sea_orm::DatabaseTransaction,
        Vec<i64>,
    )
        -> crate::auth::BoxFuture<'a, Result<Vec<i64>, ApiError>>,
    projected_groups: impl Fn(&user::Model) -> Option<Vec<i64>>,
    write: impl for<'a> FnOnce(
        &'a sea_orm::DatabaseTransaction,
    ) -> crate::auth::BoxFuture<'a, Result<(), ApiError>>,
    action: &str,
    object_id: i64,
    props: serde_json::Value,
) -> Result<(), ApiError> {
    let txn = db.begin().await?;
    txn.execute_raw(sea_orm::Statement::from_sql_and_values(
        sea_orm::DbBackend::Postgres,
        "SELECT pg_advisory_xact_lock($1)",
        [org_id.into()],
    ))
    .await?;

    let admin_groups = admin_conferring_group_ids(&txn, org_id).await?;
    let admin_groups = admin_groups_after(&txn, admin_groups).await?;
    let had_admin_before =
        require_admin_remains(&txn, org_id, &admin_groups, projected_groups).await?;

    write(&txn).await?;

    // Checks the post-write rows against `had_admin_before`, captured earlier.
    require_admin_survived(&txn, org_id, had_admin_before).await?;

    record_event(
        &txn,
        org_id,
        Some(actor_id),
        action,
        Some("group"),
        Some(object_id),
        props,
    )
    .await?;

    txn.commit().await?;
    Ok(())
}

pub async fn list(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_view(&db, &caller).await?;
    let groups = group::Entity::find()
        .filter(group::Column::OrgId.eq(org.id))
        .all(&db)
        .await?;
    Ok(Json(groups.into_iter().map(to_resp).collect::<Vec<_>>()))
}

#[derive(Deserialize)]
pub struct CreateGroupBody {
    pub name: String,
}

pub async fn create(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Json(body): Json<CreateGroupBody>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_manage(&db, &caller).await?;
    let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
    let g = group::ActiveModel {
        org_id: Set(org.id),
        name: Set(body.name),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
        created_at: Set(now),
        updated_at: Set(now),
        // A new group grants no data-source reach until an admin opts it in, closing the
        // create-then-widen escalation.
        data_source_access_restricted: Set(true),
        ..Default::default()
    }
    .insert(&db)
    .await
    .map_err(name_conflict)?;

    record_event(
        &db,
        org.id,
        Some(caller.0.id),
        "group.created",
        Some("group"),
        Some(g.id),
        serde_json::json!({ "name": g.name }),
    )
    .await?;

    Ok(Json(to_resp(g)))
}

pub async fn get(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_view(&db, &caller).await?;
    let g = find_group(&db, org.id, id).await?;
    Ok(Json(to_resp(g)))
}

#[derive(Deserialize)]
pub struct UpdateGroupBody {
    pub name: Option<String>,
}

pub async fn update(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path(id): Path<i64>,
    Json(body): Json<UpdateGroupBody>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_manage(&db, &caller).await?;
    let g = find_group(&db, org.id, id).await?;
    if body.name.is_some() && g.r#type == "builtin" {
        return Err(ApiError::bad_request("cannot rename built-in groups"));
    }
    let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
    let mut active: group::ActiveModel = g.into();
    if let Some(name) = body.name {
        active.name = Set(name);
    }
    active.updated_at = Set(now);
    let g = active.update(&db).await.map_err(name_conflict)?;

    record_event(
        &db,
        org.id,
        Some(caller.0.id),
        "group.updated",
        Some("group"),
        Some(g.id),
        serde_json::json!({ "name": g.name }),
    )
    .await?;

    Ok(Json(to_resp(g)))
}

pub async fn delete(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_manage(&db, &caller).await?;
    let g = find_group(&db, org.id, id).await?;
    if g.r#type == "builtin" {
        return Err(ApiError::bad_request("cannot delete built-in groups"));
    }

    // Deleting a group strips its roles' permissions from its members, so apply the same ceiling as
    // `add_role`'s `require_can_grant`. A group's `permissions` column is vestigial.
    let attached_role_ids: Vec<i64> = group_role::Entity::find()
        .filter(group_role::Column::GroupId.eq(id))
        .all(&db)
        .await?
        .into_iter()
        .map(|l| l.role_id)
        .collect();
    let mut revoked: Vec<String> = if attached_role_ids.is_empty() {
        vec![]
    } else {
        role::Entity::find()
            .filter(role::Column::Id.is_in(attached_role_ids))
            .all(&db)
            .await?
            .into_iter()
            .flat_map(|r| r.permissions)
            .collect()
    };
    revoked.sort();
    revoked.dedup();
    require_can_revoke(&db, &caller.0, &revoked).await?;

    let name = g.name.clone();
    write_admin_gated_change(
        &db,
        org.id,
        caller.0.id,
        move |_txn, mut groups| {
            Box::pin(async move {
                groups.retain(|gid| *gid != id);
                Ok(groups)
            })
        },
        |u| Some(u.groups.clone()),
        move |txn| Box::pin(async move { g.delete(txn).await.map(|_| ()).map_err(ApiError::from) }),
        "group.deleted",
        id,
        serde_json::json!({ "name": name }),
    )
    .await?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Serialize)]
pub struct MemberResponse {
    pub id: i64,
    pub name: String,
    pub email: String,
}

pub async fn list_members(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_view(&db, &caller).await?;
    find_group(&db, org.id, id).await?;
    let members = user::Entity::find()
        .filter(user::Column::OrgId.eq(org.id))
        .filter(sea_orm::sea_query::Expr::cust_with_values(
            "$1 = ANY(users.groups)",
            [id],
        ))
        .all(&db)
        .await?
        .into_iter()
        .map(|u| MemberResponse {
            id: u.id,
            name: u.name,
            email: u.email,
        })
        .collect::<Vec<_>>();
    Ok(Json(members))
}

#[derive(Deserialize)]
pub struct AddMemberBody {
    pub user_id: i64,
}

pub async fn add_member(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path(id): Path<i64>,
    Json(body): Json<AddMemberBody>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_manage(&db, &caller).await?;
    find_group(&db, org.id, id).await?;
    let u = find_org_user(&db, org.id, body.user_id).await?;
    if !u.groups.contains(&id) {
        require_can_grant_groups(&db, &caller.0, org.id, &[id]).await?;
        crate::services::user_service::mutate_user_groups(&db, body.user_id, org.id, &[id], &[])
            .await?;

        record_event(
            &db,
            org.id,
            Some(caller.0.id),
            "group.member_added",
            Some("group"),
            Some(id),
            serde_json::json!({ "user_id": body.user_id }),
        )
        .await?;
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn remove_member(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path((id, user_id)): Path<(i64, i64)>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_manage(&db, &caller).await?;
    find_group(&db, org.id, id).await?;
    find_org_user(&db, org.id, user_id).await?;

    // Runs in the org-scoped advisory-locked transaction (`write_admin_gated_change`) so concurrent
    // removals can't read each other's uncommitted writes under READ COMMITTED.
    write_admin_gated_change(
        &db,
        org.id,
        caller.0.id,
        |_txn, groups| Box::pin(async move { Ok(groups) }),
        move |candidate| {
            Some(if candidate.id == user_id {
                candidate
                    .groups
                    .iter()
                    .copied()
                    .filter(|&g| g != id)
                    .collect()
            } else {
                candidate.groups.clone()
            })
        },
        move |txn| {
            Box::pin(async move {
                crate::services::user_service::mutate_user_groups(txn, user_id, org.id, &[], &[id])
                    .await
                    .map(|_| ())
            })
        },
        "group.member_removed",
        id,
        serde_json::json!({ "user_id": user_id }),
    )
    .await?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Serialize)]
pub struct DataSourceAssignment {
    pub id: i64,
    pub data_source_id: i64,
    pub group_id: i64,
    /// Display name of the data source, joined in so the frontend needs no second round trip.
    pub name: String,
}

/// `list_data_sources` response: the group's posture plus its allowlist in one call.
#[derive(Serialize)]
pub struct DataSourceAccessResponse {
    pub restricted: bool,
    pub entries: Vec<DataSourceAssignment>,
}

pub async fn list_data_sources(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_view(&db, &caller).await?;
    let g = find_group(&db, org.id, id).await?;
    let rows = data_source_group::Entity::find()
        .filter(data_source_group::Column::GroupId.eq(id))
        .all(&db)
        .await?;
    let ds_ids: Vec<i64> = rows.iter().map(|a| a.data_source_id).collect();
    let names: std::collections::HashMap<i64, String> = data_source::Entity::find()
        .filter(data_source::Column::Id.is_in(ds_ids))
        .all(&db)
        .await?
        .into_iter()
        .map(|d| (d.id, d.name))
        .collect();
    let entries = rows
        .into_iter()
        .map(|a| DataSourceAssignment {
            id: a.id,
            data_source_id: a.data_source_id,
            group_id: a.group_id,
            name: names
                .get(&a.data_source_id)
                .cloned()
                .unwrap_or_else(|| "Unknown data source".to_string()),
        })
        .collect::<Vec<_>>();
    Ok(Json(DataSourceAccessResponse {
        restricted: g.data_source_access_restricted,
        entries,
    }))
}

#[derive(Deserialize)]
pub struct AssignDataSourceBody {
    pub data_source_id: i64,
}

pub async fn assign_data_source(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path(id): Path<i64>,
    Json(body): Json<AssignDataSourceBody>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_manage(&db, &caller).await?;
    find_group(&db, org.id, id).await?;
    // Cross-org guard: a group may only link to a data source in its own org (which holds that
    // org's DB credentials).
    data_source::Entity::find_by_id(body.data_source_id)
        .filter(data_source::Column::OrgId.eq(org.id))
        .one(&db)
        .await?
        .ok_or_else(|| ApiError::not_found("data source not found"))?;
    let caller_groups = load_effective_groups(&db, &caller.0).await?;
    if !is_admin(&caller_groups) {
        let caller_rows = data_source_group::Entity::find()
            .filter(data_source_group::Column::GroupId.is_in(caller.0.groups.clone()))
            .all(&db)
            .await?;
        if !in_scope(&postures(&caller_groups), body.data_source_id, &caller_rows) {
            return Err(ApiError::forbidden(
                "you cannot grant access to a data source you cannot reach",
            ));
        }
    }
    let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
    let existing = data_source_group::Entity::find()
        .filter(data_source_group::Column::GroupId.eq(id))
        .filter(data_source_group::Column::DataSourceId.eq(body.data_source_id))
        .one(&db)
        .await?;
    if existing.is_none()
        && insert_if_absent(
            &db,
            data_source_group::ActiveModel {
                data_source_id: Set(body.data_source_id),
                group_id: Set(id),
                created_at: Set(now),
                updated_at: Set(now),
                ..Default::default()
            },
        )
        .await?
    {
        record_event(
            &db,
            org.id,
            Some(caller.0.id),
            "group.data_source_assigned",
            Some("group"),
            Some(id),
            serde_json::json!({ "data_source_id": body.data_source_id }),
        )
        .await?;
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct SetDataSourceAccessBody {
    pub restricted: bool,
}

/// `PUT /groups/{id}/data_source_access`: toggles the group's posture.
pub async fn set_data_source_access(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path(id): Path<i64>,
    Json(body): Json<SetDataSourceAccessBody>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_manage(&db, &caller).await?;
    let g = find_group(&db, org.id, id).await?;
    // Widening (restricted -> unrestricted) grants reach over every data source, so bound it like
    // any reach grant; narrowing never needs this.
    if !body.restricted && g.data_source_access_restricted {
        let caller_groups = load_effective_groups(&db, &caller.0).await?;
        if !is_admin(&caller_groups) && !postures(&caller_groups).iter().any(|p| !p.restricted) {
            return Err(ApiError::forbidden(
                "cannot grant unrestricted data-source access you do not hold",
            ));
        }
    }
    let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
    let mut active: group::ActiveModel = g.into();
    active.data_source_access_restricted = Set(body.restricted);
    active.updated_at = Set(now);
    active.update(&db).await?;

    record_event(
        &db,
        org.id,
        Some(caller.0.id),
        "group.data_source_access_restricted_set",
        Some("group"),
        Some(id),
        serde_json::json!({ "restricted": body.restricted }),
    )
    .await?;

    Ok(Json(
        serde_json::json!({ "ok": true, "restricted": body.restricted }),
    ))
}

pub async fn remove_data_source(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path((id, ds_id)): Path<(i64, i64)>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_manage(&db, &caller).await?;
    find_group(&db, org.id, id).await?;
    data_source_group::Entity::delete_many()
        .filter(data_source_group::Column::GroupId.eq(id))
        .filter(data_source_group::Column::DataSourceId.eq(ds_id))
        .exec(&db)
        .await?;

    record_event(
        &db,
        org.id,
        Some(caller.0.id),
        "group.data_source_removed",
        Some("group"),
        Some(id),
        serde_json::json!({ "data_source_id": ds_id }),
    )
    .await?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Serialize)]
pub struct GroupRoleResponse {
    pub id: i64,
    pub name: String,
    pub permissions: Vec<String>,
}

pub async fn list_roles(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_view(&db, &caller).await?;
    find_group(&db, org.id, id).await?;
    let links = group_role::Entity::find()
        .filter(group_role::Column::GroupId.eq(id))
        .all(&db)
        .await?;
    let role_ids: Vec<i64> = links.iter().map(|l| l.role_id).collect();
    let roles = if role_ids.is_empty() {
        vec![]
    } else {
        role::Entity::find()
            .filter(role::Column::Id.is_in(role_ids))
            .all(&db)
            .await?
    };
    let out: Vec<_> = roles
        .into_iter()
        .map(|r| GroupRoleResponse {
            id: r.id,
            name: r.name,
            permissions: r.permissions,
        })
        .collect();
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct AddRoleBody {
    pub role_id: i64,
}

pub async fn add_role(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path(id): Path<i64>,
    Json(body): Json<AddRoleBody>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_manage(&db, &caller).await?;
    find_group(&db, org.id, id).await?;
    // The role must belong to the caller's org, or an admin could grant another org's role.
    let r = find_role(&db, org.id, body.role_id).await?;
    require_can_grant(&db, &caller.0, &r.permissions).await?;
    let existing = group_role::Entity::find()
        .filter(group_role::Column::GroupId.eq(id))
        .filter(group_role::Column::RoleId.eq(body.role_id))
        .one(&db)
        .await?;
    let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
    if existing.is_none()
        && insert_if_absent(
            &db,
            group_role::ActiveModel {
                group_id: Set(id),
                role_id: Set(body.role_id),
                created_at: Set(now),
                updated_at: Set(now),
                ..Default::default()
            },
        )
        .await?
    {
        record_event(
            &db,
            org.id,
            Some(caller.0.id),
            "group.role_attached",
            Some("group"),
            Some(id),
            serde_json::json!({ "role_id": body.role_id }),
        )
        .await?;
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn remove_role(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Path((id, role_id)): Path<(i64, i64)>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    require_manage(&db, &caller).await?;
    // Detaching a role from a builtin group is ordinary reconfiguration (unlike `delete`), so no
    // `type == "builtin"` guard; the grant-ceiling check and the last-admin check are the
    // protection.
    find_group(&db, org.id, id).await?;

    // Same ceiling as attaching: a non-admin `manage_groups` holder must not strip a role's
    // permissions (e.g. `admin`) by detaching it.
    let r = find_role(&db, org.id, role_id).await?;
    require_can_revoke(&db, &caller.0, &r.permissions).await?;

    write_admin_gated_change(
        &db,
        org.id,
        caller.0.id,
        move |txn, mut groups| {
            Box::pin(async move {
                // Would `id` still confer admin through another attached role after this detach?
                if groups.contains(&id)
                    && !group_still_confers_admin_via_other_roles(txn, id, role_id).await?
                {
                    groups.retain(|gid| *gid != id);
                }
                Ok(groups)
            })
        },
        |u| Some(u.groups.clone()),
        move |txn| {
            Box::pin(async move {
                group_role::Entity::delete_many()
                    .filter(group_role::Column::GroupId.eq(id))
                    .filter(group_role::Column::RoleId.eq(role_id))
                    .exec(txn)
                    .await?;
                Ok(())
            })
        },
        "group.role_detached",
        id,
        serde_json::json!({ "role_id": role_id }),
    )
    .await?;

    Ok(Json(serde_json::json!({ "ok": true })))
}
