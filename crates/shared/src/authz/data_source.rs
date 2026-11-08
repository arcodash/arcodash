//! Per-group data-source visibility.
//!
//! A group is either unrestricted (sees & queries every source) or restricted to a flat allowlist.
//! A source is in a user's scope if any unrestricted group covers it or any restricted group lists
//! it. Management of an in-scope source is decided separately by the caller's data-source
//! capability, not stored here.

use crate::entities::data_source_group::Model as DataSourceGroupModel;
use crate::entities::group::Model as GroupModel;
use crate::entities::user::Model as UserModel;
use crate::permissions_catalog::key_implies;

/// One of the caller's groups paired with whether it is restricted to an allowlist.
#[derive(Clone, Copy, Debug)]
pub struct GroupPosture {
    pub group_id: i64,
    pub restricted: bool,
}

/// True if `data_source_id` is within the caller's visible scope.
#[must_use]
pub fn in_scope(
    postures: &[GroupPosture],
    data_source_id: i64,
    rows: &[DataSourceGroupModel],
) -> bool {
    postures.iter().any(|p| {
        !p.restricted
            || rows
                .iter()
                .any(|r| r.group_id == p.group_id && r.data_source_id == data_source_id)
    })
}

/// True if some group the user actually belongs to both grants `permission` (via its already-loaded
/// effective `permissions`, `key_implies`) and is itself unrestricted.
///
/// A role's grant and the group's own restriction travel together — an unrestricted group that
/// grants nothing can't be combined with a different, restricted group's role to route around that
/// group's restriction.
#[must_use]
pub fn any_unrestricted_group_grants(
    user: &UserModel,
    groups: &[GroupModel],
    permission: &str,
) -> bool {
    groups
        .iter()
        .filter(|g| user.groups.contains(&g.id))
        .any(|g| {
            !g.data_source_access_restricted
                && g.permissions
                    .iter()
                    .any(|held| key_implies(held, permission))
        })
}

/// True if some single group the user belongs to both grants `permission` and reaches
/// `data_source_id` (unrestricted, or restricted with the id on its allowlist).
///
/// Reach and the capability grant must land on the SAME group: an unrestricted group that grants
/// nothing can't lend its reach to a different, restricted group's edit grant.
#[must_use]
pub fn any_group_grants_with_reach(
    user: &UserModel,
    groups: &[GroupModel],
    permission: &str,
    data_source_id: i64,
    rows: &[DataSourceGroupModel],
) -> bool {
    groups
        .iter()
        .filter(|g| user.groups.contains(&g.id))
        .any(|g| {
            let grants = g
                .permissions
                .iter()
                .any(|held| key_implies(held, permission));
            let reaches = !g.data_source_access_restricted
                || rows
                    .iter()
                    .any(|r| r.group_id == g.id && r.data_source_id == data_source_id);
            grants && reaches
        })
}

/// Build postures from the caller's already-loaded group models.
#[must_use]
pub fn postures(groups: &[GroupModel]) -> Vec<GroupPosture> {
    groups
        .iter()
        .map(|g| GroupPosture {
            group_id: g.id,
            restricted: g.data_source_access_restricted,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{data_source_group_model, group_model_with_restriction, user_model};

    fn posture(group_id: i64, restricted: bool) -> GroupPosture {
        GroupPosture {
            group_id,
            restricted,
        }
    }

    #[test]
    fn unrestricted_group_is_in_scope_with_no_row() {
        assert!(in_scope(&[posture(10, false)], 100, &[]));
    }

    #[test]
    fn restricted_group_with_no_row_is_not_in_scope() {
        assert!(!in_scope(&[posture(10, true)], 100, &[]));
    }

    #[test]
    fn restricted_group_with_a_row_is_in_scope() {
        let dsg = data_source_group_model(1, 100, 10);
        assert!(in_scope(&[posture(10, true)], 100, &[dsg]));
    }

    #[test]
    fn union_unrestricted_group_wins_over_a_restricted_no_row_group() {
        let postures = [posture(10, true), posture(20, false)];
        assert!(in_scope(&postures, 100, &[]));
    }

    #[test]
    fn union_of_two_restricted_groups_covers_both_allowlists_not_a_third_source() {
        let dsg_a = data_source_group_model(1, 100, 10);
        let dsg_b = data_source_group_model(2, 200, 20);
        let postures = [posture(10, true), posture(20, true)];
        let rows = [dsg_a, dsg_b];
        assert!(in_scope(&postures, 100, &rows));
        assert!(in_scope(&postures, 200, &rows));
        assert!(!in_scope(&postures, 300, &rows));
    }

    #[test]
    fn restricted_with_empty_allowlist_is_not_in_scope() {
        assert!(!in_scope(&[posture(10, true)], 100, &[]));
    }

    #[test]
    fn in_scope_false_with_no_groups_at_all() {
        assert!(!in_scope(&[], 100, &[]));
    }

    #[test]
    fn same_group_grants_and_unrestricted_allows() {
        let group =
            group_model_with_restriction(10, 1, "a", vec!["create_data_source".into()], false);
        let user = user_model(1, 1, vec![10]);
        assert!(any_unrestricted_group_grants(
            &user,
            &[group],
            "create_data_source"
        ));
    }

    /// The exact gap the same-group-tied rule closes: the capability lives on a restricted
    /// group, and a wholly unrelated group is merely unrestricted. Neither group alone
    /// justifies create, so combining them across groups must not either.
    #[test]
    fn capability_on_restricted_group_and_grant_on_unrestricted_group_does_not_combine() {
        let capable_but_restricted =
            group_model_with_restriction(10, 1, "a", vec!["create_data_source".into()], true);
        let unrestricted_but_powerless = group_model_with_restriction(20, 1, "b", vec![], false);
        let user = user_model(1, 1, vec![10, 20]);
        assert!(!any_unrestricted_group_grants(
            &user,
            &[capable_but_restricted, unrestricted_but_powerless],
            "create_data_source"
        ));
    }

    #[test]
    fn capability_on_restricted_group_with_no_other_group_denies() {
        let group =
            group_model_with_restriction(10, 1, "a", vec!["create_data_source".into()], true);
        let user = user_model(1, 1, vec![10]);
        assert!(!any_unrestricted_group_grants(
            &user,
            &[group],
            "create_data_source"
        ));
    }

    #[test]
    fn manage_data_source_on_unrestricted_group_implies_create() {
        let group =
            group_model_with_restriction(10, 1, "a", vec!["manage_data_source".into()], false);
        let user = user_model(1, 1, vec![10]);
        assert!(any_unrestricted_group_grants(
            &user,
            &[group],
            "create_data_source"
        ));
    }

    #[test]
    fn non_member_group_is_ignored_even_if_qualifying() {
        let group =
            group_model_with_restriction(10, 1, "a", vec!["create_data_source".into()], false);
        let user = user_model(1, 1, vec![]); // not a member of group 10
        assert!(!any_unrestricted_group_grants(
            &user,
            &[group],
            "create_data_source"
        ));
    }

    #[test]
    fn no_groups_at_all_denies() {
        let user = user_model(1, 1, vec![]);
        assert!(!any_unrestricted_group_grants(
            &user,
            &[],
            "create_data_source"
        ));
    }

    #[test]
    fn grants_with_reach_true_when_same_unrestricted_group_grants_and_reaches() {
        let group =
            group_model_with_restriction(10, 1, "a", vec!["edit_data_source".into()], false);
        let user = user_model(1, 1, vec![10]);
        assert!(any_group_grants_with_reach(
            &user,
            &[group],
            "edit_data_source",
            100,
            &[]
        ));
    }

    /// The core gap this helper closes: capability on a restricted group whose allowlist doesn't
    /// cover the data source, plus reach only via an unrelated unrestricted group that grants
    /// nothing — neither group alone qualifies, so combining them must not either.
    #[test]
    fn grants_with_reach_false_when_capability_and_reach_are_on_different_groups() {
        let capable_but_restricted =
            group_model_with_restriction(10, 1, "a", vec!["edit_data_source".into()], true);
        let unrestricted_but_powerless = group_model_with_restriction(20, 1, "b", vec![], false);
        let user = user_model(1, 1, vec![10, 20]);
        assert!(!any_group_grants_with_reach(
            &user,
            &[capable_but_restricted, unrestricted_but_powerless],
            "edit_data_source",
            100,
            &[]
        ));
    }

    #[test]
    fn grants_with_reach_true_when_same_restricted_group_grants_and_ds_is_on_its_allowlist() {
        let group = group_model_with_restriction(10, 1, "a", vec!["edit_data_source".into()], true);
        let user = user_model(1, 1, vec![10]);
        let dsg = data_source_group_model(1, 100, 10);
        assert!(any_group_grants_with_reach(
            &user,
            &[group],
            "edit_data_source",
            100,
            &[dsg]
        ));
    }

    #[test]
    fn grants_with_reach_false_when_restricted_group_grants_but_ds_off_its_allowlist() {
        let group = group_model_with_restriction(10, 1, "a", vec!["edit_data_source".into()], true);
        let user = user_model(1, 1, vec![10]);
        assert!(!any_group_grants_with_reach(
            &user,
            &[group],
            "edit_data_source",
            100,
            &[]
        ));
    }

    #[test]
    fn grants_with_reach_true_via_key_implies_manage_data_source() {
        let group =
            group_model_with_restriction(10, 1, "a", vec!["manage_data_source".into()], false);
        let user = user_model(1, 1, vec![10]);
        assert!(any_group_grants_with_reach(
            &user,
            &[group],
            "edit_data_source",
            100,
            &[]
        ));
    }

    #[test]
    fn grants_with_reach_ignores_non_member_group() {
        let group =
            group_model_with_restriction(10, 1, "a", vec!["edit_data_source".into()], false);
        let user = user_model(1, 1, vec![]);
        assert!(!any_group_grants_with_reach(
            &user,
            &[group],
            "edit_data_source",
            100,
            &[]
        ));
    }

    #[test]
    fn grants_with_reach_false_with_no_groups_at_all() {
        let user = user_model(1, 1, vec![]);
        assert!(!any_group_grants_with_reach(
            &user,
            &[],
            "edit_data_source",
            100,
            &[]
        ));
    }

    #[test]
    fn postures_reads_restricted_flag_off_each_group_model() {
        let groups = [
            group_model_with_restriction(1, 1, "a", vec![], true),
            group_model_with_restriction(2, 1, "b", vec![], false),
        ];
        let built = postures(&groups);
        assert_eq!(built.len(), 2);
        assert!(built.iter().any(|p| p.group_id == 1 && p.restricted));
        assert!(built.iter().any(|p| p.group_id == 2 && !p.restricted));
    }
}
