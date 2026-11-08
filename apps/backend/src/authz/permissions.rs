//! Group object permissions and the single authorization chokepoint (`user_has_permission`).
//!
//! See `arcodash_core::permissions_catalog` for the key set and `manage_<category>` implies-map:
//! `admin` implies every key, `manage_<category>` implies that category's granular keys. The core
//! helpers live in `arcodash_core::authz::permissions` (re-exported below); what stays here builds
//! an `ApiError`.

use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};

use arcodash_core::entities::user::Model as User;
use arcodash_core::entities::{access_permission, data_source_group, user};

pub use arcodash_core::authz::permissions::*;

use crate::authz::access::{self, ObjectRole};
use crate::authz::data_source;
use crate::error::ApiError;

/// Loads `user`'s effective groups and enforces `permission` via the chokepoint, else 403.
pub async fn require_permission(
    db: &impl ConnectionTrait,
    user: &User,
    permission: &str,
) -> Result<(), ApiError> {
    let groups = load_effective_groups(db, user).await?;
    if user_has_permission(user, &groups, permission) {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!(
            "missing the {permission} permission"
        )))
    }
}

/// Confirm `caller` holds every key in `granted` before they hand it to someone else, preventing
/// privilege escalation via role/group grants. The admin bypass is inherent in
/// `user_has_permission`.
pub async fn require_can_grant(
    db: &impl ConnectionTrait,
    caller: &User,
    granted: &[String],
) -> Result<(), ApiError> {
    require_holds_all(db, caller, granted, "grant").await
}

/// Confirm `caller` holds every key they are about to take away (mirror of [`require_can_grant`]).
/// `revoked` must be the set difference, not the whole old list.
pub async fn require_can_revoke(
    db: &impl ConnectionTrait,
    caller: &User,
    revoked: &[String],
) -> Result<(), ApiError> {
    require_holds_all(db, caller, revoked, "revoke").await
}

/// 403 naming every key in `keys` that `caller` does not hold; `verb` is "grant" or "revoke".
async fn require_holds_all(
    db: &impl ConnectionTrait,
    caller: &User,
    keys: &[String],
    verb: &str,
) -> Result<(), ApiError> {
    let caller_groups = load_effective_groups(db, caller).await?;
    let missing: Vec<&str> = keys
        .iter()
        .map(String::as_str)
        .filter(|k| !user_has_permission(caller, &caller_groups, k))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(ApiError::forbidden(format!(
        "cannot {verb} permissions you do not hold: {}",
        missing.join(", ")
    )))
}

/// `true` if any projection (a user's post-change groups, or `None` if disabled/removed) still
/// intersects `admin_groups`. The pure rule under [`require_admin_remains`].
fn any_projection_confers_admin(
    admin_groups: &[i64],
    projections: impl IntoIterator<Item = Option<Vec<i64>>>,
) -> bool {
    projections
        .into_iter()
        .any(|p| p.is_some_and(|groups| groups.iter().any(|g| admin_groups.contains(g))))
}

/// The error string is asserted by the frontend and tests; do not change it.
const LAST_ADMIN_MESSAGE: &str = "cannot remove the last admin from the organization";

/// O(users) scan on a rare admin-mutation path; use an indexed count if orgs grow large.
async fn enabled_org_users(
    db: &impl ConnectionTrait,
    org_id: i64,
) -> Result<Vec<User>, sea_orm::DbErr> {
    user::Entity::find()
        .filter(user::Column::OrgId.eq(org_id))
        .filter(user::Column::DisabledAt.is_null())
        .all(db)
        .await
}

/// `true` if any of `users` belongs to one of `admin_groups`.
fn any_user_in_admin_groups(admin_groups: &[i64], users: &[User]) -> bool {
    any_projection_confers_admin(admin_groups, users.iter().map(|u| Some(u.groups.clone())))
}

/// Errors when the org would be left with no enabled admin.
///
/// `admin_groups` is the post-change set, used only to evaluate `projected_groups`; whether the
/// change is blamed is judged against the pre-change admin conferral, re-queried here. Returns
/// `had_admin_before` so a post-write recheck ([`require_admin_survived`]) can reuse the pre-write
/// fact.
pub async fn require_admin_remains(
    db: &impl ConnectionTrait,
    org_id: i64,
    admin_groups: &[i64],
    projected_groups: impl Fn(&User) -> Option<Vec<i64>>,
) -> Result<bool, ApiError> {
    let users = enabled_org_users(db, org_id).await?;

    // Only enforce "stays >= 1" if the org had an enabled admin before, so it isn't blamed for an
    // existing deficit.
    let current_admin_groups = admin_conferring_group_ids(db, org_id).await?;
    if !any_user_in_admin_groups(&current_admin_groups, &users) {
        return Ok(false);
    }
    if any_projection_confers_admin(admin_groups, users.iter().map(&projected_groups)) {
        Ok(true)
    } else {
        Err(ApiError::bad_request(LAST_ADMIN_MESSAGE))
    }
}

/// Post-write counterpart to [`require_admin_remains`].
///
/// `had_admin_before` must come from the matching pre-write call; a fresh read now would compare
/// the rows to themselves. Errors if the org had an admin and has none now.
pub async fn require_admin_survived(
    db: &impl ConnectionTrait,
    org_id: i64,
    had_admin_before: bool,
) -> Result<(), ApiError> {
    if !had_admin_before {
        return Ok(());
    }
    let admin_groups = admin_conferring_group_ids(db, org_id).await?;
    if any_user_in_admin_groups(&admin_groups, &enabled_org_users(db, org_id).await?) {
        Ok(())
    } else {
        Err(ApiError::bad_request(LAST_ADMIN_MESSAGE))
    }
}

/// The ceiling for handing someone membership of `group_ids`: the caller must hold every permission
/// those groups confer, every object grant addressed to them, and reach on every data source they
/// open up.
pub async fn require_can_grant_groups(
    db: &impl ConnectionTrait,
    caller: &User,
    org_id: i64,
    group_ids: &[i64],
) -> Result<(), ApiError> {
    if group_ids.is_empty() {
        return Ok(());
    }

    let target_groups = load_effective_groups_by_ids(db, org_id, group_ids).await?;

    // Dedupe the raw request ids too, or a repeated valid id (e.g. [7, 7]) reads as unknown.
    let mut requested: Vec<i64> = group_ids.to_vec();
    requested.sort_unstable();
    requested.dedup();

    // Ids naming no row or a row in another org are rejected, not dropped: the array is read as an
    // authorization input elsewhere.
    if target_groups.len() != requested.len() {
        let resolved: std::collections::HashSet<i64> = target_groups.iter().map(|g| g.id).collect();
        let unknown: Vec<String> = requested
            .iter()
            .filter(|id| !resolved.contains(id))
            .map(i64::to_string)
            .collect();
        return Err(ApiError::bad_request(format!(
            "unknown group id(s): {}",
            unknown.join(", ")
        )));
    }

    let mut conferred: Vec<String> = target_groups
        .iter()
        .flat_map(|g| g.permissions.clone())
        .collect();
    conferred.sort();
    conferred.dedup();
    require_can_grant(db, caller, &conferred).await?;

    let caller_groups = load_effective_groups(db, caller).await?;
    if is_admin(&caller_groups) {
        return Ok(());
    }

    let caller_rows = data_source_group::Entity::find()
        .filter(data_source_group::Column::GroupId.is_in(caller.groups.clone()))
        .all(db)
        .await?;
    let caller_postures = data_source::postures(&caller_groups);
    let caller_has_unrestricted = caller_postures.iter().any(|p| !p.restricted);
    let target_rows = data_source_group::Entity::find()
        .filter(data_source_group::Column::GroupId.is_in(group_ids.to_vec()))
        .all(db)
        .await?;
    for g in &target_groups {
        if !g.data_source_access_restricted {
            if !caller_has_unrestricted {
                return Err(ApiError::forbidden(
                    "cannot grant unrestricted data-source access you do not hold",
                ));
            }
            continue;
        }
        let unreachable = target_rows
            .iter()
            .filter(|r| r.group_id == g.id)
            .any(|r| !data_source::in_scope(&caller_postures, r.data_source_id, &caller_rows));
        if unreachable {
            return Err(ApiError::forbidden(
                "cannot grant access to a data source you cannot reach",
            ));
        }
    }

    let grants = access_permission::Entity::find()
        .filter(access_permission::Column::GranteeGroupId.is_in(group_ids.to_vec()))
        .all(db)
        .await?;
    for grant in &grants {
        let granted_role = match grant.access_type.as_str() {
            "owner" => ObjectRole::Owner,
            "editor" | "modify" => ObjectRole::Editor,
            "viewer" | "view" => ObjectRole::Viewer,
            _ => continue, // fail-closed, matches `explicit_role`'s convention
        };
        // N round trips on a rare path; batch if group ACL counts grow.
        let caller_role =
            access::object_role_for(db, caller, &grant.object_type, grant.object_id).await?;
        if caller_role.is_none_or(|r| r < granted_role) {
            return Err(ApiError::forbidden(
                "cannot grant access to an object you cannot reach at that level",
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_admin_disabled_leaves_no_admin() {
        assert!(!any_projection_confers_admin(&[10], [None]));
    }

    #[test]
    fn second_admin_present_is_ok() {
        assert!(any_projection_confers_admin(&[10], [None, Some(vec![10])]));
    }

    #[test]
    fn projection_of_none_for_the_only_admin_leaves_no_admin() {
        assert!(!any_projection_confers_admin(&[10], [None]));
    }

    #[test]
    fn group_demoted_out_of_the_admin_set_leaves_no_admin() {
        assert!(!any_projection_confers_admin(&[], [Some(vec![10])]));
    }

    #[test]
    fn admin_survives_when_membership_keeps_an_admin_group() {
        assert!(any_projection_confers_admin(&[10, 20], [Some(vec![20])]));
    }

    #[test]
    fn no_users_at_all_leaves_no_admin() {
        assert!(!any_projection_confers_admin(
            &[10],
            Vec::<Option<Vec<i64>>>::new()
        ));
    }
}
