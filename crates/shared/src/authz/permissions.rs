//! Group object permissions, and the single authorization chokepoint (`user_has_permission`).
//!
//! See the permission catalog in `arcodash_core::permissions_catalog` for the fixed key set and the
//! `manage_<category>` implies-map.
//!
//! `admin` implies every key; a held `manage_<category>` key implies every granular key in that
//! category; anything else is a direct membership check.

use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter};

use crate::entities::group::Model as Group;
use crate::entities::user::Model as User;
use crate::entities::{group, group_role, role};
use crate::permissions_catalog::key_implies;

/// `true` if `group` holds `admin`, the single super-permission that implies every other catalog
/// key at the chokepoint.
fn grants_admin(group: &Group) -> bool {
    group.permissions.iter().any(|p| p == "admin")
}

/// `true` if any of `groups` grants `admin`. The single admin check — every other admin-detection
/// copy across services/handlers routes through this.
pub fn is_admin(groups: &[Group]) -> bool {
    groups.iter().any(grants_admin)
}

/// Returns `true` if `user` has `permission`.
///
/// Either any member group grants `admin` (implies every permission), or any member group holds a
/// key that [`key_implies`] `permission` (direct membership, or a held `manage_<category>` key
/// implying a granular key in its category).
///
/// `groups` should be the full set of groups the user belongs to (typically looked up via
/// `user.groups`, the array of group ids on the `users` row); groups the user is not a member of
/// are ignored even if present in the slice.
#[must_use]
pub fn user_has_permission(user: &User, groups: &[Group], permission: &str) -> bool {
    groups
        .iter()
        .filter(|g| user.groups.contains(&g.id))
        .any(|g| {
            grants_admin(g)
                || g.permissions
                    .iter()
                    .any(|held| key_implies(held, permission))
        })
}

/// Load `user`'s groups with each group's `permissions` field replaced by the union of the
/// permissions of the roles linked to that group.
///
/// This is the single chokepoint that makes roles the source of truth for authorization, while
/// keeping `user_has_permission`'s signature stable.
pub async fn load_effective_groups(
    db: &impl ConnectionTrait,
    user: &User,
) -> Result<Vec<Group>, DbErr> {
    load_effective_groups_by_ids(db, user.org_id, &user.groups).await
}

/// Same as [`load_effective_groups`], parameterized directly by org and group ids.
///
/// Takes ids instead of a `User` row, which lets a caller compute a group set that isn't (yet, or
/// ever) any single user's membership, e.g. the groups a mutation is about to add someone to.
pub async fn load_effective_groups_by_ids(
    db: &impl ConnectionTrait,
    org_id: i64,
    group_ids: &[i64],
) -> Result<Vec<Group>, DbErr> {
    if group_ids.is_empty() {
        return Ok(vec![]);
    }
    let mut groups = group::Entity::find()
        .filter(group::Column::OrgId.eq(org_id))
        .filter(group::Column::Id.is_in(group_ids.to_vec()))
        .all(db)
        .await?;

    let group_ids: Vec<i64> = groups.iter().map(|g| g.id).collect();
    let links = group_role::Entity::find()
        .filter(group_role::Column::GroupId.is_in(group_ids))
        .all(db)
        .await?;
    let role_ids: Vec<i64> = links.iter().map(|l| l.role_id).collect();
    let roles = if role_ids.is_empty() {
        vec![]
    } else {
        role::Entity::find()
            .filter(role::Column::Id.is_in(role_ids))
            .all(db)
            .await?
    };

    for g in &mut groups {
        let mut perms: Vec<String> = links
            .iter()
            .filter(|l| l.group_id == g.id)
            .filter_map(|l| roles.iter().find(|r| r.id == l.role_id))
            .flat_map(|r| r.permissions.clone())
            .collect();
        perms.sort();
        perms.dedup();
        g.permissions = perms; // overwrite the stored column with role-derived perms
    }
    Ok(groups)
}

/// The flat set of permission keys the user effectively has (roles ∪, admin implies-all not
/// expanded).
pub async fn effective_permission_keys(
    db: &impl ConnectionTrait,
    user: &User,
) -> Result<Vec<String>, DbErr> {
    permission_keys_for_groups(db, user.org_id, &user.groups).await
}

/// The flat set of permission keys granted by `group_ids`.
///
/// Roles ∪, admin implies-all not expanded. This is the group-id-parameterized counterpart to
/// [`effective_permission_keys`], for computing what a set of groups would confer independent of
/// any one user's current membership.
pub async fn permission_keys_for_groups(
    db: &impl ConnectionTrait,
    org_id: i64,
    group_ids: &[i64],
) -> Result<Vec<String>, DbErr> {
    let groups = load_effective_groups_by_ids(db, org_id, group_ids).await?;
    let mut keys: Vec<String> = groups.into_iter().flat_map(|g| g.permissions).collect();
    keys.sort();
    keys.dedup();
    Ok(keys)
}

/// The ids of `org_id`'s groups that confer `admin` through their attached roles.
pub async fn admin_conferring_group_ids(
    db: &impl ConnectionTrait,
    org_id: i64,
) -> Result<Vec<i64>, DbErr> {
    let all_ids: Vec<i64> = group::Entity::find()
        .filter(group::Column::OrgId.eq(org_id))
        .all(db)
        .await?
        .into_iter()
        .map(|g| g.id)
        .collect();
    let groups = load_effective_groups_by_ids(db, org_id, &all_ids).await?;
    Ok(groups
        .into_iter()
        .filter(grants_admin)
        .map(|g| g.id)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{group_model, user_model};

    #[test]
    fn user_with_matching_group_permission_has_access() {
        let group = group_model(10, 1, "default", vec!["view_query".to_string()]);
        let user = user_model(1, 1, vec![10]);
        assert!(user_has_permission(&user, &[group], "view_query"));
    }

    #[test]
    fn user_without_matching_permission_lacks_access() {
        let group = group_model(10, 1, "default", vec!["view_query".to_string()]);
        let user = user_model(1, 1, vec![10]);
        assert!(!user_has_permission(&user, &[group], "create_query"));
    }

    #[test]
    fn admin_group_implies_every_permission() {
        let group = group_model(10, 1, "admin", vec!["admin".to_string()]);
        let user = user_model(1, 1, vec![10]);
        assert!(user_has_permission(
            &user,
            std::slice::from_ref(&group),
            "create_query"
        ));
        assert!(user_has_permission(&user, &[group], "anything_at_all"));
    }

    #[test]
    fn manage_query_holder_passes_execute_query() {
        let group = group_model(10, 1, "query-mgr", vec!["manage_query".to_string()]);
        let user = user_model(1, 1, vec![10]);
        assert!(user_has_permission(&user, &[group], "execute_query"));
    }

    #[test]
    fn manage_monitor_holder_fails_manage_destinations() {
        let group = group_model(10, 1, "monitor-mgr", vec!["manage_monitor".to_string()]);
        let user = user_model(1, 1, vec![10]);
        assert!(!user_has_permission(&user, &[group], "manage_destinations"));
    }

    #[test]
    fn raw_edit_data_source_holder_does_not_pass_create_data_source() {
        let group = group_model(10, 1, "ds-editor", vec!["edit_data_source".to_string()]);
        let user = user_model(1, 1, vec![10]);
        assert!(user_has_permission(
            &user,
            std::slice::from_ref(&group),
            "edit_data_source"
        ));
        assert!(!user_has_permission(&user, &[group], "create_data_source"));
    }

    #[test]
    fn view_users_only_holder_fails_manage_users() {
        let group = group_model(10, 1, "viewer", vec!["view_users".to_string()]);
        let user = user_model(1, 1, vec![10]);
        assert!(!user_has_permission(&user, &[group], "manage_users"));
    }

    #[test]
    fn groups_the_user_does_not_belong_to_are_ignored() {
        let other_groups_admin = group_model(99, 1, "admin", vec!["admin".to_string()]);
        let user = user_model(1, 1, vec![10]); // not a member of group 99
        assert!(!user_has_permission(
            &user,
            &[other_groups_admin],
            "create_query"
        ));
    }

    #[test]
    fn union_of_multiple_groups_permissions_is_checked() {
        let group_a = group_model(10, 1, "a", vec!["create_query".to_string()]);
        let group_b = group_model(11, 1, "b", vec!["edit_query".to_string()]);
        let user = user_model(1, 1, vec![10, 11]);
        assert!(user_has_permission(
            &user,
            &[group_a.clone(), group_b.clone()],
            "create_query"
        ));
        assert!(user_has_permission(
            &user,
            &[group_a, group_b],
            "edit_query"
        ));
    }

    #[test]
    fn user_in_zero_groups_has_no_permissions() {
        let group = group_model(10, 1, "default", vec!["view_query".to_string()]);
        let user = user_model(1, 1, vec![]); // member of no groups
        assert!(!user_has_permission(&user, &[group], "view_query"));
    }

    #[test]
    fn empty_permission_string_not_granted_by_group_with_no_permissions() {
        let group = group_model(10, 1, "empty", vec![]);
        let user = user_model(1, 1, vec![10]);
        assert!(!user_has_permission(&user, &[group], ""));
    }
}
