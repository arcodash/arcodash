//! Object-level ACL (`access_permissions`): grants view/modify on a query or dashboard to a user or
//! group, additive to group-level data-source access.

use chrono::Utc;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, TransactionTrait,
};

use arcodash_core::entities::{access_permission, dashboard, group, monitor, query, user};
use arcodash_core::events::record_event;

use crate::authz::access::ObjectRole;
use crate::authz::authorize::{evaluate, Action, DashboardVerb, MonitorVerb, QueryVerb};
use crate::error::{is_unique_violation, ApiError};

pub async fn list(
    db: &impl ConnectionTrait,
    org_id: i64,
    object_type: &str,
    object_id: i64,
) -> Result<Vec<access_permission::Model>, ApiError> {
    let _ = org_id; // org scope enforced by the object lookup in the handler
    Ok(access_permission::Entity::find()
        .filter(access_permission::Column::ObjectType.eq(object_type))
        .filter(access_permission::Column::ObjectId.eq(object_id))
        .all(db)
        .await?)
}

/// Grant access to a user or a group (exactly one of `grantee_id`/`grantee_group_id`).
///
/// `access_type` takes `owner`/`editor`/`viewer` or the older `view`/`modify`, normalized before
/// storing. Re-granting updates the existing row in place.
pub async fn grant(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    org_id: i64,
    grantor: &user::Model,
    object_type: &str,
    object_id: i64,
    grantee_id: Option<i64>,
    grantee_group_id: Option<i64>,
    access_type: &str,
) -> Result<access_permission::Model, ApiError> {
    let normalized_access_type = match access_type {
        "owner" | "editor" | "viewer" => access_type,
        "view" => "viewer",
        "modify" => "editor",
        _ => {
            return Err(ApiError::bad_request(
                "access_type must be one of \"owner\", \"editor\", or \"viewer\" \
                 (legacy \"view\"/\"modify\" are also accepted)",
            ))
        }
    };

    if grantee_id.is_some() == grantee_group_id.is_some() {
        return Err(ApiError::bad_request(
            "exactly one of grantee_id or grantee_group_id must be set",
        ));
    }

    require_owner_reach(
        db,
        org_id,
        grantor,
        object_type,
        object_id,
        "only the object owner or an admin can share",
    )
    .await?;

    let grantor_is_admin = {
        let groups = crate::authz::permissions::load_effective_groups(db, grantor).await?;
        crate::authz::permissions::is_admin(&groups)
    };

    if let Some(uid) = grantee_id {
        user::Entity::find_by_id(uid)
            .filter(user::Column::OrgId.eq(org_id))
            .one(db)
            .await?
            .ok_or_else(|| ApiError::not_found("user not found"))?;
    }
    if let Some(gid) = grantee_group_id {
        group::Entity::find_by_id(gid)
            .filter(group::Column::OrgId.eq(org_id))
            .one(db)
            .await?
            .ok_or_else(|| ApiError::not_found("group not found"))?;
    }

    let now = Utc::now();

    // A closure matching the set grantee column (the two are partial-unique), re-runnable after a
    // racing insert.
    let find_existing = || {
        let q = access_permission::Entity::find()
            .filter(access_permission::Column::ObjectType.eq(object_type))
            .filter(access_permission::Column::ObjectId.eq(object_id));
        match (grantee_id, grantee_group_id) {
            (Some(uid), None) => q.filter(access_permission::Column::GranteeId.eq(uid)),
            (None, Some(gid)) => q.filter(access_permission::Column::GranteeGroupId.eq(gid)),
            _ => q,
        }
    };

    let set_access_type = |existing: access_permission::Model| {
        let mut active: access_permission::ActiveModel = existing.into();
        active.access_type = Set(normalized_access_type.to_string());
        active.updated_at = Set(now.into());
        active.update(db)
    };

    let model = if let Some(existing) = find_existing().one(db).await? {
        let downgrading_owner =
            existing.access_type == "owner" && normalized_access_type != "owner";
        let is_own_grant = grantee_id == Some(grantor.id);
        if downgrading_owner && !is_own_grant && !grantor_is_admin {
            return Err(ApiError::forbidden(
                "only an admin can downgrade an owner grant",
            ));
        }
        set_access_type(existing).await?
    } else {
        let active = access_permission::ActiveModel {
            object_type: Set(object_type.to_string()),
            object_id: Set(object_id),
            access_type: Set(normalized_access_type.to_string()),
            grantor_id: Set(grantor.id),
            grantee_id: Set(grantee_id),
            grantee_group_id: Set(grantee_group_id),
            created_at: Set(now.into()),
            updated_at: Set(now.into()),
            ..Default::default()
        };

        match crate::db::insert_scoped(db, active).await {
            Ok(m) => m,
            // Lost a race against a concurrent grant; update the row it created.
            Err(e) if is_unique_violation(&e) => {
                let existing = find_existing()
                    .one(db)
                    .await?
                    .ok_or_else(|| ApiError::not_found("permission not found"))?;
                set_access_type(existing).await?
            }
            Err(e) => return Err(e.into()),
        }
    };

    record_event(
        db,
        org_id,
        Some(grantor.id),
        "permission.granted",
        Some(object_type),
        Some(object_id),
        serde_json::json!({
            "grantee_user_id": grantee_id,
            "grantee_group_id": grantee_group_id,
            "access_type": normalized_access_type,
        }),
    )
    .await?;

    Ok(model)
}

/// Change an object's `visibility` tier (`"workspace"`, `"restricted"`, `"public"`; `"public"` only
/// for dashboards). Requires Owner-tier reach, with the same uniform 404 as [`grant`].
pub async fn set_visibility(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    object_type: &str,
    object_id: i64,
    visibility: &str,
) -> Result<(), ApiError> {
    require_owner_reach(
        db,
        org_id,
        user,
        object_type,
        object_id,
        "must be the object owner or an admin to change visibility",
    )
    .await?;

    if !matches!(visibility, "workspace" | "restricted" | "public") {
        return Err(ApiError::bad_request(
            "visibility must be one of \"workspace\", \"restricted\", or \"public\"",
        ));
    }
    if visibility == "public" && object_type != "dashboard" {
        return Err(ApiError::bad_request(
            "public visibility is only supported for dashboards",
        ));
    }

    let from = match object_type {
        "query" => {
            let q = query::Entity::find_by_id(object_id)
                .one(db)
                .await?
                .ok_or_else(|| ApiError::not_found("query not found"))?;
            let from = q.visibility.clone();
            let mut active: query::ActiveModel = q.into();
            active.visibility = Set(visibility.to_string());
            active.update(db).await?;
            from
        }
        "dashboard" => {
            let d = dashboard::Entity::find_by_id(object_id)
                .one(db)
                .await?
                .ok_or_else(|| ApiError::not_found("dashboard not found"))?;
            let from = d.visibility.clone();
            let mut active: dashboard::ActiveModel = d.into();
            active.visibility = Set(visibility.to_string());
            active.update(db).await?;
            from
        }
        "monitor" => {
            let m = monitor::Entity::find_by_id(object_id)
                .one(db)
                .await?
                .ok_or_else(|| ApiError::not_found("monitor not found"))?;
            let from = m.visibility.clone();
            let mut active: monitor::ActiveModel = m.into();
            active.visibility = Set(visibility.to_string());
            active.update(db).await?;
            from
        }
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown object_type: {other}"
            )))
        }
    };

    record_event(
        db,
        org_id,
        Some(user.id),
        "object.visibility_changed",
        Some(object_type),
        Some(object_id),
        serde_json::json!({ "from": from, "to": visibility }),
    )
    .await?;

    Ok(())
}

/// Revoke an `access_permission` by id: admin, an owner-tier grantee with edit capability, or the
/// original grantor.
///
/// The route's `object_type`/`object_id` are checked against the loaded row first, so a `perm_id`
/// for another object 404s.
pub async fn revoke(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    object_type: &str,
    object_id: i64,
    permission_id: i64,
) -> Result<(), ApiError> {
    let perm = access_permission::Entity::find_by_id(permission_id)
        .one(db)
        .await?
        .ok_or_else(|| ApiError::not_found("permission not found"))?;

    if perm.object_type != object_type || perm.object_id != object_id {
        return Err(ApiError::not_found("permission not found"));
    }

    object_exists(db, org_id, &perm.object_type, perm.object_id).await?;

    // Owner-tier reach with edit capability (or the original grantor) may revoke; `evaluate`
    // includes the admin bypass.
    let revoker_access = evaluate(
        db,
        user,
        edit_action_for(&perm.object_type, perm.object_id)?,
    )
    .await?;
    let is_owner = revoker_access.can_edit && revoker_access.object_role == Some(ObjectRole::Owner);
    if !is_owner && perm.grantor_id != user.id {
        return Err(ApiError::forbidden(
            "only the grantor or an admin can revoke",
        ));
    }

    access_permission::Entity::delete_by_id(permission_id)
        .exec(db)
        .await?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "permission.revoked",
        Some(perm.object_type.as_str()),
        Some(perm.object_id),
        serde_json::json!({
            "permission_id": permission_id,
            "grantee_user_id": perm.grantee_id,
            "grantee_group_id": perm.grantee_group_id,
            "access_type": perm.access_type,
        }),
    )
    .await?;

    Ok(())
}

/// Maps a generic `object_type` to the Edit-tier [`Action`] `evaluate` needs.
fn edit_action_for(object_type: &str, object_id: i64) -> Result<Action, ApiError> {
    match object_type {
        "query" => Ok(Action::Query {
            verb: QueryVerb::Edit,
            id: object_id,
        }),
        "dashboard" => Ok(Action::Dashboard {
            verb: DashboardVerb::Edit,
            id: object_id,
        }),
        "monitor" => Ok(Action::Monitor {
            verb: MonitorVerb::Edit,
            id: object_id,
        }),
        other => Err(ApiError::bad_request(format!(
            "unknown object_type: {other}"
        ))),
    }
}

/// The 404 for an object with no resolvable role; an inaccessible object and a missing/cross-org id
/// must look identical.
fn not_found_for_object_type(object_type: &str) -> ApiError {
    match object_type {
        "query" => ApiError::not_found("query not found"),
        "dashboard" => ApiError::not_found("dashboard not found"),
        "monitor" => ApiError::not_found("monitor not found"),
        other => ApiError::bad_request(format!("unknown object_type: {other}")),
    }
}

/// Confirms `object_id` is real and in `org_id`; `evaluate`'s admin path treats any id as
/// existing, so this runs ahead of the reach check to keep a nonexistent id 404ing for admins too.
async fn object_exists(
    db: &impl ConnectionTrait,
    org_id: i64,
    object_type: &str,
    object_id: i64,
) -> Result<(), ApiError> {
    let found = match object_type {
        "query" => query::Entity::find_by_id(object_id)
            .filter(query::Column::OrgId.eq(org_id))
            .one(db)
            .await?
            .is_some(),
        "dashboard" => dashboard::Entity::find_by_id(object_id)
            .filter(dashboard::Column::OrgId.eq(org_id))
            .one(db)
            .await?
            .is_some(),
        "monitor" => monitor::Entity::find_by_id(object_id)
            .filter(monitor::Column::OrgId.eq(org_id))
            .one(db)
            .await?
            .is_some(),
        _ => false,
    };
    if found {
        Ok(())
    } else {
        Err(not_found_for_object_type(object_type))
    }
}

/// Owner-tier reach with edit capability on an existing in-org object. A caller with no view reach
/// gets the uniform 404 rather than a 403 that confirms existence; any other role gets
/// `non_owner_message`.
async fn require_owner_reach(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    object_type: &str,
    object_id: i64,
    non_owner_message: &str,
) -> Result<(), ApiError> {
    object_exists(db, org_id, object_type, object_id).await?;
    let access = evaluate(db, user, edit_action_for(object_type, object_id)?).await?;
    match access.object_role {
        Some(ObjectRole::Owner) if access.can_edit => Ok(()),
        Some(ObjectRole::Owner) => Err(ApiError::forbidden(format!(
            "you need edit access to this {object_type} to share it"
        ))),
        Some(_) => Err(ApiError::forbidden(non_owner_message)),
        None => Err(not_found_for_object_type(object_type)),
    }
}
