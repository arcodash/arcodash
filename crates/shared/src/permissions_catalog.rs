//! The fixed, code-defined permission catalog. The single source of truth for which permission
//! strings exist and how they're grouped/labeled in the Roles UI. Roles may only contain keys
//! present here.

#[derive(Clone, Copy, Debug)]
pub struct PermissionInfo {
    pub key: &'static str,
    pub label: &'static str,
    pub category: &'static str,
}

const fn perm(key: &'static str, label: &'static str, category: &'static str) -> PermissionInfo {
    PermissionInfo {
        key,
        label,
        category,
    }
}

/// The canonical catalog. `admin` is the single super-permission (Admin category) — it implies
/// every other key at the `user_has_permission` chokepoint, not via anything listed here.
pub const CATALOG: &[PermissionInfo] = &[
    perm("admin", "Administer workspace", "Admin"),
    // Directory read. Held by the default role because the share and assignment pickers
    // enumerate people; narrowing it breaks sharing.
    perm("view_users", "View everyone in the workspace", "Admin"),
    perm("manage_users", "Manage users", "Admin"),
    perm("view_groups", "View groups", "Admin"),
    perm("manage_groups", "Manage groups", "Admin"),
    perm("view_roles", "View roles", "Admin"),
    perm("manage_roles", "Manage roles", "Admin"),
    perm("view_dashboard", "View dashboards", "Dashboards"),
    perm("create_dashboard", "Create dashboards", "Dashboards"),
    perm("edit_dashboard", "Edit dashboards", "Dashboards"),
    perm("manage_dashboard", "Manage dashboards", "Dashboards"),
    perm("view_query", "View queries", "Queries"),
    perm("create_query", "Create queries", "Queries"),
    perm("view_snippets", "View query snippets", "Queries"),
    perm("manage_snippets", "Manage query snippets", "Queries"),
    perm("edit_query", "Edit queries", "Queries"),
    perm("execute_query", "Run queries", "Queries"),
    perm("schedule_query", "Schedule queries", "Queries"),
    perm("manage_query", "Manage queries", "Queries"),
    perm("view_data_source", "View data sources", "Data sources"),
    perm("create_data_source", "Create data sources", "Data sources"),
    perm("edit_data_source", "Edit data sources", "Data sources"),
    perm("manage_data_source", "Manage data sources", "Data sources"),
    perm("view_monitor", "View monitors", "Monitors & Alerts"),
    perm("create_monitor", "Create monitors", "Monitors & Alerts"),
    perm("edit_monitor", "Edit monitors", "Monitors & Alerts"),
    perm("manage_monitor", "Manage monitors", "Monitors & Alerts"),
    perm(
        "view_destinations",
        "View notification destinations",
        "Monitors & Alerts",
    ),
    perm(
        "manage_destinations",
        "Manage notification destinations",
        "Monitors & Alerts",
    ),
    // Two graded permissions: `create_embed` is the baseline "share outward" capability (signed
    // + org-member public links); `share_public_anonymous` is strictly stronger (truly anonymous,
    // no-sign-in public links) and is deliberately withheld from every default non-admin role
    // (see `org_seed.rs`) so a fresh org can't produce an anonymous link until an admin grants it.
    perm("create_embed", "Organization shareable links", "Sharing"),
    perm(
        "share_public_anonymous",
        "Public shareable links",
        "Sharing",
    ),
];

/// `key` grants every key in `implied` at the chokepoint. `admin` is handled separately
/// (implies-all) and is not listed here. No `manage_*` implies a delete key — there are none
/// (delete folds into edit).
pub const IMPLIES: &[(&str, &[&str])] = &[
    (
        "manage_dashboard",
        &["view_dashboard", "create_dashboard", "edit_dashboard"],
    ),
    (
        "manage_query",
        &[
            "view_query",
            "create_query",
            "edit_query",
            "execute_query",
            "schedule_query",
        ],
    ),
    (
        "manage_data_source",
        &["view_data_source", "create_data_source", "edit_data_source"],
    ),
    (
        "manage_monitor",
        &["view_monitor", "create_monitor", "edit_monitor"],
    ),
    // Inviting a user requires picking a group, so managing users is unusable without seeing
    // which groups exist and what they grant.
    ("manage_users", &["view_users", "view_groups"]),
    ("manage_groups", &["view_groups"]),
    ("manage_roles", &["view_roles"]),
    ("manage_snippets", &["view_snippets"]),
    ("manage_destinations", &["view_destinations"]),
];

/// Does holding `held` satisfy a check for `wanted`? (Excludes admin — caller short-circuits
/// admin separately.)
#[must_use]
pub fn key_implies(held: &str, wanted: &str) -> bool {
    held == wanted
        || IMPLIES
            .iter()
            .any(|(k, vs)| *k == held && vs.contains(&wanted))
}

/// Expand a raw key set to include every implied granular key (for the session payload so the
/// frontend `has()` needs no implies logic). Does NOT expand `admin` — the session sends
/// `is_admin` separately.
#[must_use]
pub fn expand_permissions(keys: &[String]) -> Vec<String> {
    let mut out: Vec<String> = keys.to_vec();
    for key in keys {
        if let Some((_, implied)) = IMPLIES.iter().find(|(k, _)| k == key) {
            out.extend(implied.iter().map(std::string::ToString::to_string));
        }
    }
    out.sort();
    out.dedup();
    out
}

#[must_use]
pub fn is_valid_permission(key: &str) -> bool {
    CATALOG.iter().any(|p| p.key == key)
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct PermissionInfoOwned {
    pub key: String,
    pub label: String,
    pub category: String,
}

#[must_use]
pub fn catalog() -> Vec<PermissionInfoOwned> {
    CATALOG
        .iter()
        .map(|p| PermissionInfoOwned {
            key: p.key.to_string(),
            label: p.label.to_string(),
            category: p.category.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_covers_the_documented_permission_set() {
        for key in [
            "admin",
            "view_users",
            "manage_users",
            "view_groups",
            "manage_groups",
            "view_roles",
            "manage_roles",
            "view_dashboard",
            "create_dashboard",
            "edit_dashboard",
            "manage_dashboard",
            "view_query",
            "create_query",
            "view_snippets",
            "manage_snippets",
            "edit_query",
            "execute_query",
            "schedule_query",
            "manage_query",
            "view_data_source",
            "create_data_source",
            "edit_data_source",
            "manage_data_source",
            "view_monitor",
            "create_monitor",
            "edit_monitor",
            "manage_monitor",
            "view_destinations",
            "manage_destinations",
            "create_embed",
            "share_public_anonymous",
        ] {
            assert!(is_valid_permission(key), "catalog missing {key}");
        }
        assert_eq!(
            CATALOG.len(),
            31,
            "catalog must contain exactly the documented keys, no more no less"
        );
    }

    #[test]
    fn removed_keys_are_no_longer_valid() {
        for key in [
            "super_admin",
            "list_users",
            "list_dashboards",
            "list_monitors",
            "view_source",
            "list_data_sources",
        ] {
            assert!(
                !is_valid_permission(key),
                "{key} must be removed from the catalog"
            );
        }
    }

    #[test]
    fn unknown_permission_is_rejected() {
        assert!(!is_valid_permission("delete_everything"));
        assert!(!is_valid_permission(""));
    }

    #[test]
    fn manage_keys_imply_their_granular_keys() {
        for (held, wanted) in [
            ("manage_query", "execute_query"),
            ("manage_users", "view_users"),
            ("manage_users", "view_groups"),
            ("manage_snippets", "view_snippets"),
            ("manage_destinations", "view_destinations"),
        ] {
            assert!(key_implies(held, wanted), "{held} must imply {wanted}");
        }
    }

    #[test]
    fn implication_does_not_cross_or_reverse() {
        for (held, wanted) in [
            ("manage_monitor", "manage_destinations"),
            ("view_snippets", "manage_snippets"),
        ] {
            assert!(!key_implies(held, wanted), "{held} must not imply {wanted}");
        }
    }

    #[test]
    fn direct_membership_implies_itself() {
        assert!(key_implies("view_query", "view_query"));
        assert!(!key_implies("view_query", "edit_query"));
    }

    #[test]
    fn expand_permissions_includes_implied_dashboard_keys() {
        let expanded = expand_permissions(&["manage_dashboard".to_string()]);
        for key in ["view_dashboard", "create_dashboard", "edit_dashboard"] {
            assert!(expanded.iter().any(|k| k == key), "missing {key}");
        }
    }

    #[test]
    fn expand_permissions_does_not_expand_admin() {
        let expanded = expand_permissions(&["admin".to_string()]);
        assert_eq!(expanded, vec!["admin".to_string()]);
    }

    #[test]
    fn expand_permissions_includes_implied_snippet_and_destination_keys() {
        let expanded = expand_permissions(&[
            "manage_snippets".to_string(),
            "manage_destinations".to_string(),
        ]);
        for key in ["view_snippets", "view_destinations"] {
            assert!(expanded.iter().any(|k| k == key), "missing {key}");
        }
    }
}
