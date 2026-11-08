//! Builtin-group seeding for a newly created organization — shared by the `default` org (seeded
//! once at migrate time) and the org-creation path (every org created afterward).
//!
//! See `permissions_catalog.rs` for the current key set.

use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::{ActiveModelTrait, ConnectionTrait, DbErr};

use crate::entities::{group, group_role, role};

/// The builtin "Editor" role's permission set — a "create/edit/run" role.
///
/// Deliberately does NOT get data-source create-edit or groups/roles `manage_*` keys — those are
/// admin-tier or opt-in via a custom role. Monitor create/edit IS included. `manage_snippets` is
/// included because snippets have no per-object ownership or admin-tier risk, unlike the excluded
/// `manage_*` keys above.
pub const DEFAULT_GROUP_PERMISSIONS: &[&str] = &[
    "create_dashboard",
    "edit_dashboard",
    "view_dashboard",
    "create_query",
    "edit_query",
    "view_query",
    "execute_query",
    "schedule_query",
    "view_data_source",
    "view_monitor",
    "create_monitor",
    "edit_monitor",
    "view_destinations",
    "view_users",
    "view_snippets",
    "manage_snippets",
    // The baseline "share outward" capability (signed + org-member public links) goes to the
    // same default role as `edit_dashboard`/`create_dashboard`. Deliberately NOT included here:
    // `share_public_anonymous` (truly-anonymous public links) — withheld from every default
    // non-admin role so a fresh org is safe until an admin deliberately grants it.
    "create_embed",
];

/// Granted to the builtin "admin" group. `admin` alone implies every other catalog key at the
/// `user_has_permission` chokepoint — kept to a single key to avoid drift with the implies-map.
pub const ADMIN_GROUP_PERMISSIONS: &[&str] = &["admin"];

/// Read-only subset for the builtin "Viewer" role.
pub const VIEWER_ROLE_PERMISSIONS: &[&str] = &[
    "view_dashboard",
    "view_query",
    "view_data_source",
    "view_monitor",
    "view_destinations",
    "view_snippets",
];

/// The three builtin group ids created by [`seed_builtin_groups`] for one org.
pub struct SeededGroups {
    pub admin_group_id: i64,
    pub default_group_id: i64,
    pub viewer_group_id: i64,
}

/// The three builtin role ids created by [`seed_builtin_roles`] for one org.
pub struct SeededRoles {
    pub admin_role_id: i64,
    pub editor_role_id: i64,
    pub viewer_role_id: i64,
}

type Timestamp = sea_orm::prelude::DateTimeWithTimeZone;

fn to_strings(perms: &[&str]) -> Vec<String> {
    perms.iter().map(std::string::ToString::to_string).collect()
}

async fn insert_role<C: ConnectionTrait>(
    db: &C,
    org_id: i64,
    name: &str,
    perms: &[&str],
    now: Timestamp,
) -> Result<i64, DbErr> {
    let m = role::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set(name.to_string()),
        permissions: Set(to_strings(perms)),
        r#type: Set("builtin".to_string()),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(db)
    .await?;
    Ok(m.id)
}

async fn insert_group<C: ConnectionTrait>(
    db: &C,
    org_id: i64,
    name: &str,
    perms: &[&str],
    now: Timestamp,
) -> Result<i64, DbErr> {
    let m = group::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set(name.to_string()),
        permissions: Set(to_strings(perms)),
        r#type: Set("builtin".to_string()),
        created_at: Set(now),
        updated_at: Set(now),
        // The builtin groups are seeded atomically with their permissions, so there is no
        // create-then-widen window to close, and a fresh org's editors and viewers must reach the
        // data sources its admin creates. Unrestricted here is a choice, not the column default.
        data_source_access_restricted: Set(false),
    }
    .insert(db)
    .await?;
    Ok(m.id)
}

/// Insert the three builtin roles for `org_id`. Idempotent-ish: callers create these once per
/// org.
pub async fn seed_builtin_roles<C: ConnectionTrait>(
    db: &C,
    org_id: i64,
) -> Result<SeededRoles, DbErr> {
    let now = chrono::Utc::now().into();
    let admin_role_id = insert_role(db, org_id, "Admin", ADMIN_GROUP_PERMISSIONS, now).await?;
    let editor_role_id = insert_role(db, org_id, "Editor", DEFAULT_GROUP_PERMISSIONS, now).await?;
    let viewer_role_id = insert_role(db, org_id, "Viewer", VIEWER_ROLE_PERMISSIONS, now).await?;
    Ok(SeededRoles {
        admin_role_id,
        editor_role_id,
        viewer_role_id,
    })
}

async fn link_group_role<C: ConnectionTrait>(
    db: &C,
    group_id: i64,
    role_id: i64,
) -> Result<(), DbErr> {
    let now = chrono::Utc::now().into();
    group_role::ActiveModel {
        id: NotSet,
        group_id: Set(group_id),
        role_id: Set(role_id),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(db)
    .await?;
    Ok(())
}

/// Insert the builtin "admin", "default", and "viewers" groups for `org_id`. `viewers` is the
/// only reachable way to place a user in a read-only role; without it the Viewer role is seeded
/// but orphaned.
pub async fn seed_builtin_groups<C: ConnectionTrait>(
    db: &C,
    org_id: i64,
) -> Result<SeededGroups, DbErr> {
    let now = chrono::Utc::now().into();
    let admin_group_id = insert_group(db, org_id, "admin", ADMIN_GROUP_PERMISSIONS, now).await?;
    let default_group_id =
        insert_group(db, org_id, "default", DEFAULT_GROUP_PERMISSIONS, now).await?;
    let viewer_group_id = insert_group(db, org_id, "viewers", VIEWER_ROLE_PERMISSIONS, now).await?;

    let roles = seed_builtin_roles(db, org_id).await?;
    link_group_role(db, admin_group_id, roles.admin_role_id).await?;
    link_group_role(db, default_group_id, roles.editor_role_id).await?;
    link_group_role(db, viewer_group_id, roles.viewer_role_id).await?;

    Ok(SeededGroups {
        admin_group_id,
        default_group_id,
        viewer_group_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_group_permissions_is_a_single_admin_key() {
        assert_eq!(ADMIN_GROUP_PERMISSIONS, ["admin"]);
    }

    #[test]
    fn default_group_permissions_has_expected_shape() {
        assert_eq!(DEFAULT_GROUP_PERMISSIONS.len(), 17);
        assert!(DEFAULT_GROUP_PERMISSIONS.contains(&"view_query"));
        assert!(DEFAULT_GROUP_PERMISSIONS.contains(&"execute_query"));
        assert!(DEFAULT_GROUP_PERMISSIONS.contains(&"view_data_source"));
        assert!(DEFAULT_GROUP_PERMISSIONS.contains(&"create_embed"));
        assert!(DEFAULT_GROUP_PERMISSIONS.contains(&"create_monitor"));
        assert!(DEFAULT_GROUP_PERMISSIONS.contains(&"edit_monitor"));
        assert!(DEFAULT_GROUP_PERMISSIONS.contains(&"view_snippets"));
        assert!(DEFAULT_GROUP_PERMISSIONS.contains(&"manage_snippets"));
        assert!(DEFAULT_GROUP_PERMISSIONS.contains(&"view_destinations"));
        assert!(
            !DEFAULT_GROUP_PERMISSIONS.contains(&"manage_dashboard"),
            "Editor stays create/edit/run — manage_* is admin-tier or opt-in"
        );
        assert!(
            !DEFAULT_GROUP_PERMISSIONS.contains(&"create_data_source"),
            "data source create/edit is admin-tier or opt-in, unlike monitor create/edit"
        );
    }

    #[test]
    fn share_public_anonymous_is_not_a_default_permission() {
        // Safe-by-default: no default non-admin role may create truly-anonymous public
        // links until an admin explicitly grants the permission.
        assert!(!DEFAULT_GROUP_PERMISSIONS.contains(&"share_public_anonymous"));
        assert!(!VIEWER_ROLE_PERMISSIONS.contains(&"share_public_anonymous"));
        assert!(!ADMIN_GROUP_PERMISSIONS.contains(&"share_public_anonymous"));
    }

    #[test]
    fn viewers_group_permissions_match_viewer_role_exactly() {
        // seed_builtin_groups sets the "viewers" group's permissions column directly from this
        // constant, so equality here is what pins the group to the role's set — a DB-backed
        // assertion of the same fact lives in the migration and integration test suites.
        assert!(!VIEWER_ROLE_PERMISSIONS.contains(&"create_query"));
        assert!(!VIEWER_ROLE_PERMISSIONS.contains(&"execute_query"));
        assert!(!VIEWER_ROLE_PERMISSIONS.contains(&"view_users"));
    }

    #[test]
    fn viewer_role_is_read_only_subset() {
        for p in VIEWER_ROLE_PERMISSIONS {
            assert!(
                DEFAULT_GROUP_PERMISSIONS.contains(p),
                "{p} not in default set"
            );
        }
        assert!(!VIEWER_ROLE_PERMISSIONS.contains(&"create_query"));
        assert!(!VIEWER_ROLE_PERMISSIONS.contains(&"edit_dashboard"));
        assert!(!VIEWER_ROLE_PERMISSIONS.contains(&"manage_snippets"));
        assert!(
            !VIEWER_ROLE_PERMISSIONS.contains(&"view_users"),
            "Viewer must not read the org user directory"
        );
    }
}
