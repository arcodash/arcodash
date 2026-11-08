//! Central per-object sharing enforcement: one pure rule ([`object_role`]) folds ownership/admin,
//! explicit `access_permissions` grants, and `visibility` + draft state into a role for
//! queries/dashboards/monitors.
//!
//! [`object_role_for`]/[`can_view_object`] are the DB-driven wrappers all read paths go through.
//! Fail-closed everywhere: an unknown `object_type`, `visibility` or `access_type`, or a grant to a
//! group the caller isn't in, never opens access.

use std::collections::HashMap;

use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter};

use super::data_source;
use crate::entities::access_permission::Model as AccessPermissionModel;
use crate::entities::{
    access_permission, dashboard, data_source_group, group, monitor, query, user,
};

/// The maximum role a user has on an object, ordered `Viewer < Editor < Owner` so folding grants is
/// a plain `.max()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ObjectRole {
    Viewer,
    Editor,
    Owner,
}

/// Role from a deliberate grant only: ownership, admin, or an `access_permissions` row for the user
/// or one of their groups.
///
/// Excludes the org-open workspace default, because explicit access is not subject to the
/// data-source gate but the workspace default is. Pure (no I/O); an unrecognized `access_type` on a
/// grant is skipped (fail-closed).
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn explicit_role(
    user_id: i64,
    user_group_ids: &[i64],
    is_admin: bool,
    owner_id: i64,
    object_type: &str,
    object_id: i64,
    grants: &[AccessPermissionModel],
) -> Option<ObjectRole> {
    if user_id == owner_id || is_admin {
        return Some(ObjectRole::Owner);
    }

    grants
        .iter()
        .filter(|g| names_grantee(g, user_id, user_group_ids, object_type, object_id))
        .filter_map(|g| match g.access_type.as_str() {
            "owner" => Some(ObjectRole::Owner),
            // "view"/"modify" are accepted aliases for viewer/editor
            "editor" | "modify" => Some(ObjectRole::Editor),
            "viewer" | "view" => Some(ObjectRole::Viewer),
            _ => None, // fail-closed: unknown access_type contributes nothing
        })
        .max()
}

/// Whether grant `g` is on exactly `(object_type, object_id)` and names this user as grantee,
/// directly or via one of their groups. Never trusts that grants are pre-filtered to the object.
fn names_grantee(
    g: &AccessPermissionModel,
    user_id: i64,
    user_group_ids: &[i64],
    object_type: &str,
    object_id: i64,
) -> bool {
    g.object_type == object_type
        && g.object_id == object_id
        && (g.grantee_id == Some(user_id)
            || g.grantee_group_id
                .is_some_and(|gid| user_group_ids.contains(&gid)))
}

/// Whether `grants` names this user (directly or via a group) as grantee on this exact object,
/// ignoring ownership/admin; used by the `shared-with-me` filter.
#[must_use]
pub fn has_any_grant(
    user_id: i64,
    user_group_ids: &[i64],
    object_type: &str,
    object_id: i64,
    grants: &[AccessPermissionModel],
) -> bool {
    grants
        .iter()
        .any(|g| names_grantee(g, user_id, user_group_ids, object_type, object_id))
}

#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn object_role(
    user_id: i64,
    user_group_ids: &[i64],
    is_admin: bool,
    owner_id: i64,
    object_type: &str,
    object_id: i64,
    visibility: &str,
    is_draft: bool,
    grants: &[AccessPermissionModel],
) -> Option<ObjectRole> {
    explicit_role(
        user_id,
        user_group_ids,
        is_admin,
        owner_id,
        object_type,
        object_id,
        grants,
    )
    .or_else(|| (!is_draft && visibility_is_public(visibility)).then_some(ObjectRole::Viewer))
}

/// Unknown values are not public; every reader of `visibility` must agree.
#[must_use]
pub fn visibility_is_public(visibility: &str) -> bool {
    matches!(visibility, "workspace" | "public")
}

/// A query is servable to a non-editor once published and not since unpublished;
/// `query_service::is_published` delegates here.
#[must_use]
pub const fn query_is_published(q: &query::Model) -> bool {
    q.published_change_id.is_some() && q.unpublished_at.is_none()
}

/// Every `access_permissions` row on any of `object_ids` of `object_type`, in one query.
async fn grants_on(
    db: &impl ConnectionTrait,
    object_type: &str,
    object_ids: &[i64],
) -> Result<Vec<AccessPermissionModel>, DbErr> {
    access_permission::Entity::find()
        .filter(access_permission::Column::ObjectType.eq(object_type))
        .filter(access_permission::Column::ObjectId.is_in(object_ids.to_vec()))
        .all(db)
        .await
}

/// [`object_role_for`] taking already-loaded `groups`/`is_admin_user`, so batch callers load groups
/// once per request.
///
/// Fail-closed (`Ok(None)`) on an unknown `object_type` or a missing/cross-org object. Monitors
/// have no draft state (`is_draft` is false); dashboards derive it from
/// `published_snapshot.is_none()`.
pub async fn object_role_for_with_groups(
    db: &impl ConnectionTrait,
    user: &user::Model,
    // Unused: only `is_admin_user` and `user.groups` are needed; the parameter keeps every
    // `_with_groups` sibling's call shape.
    _groups: &[group::Model],
    is_admin_user: bool,
    object_type: &str,
    object_id: i64,
) -> Result<Option<ObjectRole>, DbErr> {
    let (owner_id, visibility, is_draft) = match object_type {
        "query" => match query::Entity::find_by_id(object_id)
            .filter(query::Column::OrgId.eq(user.org_id))
            .one(db)
            .await?
        {
            Some(q) => {
                let is_draft = !query_is_published(&q);
                (q.user_id, q.visibility, is_draft)
            }
            None => return Ok(None),
        },
        "dashboard" => match dashboard::Entity::find_by_id(object_id)
            .filter(dashboard::Column::OrgId.eq(user.org_id))
            .one(db)
            .await?
        {
            // A never-published dashboard (`published_snapshot IS NULL`) is the analogue of a draft
            // query.
            Some(d) => (d.user_id, d.visibility, d.published_snapshot.is_none()),
            None => return Ok(None),
        },
        "monitor" => match monitor::Entity::find_by_id(object_id)
            .filter(monitor::Column::OrgId.eq(user.org_id))
            .one(db)
            .await?
        {
            Some(m) => (m.user_id, m.visibility, false),
            None => return Ok(None),
        },
        _ => return Ok(None), // fail-closed: unknown object_type
    };

    let grants = grants_on(db, object_type, &[object_id]).await?;

    Ok(object_role(
        user.id,
        &user.groups,
        is_admin_user,
        owner_id,
        object_type,
        object_id,
        &visibility,
        is_draft,
        &grants,
    ))
}

/// Loads the caller's effective groups/admin status, then delegates to
/// [`object_role_for_with_groups`]; callers resolving several objects should load groups once and
/// call that directly.
pub async fn object_role_for(
    db: &impl ConnectionTrait,
    user: &user::Model,
    object_type: &str,
    object_id: i64,
) -> Result<Option<ObjectRole>, DbErr> {
    let groups = crate::authz::permissions::load_effective_groups(db, user).await?;
    let admin = crate::authz::permissions::is_admin(&groups);
    object_role_for_with_groups(db, user, &groups, admin, object_type, object_id).await
}

/// Roles for many objects of one `object_type` in one pass (one object query plus one grants
/// query). Every id is present in the map; missing/cross-org ids and unknown types map to `None`.
pub async fn object_roles_for(
    db: &impl ConnectionTrait,
    user: &user::Model,
    // Unused — see the same parameter on `object_role_for_with_groups`.
    _groups: &[group::Model],
    is_admin_user: bool,
    object_type: &str,
    object_ids: &[i64],
) -> Result<HashMap<i64, Option<ObjectRole>>, DbErr> {
    let mut result: HashMap<i64, Option<ObjectRole>> =
        object_ids.iter().map(|&id| (id, None)).collect();
    if object_ids.is_empty() {
        return Ok(result);
    }

    let rows: Vec<(i64, i64, String, bool)> = match object_type {
        "query" => query::Entity::find()
            .filter(query::Column::Id.is_in(object_ids.to_vec()))
            .filter(query::Column::OrgId.eq(user.org_id))
            .all(db)
            .await?
            .into_iter()
            .map(|q| {
                let is_draft = !query_is_published(&q);
                (q.id, q.user_id, q.visibility, is_draft)
            })
            .collect(),
        "dashboard" => dashboard::Entity::find()
            .filter(dashboard::Column::Id.is_in(object_ids.to_vec()))
            .filter(dashboard::Column::OrgId.eq(user.org_id))
            .all(db)
            .await?
            .into_iter()
            .map(|d| {
                (
                    d.id,
                    d.user_id,
                    d.visibility,
                    d.published_snapshot.is_none(),
                )
            })
            .collect(),
        "monitor" => monitor::Entity::find()
            .filter(monitor::Column::Id.is_in(object_ids.to_vec()))
            .filter(monitor::Column::OrgId.eq(user.org_id))
            .all(db)
            .await?
            .into_iter()
            .map(|m| (m.id, m.user_id, m.visibility, false))
            .collect(),
        _ => return Ok(result), // fail-closed: unknown object_type, every id stays None
    };

    let grants = grants_on(db, object_type, object_ids).await?;

    for (id, owner_id, visibility, is_draft) in rows {
        let role = object_role(
            user.id,
            &user.groups,
            is_admin_user,
            owner_id,
            object_type,
            id,
            &visibility,
            is_draft,
            &grants,
        );
        result.insert(id, role);
    }

    Ok(result)
}

/// Whether `user` has scope over `data_source_id`: admins bypass, otherwise any of `groups` must be
/// unrestricted or allowlist it.
async fn has_data_source_access(
    db: &impl ConnectionTrait,
    groups: &[group::Model],
    is_admin_user: bool,
    data_source_id: i64,
) -> Result<bool, DbErr> {
    if is_admin_user {
        return Ok(true);
    }
    let dsgs = data_source_group::Entity::find()
        .filter(data_source_group::Column::DataSourceId.eq(data_source_id))
        .all(db)
        .await?;
    Ok(data_source::in_scope(
        &data_source::postures(groups),
        data_source_id,
        &dsgs,
    ))
}

/// [`can_view_query_like`] taking already-loaded `groups`/`is_admin_user`. An explicit grant is
/// viewable regardless of data-source access; only the org-open workspace default is gated on it.
/// Fail-closed otherwise.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn can_view_query_like_with_groups(
    db: &impl ConnectionTrait,
    user: &user::Model,
    groups: &[group::Model],
    is_admin_user: bool,
    object_type: &str,
    object_id: i64,
    owner_id: i64,
    visibility: &str,
    is_draft: bool,
    data_source_id: i64,
) -> Result<bool, DbErr> {
    let grants = grants_on(db, object_type, &[object_id]).await?;

    if explicit_role(
        user.id,
        &user.groups,
        is_admin_user,
        owner_id,
        object_type,
        object_id,
        &grants,
    )
    .is_some()
    {
        return Ok(true);
    }

    // The remaining case is `object_role`'s tail; reusing it would re-scan grants for nothing.
    if !is_draft && visibility_is_public(visibility) {
        return has_data_source_access(db, groups, is_admin_user, data_source_id).await;
    }

    Ok(false)
}

/// Loads the caller's effective groups/admin status, then delegates to
/// [`can_view_query_like_with_groups`]. Kept for `connectors::query_results`'s SQL/Rust parity
/// test.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) async fn can_view_query_like(
    db: &impl ConnectionTrait,
    user: &user::Model,
    object_type: &str,
    object_id: i64,
    owner_id: i64,
    visibility: &str,
    is_draft: bool,
    data_source_id: i64,
) -> Result<bool, DbErr> {
    let groups = crate::authz::permissions::load_effective_groups(db, user).await?;
    let admin = crate::authz::permissions::is_admin(&groups);
    can_view_query_like_with_groups(
        db,
        user,
        &groups,
        admin,
        object_type,
        object_id,
        owner_id,
        visibility,
        is_draft,
        data_source_id,
    )
    .await
}

/// [`can_view_object`] taking already-loaded `groups`/`is_admin_user`. Queries/monitors layer the
/// data-source gate under the org-open default only; dashboards are gated purely on
/// [`object_role_for_with_groups`].
pub async fn can_view_object_with_groups(
    db: &impl ConnectionTrait,
    user: &user::Model,
    groups: &[group::Model],
    is_admin_user: bool,
    object_type: &str,
    object_id: i64,
) -> Result<bool, DbErr> {
    match object_type {
        "query" => {
            let Some(q) = query::Entity::find_by_id(object_id)
                .filter(query::Column::OrgId.eq(user.org_id))
                .one(db)
                .await?
            else {
                return Ok(false);
            };
            can_view_query_like_with_groups(
                db,
                user,
                groups,
                is_admin_user,
                "query",
                q.id,
                q.user_id,
                &q.visibility,
                !query_is_published(&q),
                q.data_source_id,
            )
            .await
        }
        "monitor" => {
            let Some(m) = monitor::Entity::find_by_id(object_id)
                .filter(monitor::Column::OrgId.eq(user.org_id))
                .one(db)
                .await?
            else {
                return Ok(false);
            };
            let Some(q) = query::Entity::find_by_id(m.query_id).one(db).await? else {
                return Ok(false);
            };
            can_view_query_like_with_groups(
                db,
                user,
                groups,
                is_admin_user,
                "monitor",
                m.id,
                m.user_id,
                &m.visibility,
                false,
                q.data_source_id,
            )
            .await
        }
        "dashboard" => Ok(object_role_for_with_groups(
            db,
            user,
            groups,
            is_admin_user,
            "dashboard",
            object_id,
        )
        .await?
        .is_some()),
        _ => Ok(false),
    }
}

/// `true` iff `user` may view `(object_type, object_id)`. Loads the caller's effective
/// groups/admin status, then delegates to [`can_view_object_with_groups`].
pub async fn can_view_object(
    db: &impl ConnectionTrait,
    user: &user::Model,
    object_type: &str,
    object_id: i64,
) -> Result<bool, DbErr> {
    let groups = crate::authz::permissions::load_effective_groups(db, user).await?;
    let admin = crate::authz::permissions::is_admin(&groups);
    can_view_object_with_groups(db, user, &groups, admin, object_type, object_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{access_permission_group, access_permission_user};

    const QUERY: &str = "query";

    // object_roles_for: the batched primitive, against real DB fixtures; skipped when
    // `TEST_DATABASE_URL` isn't set.
    mod batched_roles {
        use super::*;
        use crate::authz::permissions;
        use crate::authz::test_db::*;
        use sea_orm::{ActiveModelTrait, ActiveValue::Set, DatabaseConnection};

        /// The safety argument for [`object_roles_for`]: for every caller/ids combination, the
        /// batched map equals [`object_role_for_with_groups`] called once per id.
        #[tokio::test]
        async fn object_roles_for_matches_object_role_for_per_id_for_queries() {
            let Some(db) = setup().await else { return };
            let suffix = uuid::Uuid::new_v4();
            let org = seed_org(&db, &format!("org-orf-query-{suffix}")).await;
            let other_org = seed_org(&db, &format!("org-orf-query-other-{suffix}")).await;

            let owner = seed_user(&db, org.org_id, vec![org.default_group_id]).await;
            let caller = seed_user(&db, org.org_id, vec![org.viewer_group_id]).await;
            let admin = seed_user(&db, org.org_id, vec![org.admin_group_id]).await;
            let other_owner =
                seed_user(&db, other_org.org_id, vec![other_org.default_group_id]).await;

            let ds = seed_data_source(&db, org.org_id).await;
            let other_ds = seed_data_source(&db, other_org.org_id).await;

            let q_own = seed_query(&db, org.org_id, ds.id, caller.id, "restricted", false).await;
            let q_user_grant =
                seed_query(&db, org.org_id, ds.id, owner.id, "restricted", false).await;
            grant(
                &db,
                "query",
                q_user_grant.id,
                "editor",
                owner.id,
                Some(caller.id),
                None,
            )
            .await;
            let q_group_grant =
                seed_query(&db, org.org_id, ds.id, owner.id, "restricted", false).await;
            grant(
                &db,
                "query",
                q_group_grant.id,
                "viewer",
                owner.id,
                None,
                Some(org.viewer_group_id),
            )
            .await;
            let q_workspace_published =
                seed_query(&db, org.org_id, ds.id, owner.id, "workspace", true).await;
            let q_draft = seed_query(&db, org.org_id, ds.id, owner.id, "workspace", false).await;
            let q_restricted_no_grant =
                seed_query(&db, org.org_id, ds.id, owner.id, "restricted", true).await;
            let q_cross_org = seed_query(
                &db,
                other_org.org_id,
                other_ds.id,
                other_owner.id,
                "workspace",
                true,
            )
            .await;
            let nonexistent_id = q_cross_org.id + 9_000_000_000;

            let ids = [
                q_own.id,
                q_user_grant.id,
                q_group_grant.id,
                q_workspace_published.id,
                q_draft.id,
                q_restricted_no_grant.id,
                q_cross_org.id,
                nonexistent_id,
            ];

            for user in [&caller, &admin] {
                let groups = permissions::load_effective_groups(&db, user).await.unwrap();
                let is_admin = permissions::is_admin(&groups);

                let batched = object_roles_for(&db, user, &groups, is_admin, "query", &ids)
                    .await
                    .unwrap();
                assert_eq!(
                    batched.len(),
                    ids.len(),
                    "every requested id must appear in the map"
                );

                for &id in &ids {
                    let single =
                        object_role_for_with_groups(&db, user, &groups, is_admin, "query", id)
                            .await
                            .unwrap();
                    assert_eq!(
                        batched.get(&id).copied(),
                        Some(single),
                        "user={} id={id}: batched vs per-id disagree",
                        user.id
                    );
                }
            }
        }

        /// Lighter sibling for dashboards: owner, a grant, published/draft, cross-org, nonexistent
        /// id.
        #[tokio::test]
        async fn object_roles_for_matches_object_role_for_per_id_for_dashboards() {
            let Some(db) = setup().await else { return };
            let suffix = uuid::Uuid::new_v4();
            let org = seed_org(&db, &format!("org-orf-dash-{suffix}")).await;
            let other_org = seed_org(&db, &format!("org-orf-dash-other-{suffix}")).await;

            let owner = seed_user(&db, org.org_id, vec![org.default_group_id]).await;
            let caller = seed_user(&db, org.org_id, vec![org.viewer_group_id]).await;
            let other_owner =
                seed_user(&db, other_org.org_id, vec![other_org.default_group_id]).await;

            let d_own = seed_dashboard(&db, org.org_id, caller.id, "restricted", false).await;
            let d_grant = seed_dashboard(&db, org.org_id, owner.id, "restricted", false).await;
            grant(
                &db,
                "dashboard",
                d_grant.id,
                "viewer",
                owner.id,
                Some(caller.id),
                None,
            )
            .await;
            let d_published = seed_dashboard(&db, org.org_id, owner.id, "workspace", true).await;
            let d_unpublished = seed_dashboard(&db, org.org_id, owner.id, "workspace", false).await;
            let d_cross_org =
                seed_dashboard(&db, other_org.org_id, other_owner.id, "workspace", true).await;
            let nonexistent_id = d_cross_org.id + 9_000_000_000;

            let ids = [
                d_own.id,
                d_grant.id,
                d_published.id,
                d_unpublished.id,
                d_cross_org.id,
                nonexistent_id,
            ];

            let groups = permissions::load_effective_groups(&db, &caller)
                .await
                .unwrap();
            let is_admin = permissions::is_admin(&groups);
            let batched = object_roles_for(&db, &caller, &groups, is_admin, "dashboard", &ids)
                .await
                .unwrap();
            assert_eq!(batched.len(), ids.len());

            for &id in &ids {
                let single =
                    object_role_for_with_groups(&db, &caller, &groups, is_admin, "dashboard", id)
                        .await
                        .unwrap();
                assert_eq!(batched.get(&id).copied(), Some(single), "id={id}");
            }
        }

        /// Lighter sibling for monitors: owner, a group grant, the workspace default, cross-org,
        /// nonexistent id.
        #[tokio::test]
        async fn object_roles_for_matches_object_role_for_per_id_for_monitors() {
            let Some(db) = setup().await else { return };
            let suffix = uuid::Uuid::new_v4();
            let org = seed_org(&db, &format!("org-orf-mon-{suffix}")).await;
            let other_org = seed_org(&db, &format!("org-orf-mon-other-{suffix}")).await;

            let owner = seed_user(&db, org.org_id, vec![org.default_group_id]).await;
            let caller = seed_user(&db, org.org_id, vec![org.viewer_group_id]).await;
            let other_owner =
                seed_user(&db, other_org.org_id, vec![other_org.default_group_id]).await;

            let ds = seed_data_source(&db, org.org_id).await;
            let other_ds = seed_data_source(&db, other_org.org_id).await;
            let q = seed_query(&db, org.org_id, ds.id, owner.id, "workspace", true).await;
            let other_q = seed_query(
                &db,
                other_org.org_id,
                other_ds.id,
                other_owner.id,
                "workspace",
                true,
            )
            .await;

            let m_own = seed_monitor(&db, org.org_id, q.id, caller.id, "restricted").await;
            let m_group_grant = seed_monitor(&db, org.org_id, q.id, owner.id, "restricted").await;
            grant(
                &db,
                "monitor",
                m_group_grant.id,
                "viewer",
                owner.id,
                None,
                Some(org.viewer_group_id),
            )
            .await;
            let m_workspace = seed_monitor(&db, org.org_id, q.id, owner.id, "workspace").await;
            let m_restricted_no_grant =
                seed_monitor(&db, org.org_id, q.id, owner.id, "restricted").await;
            let m_cross_org = seed_monitor(
                &db,
                other_org.org_id,
                other_q.id,
                other_owner.id,
                "workspace",
            )
            .await;
            let nonexistent_id = m_cross_org.id + 9_000_000_000;

            let ids = [
                m_own.id,
                m_group_grant.id,
                m_workspace.id,
                m_restricted_no_grant.id,
                m_cross_org.id,
                nonexistent_id,
            ];

            let groups = permissions::load_effective_groups(&db, &caller)
                .await
                .unwrap();
            let is_admin = permissions::is_admin(&groups);
            let batched = object_roles_for(&db, &caller, &groups, is_admin, "monitor", &ids)
                .await
                .unwrap();
            assert_eq!(batched.len(), ids.len());

            for &id in &ids {
                let single =
                    object_role_for_with_groups(&db, &caller, &groups, is_admin, "monitor", id)
                        .await
                        .unwrap();
                assert_eq!(batched.get(&id).copied(), Some(single), "id={id}");
            }
        }

        #[tokio::test]
        async fn object_roles_for_on_an_empty_id_slice_issues_no_query() {
            let Some(db) = setup().await else { return };
            let suffix = uuid::Uuid::new_v4();
            let org = seed_org(&db, &format!("org-orf-empty-{suffix}")).await;
            let user = seed_user(&db, org.org_id, vec![org.default_group_id]).await;
            let groups = permissions::load_effective_groups(&db, &user)
                .await
                .unwrap();

            // `object_roles_for` returns before touching `db` when `object_ids` is empty.
            let result = object_roles_for(&db, &user, &groups, false, "query", &[])
                .await
                .unwrap();
            assert!(result.is_empty());
        }

        #[tokio::test]
        async fn object_role_for_delegates_to_the_groups_variant() {
            let Some(db) = setup().await else { return };
            let suffix = uuid::Uuid::new_v4();
            let org = seed_org(&db, &format!("org-orf-delegate-{suffix}")).await;
            let user = seed_user(&db, org.org_id, vec![org.viewer_group_id]).await;
            let ds = seed_data_source(&db, org.org_id).await;
            let q = seed_query(&db, org.org_id, ds.id, user.id, "restricted", false).await;

            let groups = permissions::load_effective_groups(&db, &user)
                .await
                .unwrap();
            let is_admin = permissions::is_admin(&groups);

            let via_wrapper = object_role_for(&db, &user, "query", q.id).await.unwrap();
            let via_groups =
                object_role_for_with_groups(&db, &user, &groups, is_admin, "query", q.id)
                    .await
                    .unwrap();
            assert_eq!(via_wrapper, via_groups);
        }

        async fn seed_group(
            db: &DatabaseConnection,
            org_id: i64,
            restricted: bool,
        ) -> group::Model {
            let suffix = uuid::Uuid::new_v4();
            let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
            group::ActiveModel {
                org_id: Set(org_id),
                name: Set(format!("orf-test-group-{suffix}")),
                permissions: Set(vec![]),
                r#type: Set("custom".to_string()),
                created_at: Set(now),
                updated_at: Set(now),
                data_source_access_restricted: Set(restricted),
                ..Default::default()
            }
            .insert(db)
            .await
            .unwrap()
        }

        /// Full cross product for `can_view_query_like_with_groups`: caller relationship x
        /// visibility x draft/published x data-source access (64 cases). Expected outcomes are
        /// derived from the rule (an explicit grant bypasses the data-source gate; only the
        /// org-open default is gated), not recorded from a prior run.
        #[tokio::test]
        async fn can_view_query_like_with_groups_matches_the_rule_across_the_full_matrix() {
            let Some(db) = setup().await else { return };
            let suffix = uuid::Uuid::new_v4();
            let org = seed_org(&db, &format!("org-cvql-matrix-{suffix}")).await;

            let open_groups = [seed_group(&db, org.org_id, false).await];
            let closed_groups = [seed_group(&db, org.org_id, true).await];

            let owner = seed_user(&db, org.org_id, vec![]).await;
            let admin_caller = seed_user(&db, org.org_id, vec![]).await;
            let grantee = seed_user(&db, org.org_id, vec![]).await;
            let stranger = seed_user(&db, org.org_id, vec![]).await;

            const DATA_SOURCE_ID: i64 = 424_242;
            let base_object_id = (suffix.as_u128() % 1_000_000_000) as i64 * 100;

            let relationships = ["owner", "admin", "explicit_grant", "no_grant"];
            let visibilities = ["workspace", "public", "restricted", "some_bogus_tier"];
            let draft_states = [true, false];
            let ds_access_states = [true, false];

            let mut case_index: i64 = 0;
            let mut total = 0;
            for &relationship in &relationships {
                for &visibility in &visibilities {
                    for &is_draft in &draft_states {
                        for &has_ds_access in &ds_access_states {
                            total += 1;
                            let object_id = base_object_id + case_index;
                            case_index += 1;

                            let groups: &[group::Model] = if has_ds_access {
                                &open_groups
                            } else {
                                &closed_groups
                            };

                            let (caller, is_admin_user) = match relationship {
                                "owner" => (&owner, false),
                                "admin" => (&admin_caller, true),
                                "explicit_grant" => {
                                    grant(
                                        &db,
                                        QUERY,
                                        object_id,
                                        "viewer",
                                        owner.id,
                                        Some(grantee.id),
                                        None,
                                    )
                                    .await;
                                    (&grantee, false)
                                }
                                "no_grant" => (&stranger, false),
                                other => panic!("unhandled relationship {other}"),
                            };

                            let expected = match relationship {
                                "owner" | "admin" | "explicit_grant" => true,
                                "no_grant" => {
                                    !is_draft
                                        && matches!(visibility, "workspace" | "public")
                                        && has_ds_access
                                }
                                other => panic!("unhandled relationship {other}"),
                            };

                            let actual = can_view_query_like_with_groups(
                                &db,
                                caller,
                                groups,
                                is_admin_user,
                                QUERY,
                                object_id,
                                owner.id,
                                visibility,
                                is_draft,
                                DATA_SOURCE_ID,
                            )
                            .await
                            .unwrap();

                            assert_eq!(
                                actual, expected,
                                "relationship={relationship} visibility={visibility} \
                                 is_draft={is_draft} has_ds_access={has_ds_access} \
                                 object_id={object_id}: expected {expected}, got {actual}"
                            );
                        }
                    }
                }
            }

            assert_eq!(total, 64, "matrix must cover exactly the claimed 64 cases");
        }
    }

    #[test]
    fn owner_is_owner() {
        assert_eq!(
            object_role(1, &[], false, 1, QUERY, 42, "workspace", false, &[]),
            Some(ObjectRole::Owner)
        );
    }

    #[test]
    fn admin_non_owner_is_owner() {
        assert_eq!(
            object_role(2, &[], true, 1, QUERY, 42, "workspace", false, &[]),
            Some(ObjectRole::Owner)
        );
    }

    #[test]
    fn direct_user_editor_grant_grants_editor() {
        let grant = access_permission_user(1, QUERY, 42, "editor", 1, 2);
        assert_eq!(
            object_role(2, &[], false, 1, QUERY, 42, "restricted", false, &[grant]),
            Some(ObjectRole::Editor)
        );
    }

    #[test]
    fn group_viewer_grant_grants_viewer() {
        let grant = access_permission_group(1, QUERY, 42, "viewer", 1, 100);
        assert_eq!(
            object_role(
                2,
                &[100],
                false,
                1,
                QUERY,
                42,
                "restricted",
                false,
                &[grant]
            ),
            Some(ObjectRole::Viewer)
        );
    }

    #[test]
    fn max_of_viewer_and_editor_grants_is_editor() {
        let viewer = access_permission_user(1, QUERY, 42, "viewer", 1, 2);
        let editor = access_permission_user(2, QUERY, 42, "editor", 1, 2);
        assert_eq!(
            object_role(
                2,
                &[],
                false,
                1,
                QUERY,
                42,
                "restricted",
                false,
                &[viewer, editor]
            ),
            Some(ObjectRole::Editor)
        );
    }

    #[test]
    fn no_grant_workspace_published_is_viewer_org_open_default() {
        assert_eq!(
            object_role(2, &[], false, 1, QUERY, 42, "workspace", false, &[]),
            Some(ObjectRole::Viewer)
        );
    }

    #[test]
    fn no_grant_workspace_draft_is_none_4a_draft_privacy() {
        assert_eq!(
            object_role(2, &[], false, 1, QUERY, 42, "workspace", true, &[]),
            None
        );
    }

    #[test]
    fn no_grant_restricted_is_none() {
        assert_eq!(
            object_role(2, &[], false, 1, QUERY, 42, "restricted", false, &[]),
            None
        );
    }

    #[test]
    fn no_grant_public_published_is_viewer() {
        assert_eq!(
            object_role(2, &[], false, 1, QUERY, 42, "public", false, &[]),
            Some(ObjectRole::Viewer)
        );
    }

    #[test]
    fn unknown_access_type_on_grant_is_ignored_fail_closed() {
        let grant = access_permission_user(1, QUERY, 42, "bogus", 1, 2);
        assert_eq!(
            object_role(2, &[], false, 1, QUERY, 42, "restricted", false, &[grant]),
            None
        );
    }

    #[test]
    fn unknown_visibility_value_is_treated_as_not_workspace_fail_closed() {
        assert_eq!(
            object_role(2, &[], false, 1, QUERY, 42, "some_bogus_tier", false, &[]),
            None
        );
    }

    #[test]
    fn visibility_is_public_accepts_only_the_two_public_tiers() {
        assert!(visibility_is_public("workspace"));
        assert!(visibility_is_public("public"));
        assert!(!visibility_is_public("restricted"));
        assert!(!visibility_is_public(""));
        assert!(!visibility_is_public("some_bogus_tier"));
    }

    #[test]
    fn group_grant_not_in_users_groups_is_ignored() {
        let grant = access_permission_group(1, QUERY, 42, "viewer", 1, 100);
        assert_eq!(
            object_role(
                2,
                &[999],
                false,
                1,
                QUERY,
                42,
                "restricted",
                false,
                &[grant]
            ),
            None
        );
    }

    #[test]
    fn grant_for_a_different_object_is_ignored() {
        let grant = access_permission_user(1, QUERY, 99, "editor", 1, 2);
        assert_eq!(
            object_role(2, &[], false, 1, QUERY, 42, "restricted", false, &[grant]),
            None
        );
    }

    #[test]
    fn public_draft_is_still_private_draft_wins_over_visibility() {
        assert_eq!(
            object_role(2, &[], false, 1, QUERY, 42, "public", true, &[]),
            None
        );
    }

    #[test]
    fn explicit_role_from_grant_ignores_visibility_and_draft() {
        // A grant confers an explicit role honored without a data-source gate, regardless of
        // visibility/draft.
        let grant = access_permission_user(1, QUERY, 42, "view", 1, 2);
        assert_eq!(
            explicit_role(2, &[], false, 1, QUERY, 42, &[grant]),
            Some(ObjectRole::Viewer)
        );
    }

    #[test]
    fn explicit_role_none_for_workspace_default() {
        // The org-open workspace default is NOT an explicit role — it must stay data-source-gated.
        assert_eq!(explicit_role(2, &[], false, 1, QUERY, 42, &[]), None);
        assert_eq!(
            object_role(2, &[], false, 1, QUERY, 42, "workspace", false, &[]),
            Some(ObjectRole::Viewer)
        );
    }

    #[test]
    fn explicit_role_owner_and_admin() {
        assert_eq!(
            explicit_role(1, &[], false, 1, QUERY, 42, &[]),
            Some(ObjectRole::Owner)
        );
        assert_eq!(
            explicit_role(2, &[], true, 1, QUERY, 42, &[]),
            Some(ObjectRole::Owner)
        );
    }
}
