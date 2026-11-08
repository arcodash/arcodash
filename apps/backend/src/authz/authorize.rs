//! The unified capability-ceiling authorization chokepoint. An [`Action`] resolves via the pure
//! [`action_spec`] to a capability key plus a required [`ObjectRole`], and one rule decides:
//!
//! ```text
//! allow = is_admin(user)
//!     OR ( has_capability(user, groups, key)            // CAPABILITY — the ceiling
//!          AND object_access(user, object) >= required ) // REACH — the object axis
//! ```
//!
//! Capability is necessary for every action; object access can only narrow within it. Create
//! actions have no object, so only capability applies; admin short-circuits both halves.
//!
//! [`authorize`] is the erroring gate; [`evaluate`] is its non-erroring twin for read-response
//! payloads.

use std::collections::HashMap;

use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};
use serde::Serialize;

use arcodash_core::entities::{data_source_group, group, user};

use crate::authz::access::{self, ObjectRole};
use crate::authz::{data_source, permissions};
use crate::error::ApiError;

/// Every verb the query family supports. `Fork` is create-shaped but needs Viewer reach on the
/// *source* query, so it carries an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryVerb {
    View,
    Edit,
    Delete,
    Revert,
    Publish,
    Restore,
    DiscardChanges,
    Execute,
    Schedule,
    Fork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardVerb {
    View,
    Edit,
    Delete,
    Share,
    RevokeShare,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorVerb {
    View,
    Edit,
    Delete,
    Mute,
    Ack,
    Subscribe,
    Unsubscribe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataSourceVerb {
    View,
    Edit,
    Delete,
    Test,
    Pause,
    Resume,
    RefreshSchema,
}

/// Widgets/visualizations/results inherit their parent's object access; only two tiers exist: read,
/// or create/edit/delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InheritedVerb {
    View,
    Edit,
}

/// Names an action for [`authorize`]/[`evaluate`]: object family, verb, and the target id (absent
/// only for the four `*Create` variants), so illegal states are unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Query {
        verb: QueryVerb,
        id: i64,
    },
    QueryCreate,
    Dashboard {
        verb: DashboardVerb,
        id: i64,
    },
    DashboardCreate,
    Monitor {
        verb: MonitorVerb,
        id: i64,
    },
    MonitorCreate,
    DataSource {
        verb: DataSourceVerb,
        id: i64,
    },
    DataSourceCreate,
    /// `dashboard_id` is the parent dashboard the widget belongs to (or will belong to).
    Widget {
        verb: InheritedVerb,
        dashboard_id: i64,
    },
    /// `query_id` is the parent query the visualization belongs to (or will belong to).
    Visualization {
        verb: InheritedVerb,
        query_id: i64,
    },
    Result {
        verb: InheritedVerb,
        query_id: i64,
    },
}

/// The object family an [`Action`] resolves to; `None` means create-shaped (no object to check
/// reach against).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjectRef {
    None,
    Query(i64),
    Dashboard(i64),
    Monitor(i64),
    DataSource(i64),
}

/// The pure result of resolving an [`Action`]: capability key, required [`ObjectRole`] (absent for
/// creates), and the object to check reach against.
struct ActionSpec {
    key: &'static str,
    required: Option<ObjectRole>,
    object: ObjectRef,
}

const fn query_tier(verb: QueryVerb) -> (&'static str, ObjectRole) {
    use QueryVerb::{
        Delete, DiscardChanges, Edit, Execute, Fork, Publish, Restore, Revert, Schedule, View,
    };
    match verb {
        View => ("view_query", ObjectRole::Viewer),
        Edit | Revert | Publish | Restore | DiscardChanges => ("edit_query", ObjectRole::Editor),
        Delete => ("edit_query", ObjectRole::Owner),
        Execute => ("execute_query", ObjectRole::Viewer),
        Schedule => ("schedule_query", ObjectRole::Editor),
        Fork => ("create_query", ObjectRole::Viewer),
    }
}

const fn dashboard_tier(verb: DashboardVerb) -> (&'static str, ObjectRole) {
    use DashboardVerb::{Delete, Edit, RevokeShare, Share, View};
    // RevokeShare has Edit's capability∩reach floor, but `dashboard_service::revoke_share` adds a
    // creator-or-admin check (`embed_service::require_can_share_object`) on top.
    match verb {
        View => ("view_dashboard", ObjectRole::Viewer),
        Edit | Share | RevokeShare => ("edit_dashboard", ObjectRole::Editor),
        Delete => ("edit_dashboard", ObjectRole::Owner),
    }
}

const fn monitor_tier(verb: MonitorVerb) -> (&'static str, ObjectRole) {
    use MonitorVerb::{Ack, Delete, Edit, Mute, Subscribe, Unsubscribe, View};
    match verb {
        View | Ack | Subscribe | Unsubscribe => ("view_monitor", ObjectRole::Viewer),
        Edit | Mute => ("edit_monitor", ObjectRole::Editor),
        Delete => ("edit_monitor", ObjectRole::Owner),
    }
}

const fn data_source_tier(verb: DataSourceVerb) -> (&'static str, ObjectRole) {
    use DataSourceVerb::{Delete, Edit, Pause, RefreshSchema, Resume, Test, View};
    match verb {
        View => ("view_data_source", ObjectRole::Viewer),
        Edit | Delete | Test | Pause | Resume | RefreshSchema => {
            ("edit_data_source", ObjectRole::Editor)
        }
    }
}

/// Pure action -> (capability key, required role, object) resolution; no I/O.
fn action_spec(action: &Action) -> ActionSpec {
    let create = |key| ActionSpec {
        key,
        required: None,
        object: ObjectRef::None,
    };
    let ((key, required), object) = match *action {
        Action::QueryCreate => return create("create_query"),
        Action::DashboardCreate => return create("create_dashboard"),
        Action::MonitorCreate => return create("create_monitor"),
        Action::DataSourceCreate => return create("create_data_source"),
        Action::Query { verb, id } => (query_tier(verb), ObjectRef::Query(id)),
        Action::Dashboard { verb, id } => (dashboard_tier(verb), ObjectRef::Dashboard(id)),
        Action::Monitor { verb, id } => (monitor_tier(verb), ObjectRef::Monitor(id)),
        Action::DataSource { verb, id } => (data_source_tier(verb), ObjectRef::DataSource(id)),
        Action::Widget { verb, dashboard_id } => {
            let verb = match verb {
                InheritedVerb::View => DashboardVerb::View,
                InheritedVerb::Edit => DashboardVerb::Edit,
            };
            (dashboard_tier(verb), ObjectRef::Dashboard(dashboard_id))
        }
        Action::Visualization { verb, query_id } | Action::Result { verb, query_id } => {
            let verb = match verb {
                InheritedVerb::View => QueryVerb::View,
                InheritedVerb::Edit => QueryVerb::Edit,
            };
            (query_tier(verb), ObjectRef::Query(query_id))
        }
    };
    ActionSpec {
        key,
        required: Some(required),
        object,
    }
}

/// The pure ceiling rule: capability is necessary for every action; when `required` is `Some`,
/// object access must also clear it. Admin bypasses both halves.
fn allow(
    is_admin: bool,
    has_capability: bool,
    required: Option<ObjectRole>,
    object_role: Option<ObjectRole>,
) -> bool {
    is_admin
        || required.map_or(has_capability, |required| {
            has_capability && object_role.is_some_and(|r| r >= required)
        })
}

/// [`allow`]'s query edit-tier ceiling for callers with batch-loaded `groups` and an `object_role`,
/// avoiding an N+1 per row. Must match `evaluate(Action::Query { verb: QueryVerb::Edit, id })`.
pub(crate) fn can_edit_query(
    user: &user::Model,
    groups: &[group::Model],
    is_admin: bool,
    object_role: Option<ObjectRole>,
) -> bool {
    allow(
        is_admin,
        permissions::user_has_permission(user, groups, "edit_query"),
        Some(ObjectRole::Editor),
        object_role,
    )
}

/// Data sources have no object-role axis: reach is binary in-scope ([`data_source::in_scope`]),
/// mapped to `Editor`; the `edit_data_source`/`manage_data_source` capability check in `allow()`
/// distinguishes view from manage.
async fn data_source_object_role(
    db: &impl ConnectionTrait,
    groups: &[group::Model],
    data_source_id: i64,
) -> Result<Option<ObjectRole>, ApiError> {
    let dsgs = data_source_group::Entity::find()
        .filter(data_source_group::Column::DataSourceId.eq(data_source_id))
        .all(db)
        .await?;
    Ok(
        data_source::in_scope(&data_source::postures(groups), data_source_id, &dsgs)
            .then_some(ObjectRole::Editor),
    )
}

/// Object roles prefetched for one list operation, keyed `(object_type, object_id)` by
/// [`prefetch_roles`]. A miss falls back to a per-object lookup, so an incomplete cache costs
/// performance, never correctness.
pub type ObjectRoleCache = HashMap<(&'static str, i64), Option<ObjectRole>>;

/// Resolves an [`ObjectRef`] to the caller's [`ObjectRole`]: query, dashboard and monitor refs hit
/// `roles` first, then [`access::object_role_for_with_groups`]; data sources always use
/// [`data_source_object_role`]; `ObjectRef::None` has nothing to check.
async fn object_access(
    db: &impl ConnectionTrait,
    user: &user::Model,
    groups: &[group::Model],
    is_admin: bool,
    roles: &ObjectRoleCache,
    object: ObjectRef,
) -> Result<Option<ObjectRole>, ApiError> {
    let (object_type, id) = match object {
        ObjectRef::None => return Ok(None),
        ObjectRef::DataSource(id) => return data_source_object_role(db, groups, id).await,
        ObjectRef::Query(id) => ("query", id),
        ObjectRef::Dashboard(id) => ("dashboard", id),
        ObjectRef::Monitor(id) => ("monitor", id),
    };
    if let Some(&cached) = roles.get(&(object_type, id)) {
        return Ok(cached);
    }
    Ok(access::object_role_for_with_groups(db, user, groups, is_admin, object_type, id).await?)
}

/// Denial status: 404 for object-bound actions (existence stays hidden), 403 for a capability
/// failure on a create action (no object to hide).
fn deny(spec: &ActionSpec) -> ApiError {
    match spec.object {
        ObjectRef::None => ApiError::forbidden(format!("missing the {} permission", spec.key)),
        ObjectRef::Query(_) => ApiError::not_found("query not found"),
        ObjectRef::Dashboard(_) => ApiError::not_found("dashboard not found"),
        ObjectRef::Monitor(_) => ApiError::not_found("monitor not found"),
        ObjectRef::DataSource(_) => ApiError::not_found("data source not found"),
    }
}

/// [`decide`] with already-loaded `groups`/`is_admin` and a prefetched `roles` cache (empty if
/// none), so list responses pay for group loading once per request and reach queries once per
/// batch.
async fn decide_with_groups(
    db: &impl ConnectionTrait,
    user: &user::Model,
    groups: &[group::Model],
    is_admin: bool,
    roles: &ObjectRoleCache,
    spec: &ActionSpec,
) -> Result<EvaluateResult, ApiError> {
    let object_role = if is_admin {
        (spec.object != ObjectRef::None).then_some(ObjectRole::Owner)
    } else {
        object_access(db, user, groups, is_admin, roles, spec.object).await?
    };

    // Edit-tier data-source actions need the capability grant and reach on the same group; the
    // generic formula treats them as independent and would let a restricted group borrow reach from
    // another.
    let can_edit = match spec.object {
        ObjectRef::DataSource(id) if !is_admin && spec.key == "edit_data_source" => {
            let dsgs = data_source_group::Entity::find()
                .filter(data_source_group::Column::DataSourceId.eq(id))
                .all(db)
                .await?;
            data_source::any_group_grants_with_reach(user, groups, "edit_data_source", id, &dsgs)
        }
        _ => allow(
            is_admin,
            permissions::user_has_permission(user, groups, spec.key),
            spec.required,
            object_role,
        ),
    };

    Ok(EvaluateResult {
        object_role,
        can_edit,
    })
}

/// Loads the facts [`allow`] needs and applies it. Admin skips the `object_access` round-trip
/// (admin is `Owner` on any object); always uses an empty [`ObjectRoleCache`].
async fn decide(
    db: &impl ConnectionTrait,
    user: &user::Model,
    spec: &ActionSpec,
) -> Result<EvaluateResult, ApiError> {
    let groups = permissions::load_effective_groups(db, user).await?;
    let is_admin = permissions::is_admin(&groups);
    decide_with_groups(db, user, &groups, is_admin, &ObjectRoleCache::new(), spec).await
}

/// The single erroring gate: `Ok(())` iff the ceiling rule allows `action` for `user`.
pub async fn authorize(
    db: &impl ConnectionTrait,
    user: &user::Model,
    action: Action,
) -> Result<(), ApiError> {
    let spec = action_spec(&action);
    let allowed = decide(db, user, &spec).await?.can_edit;
    crate::metrics::record_authz_decision(spec.key, allowed);
    if allowed {
        return Ok(());
    }
    // The only trace a denial leaves beyond the uniform 404/403; `evaluate*` stay silent since
    // "no access" is normal for per-row `caller` blocks.
    tracing::warn!(
        user_id = user.id,
        org_id = user.org_id,
        capability = spec.key,
        object = ?spec.object,
        "authz denied"
    );
    Err(deny(&spec))
}

/// The caller-block booleans for a read response without allocating an error per action. `action`
/// should be the object's Edit-tier action; `can_edit` is what `authorize` would decide for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluateResult {
    pub object_role: Option<ObjectRole>,
    pub can_edit: bool,
}

pub async fn evaluate(
    db: &impl ConnectionTrait,
    user: &user::Model,
    action: Action,
) -> Result<EvaluateResult, ApiError> {
    decide(db, user, &action_spec(&action)).await
}

/// [`evaluate`] with already-loaded `groups`/`is_admin` and a prefetched `roles` cache, for per-row
/// `caller` blocks. A cache miss falls back to the per-object lookup, so it changes cost, never the
/// answer.
pub async fn evaluate_with_roles(
    db: &impl ConnectionTrait,
    user: &user::Model,
    groups: &[group::Model],
    is_admin: bool,
    roles: &ObjectRoleCache,
    action: Action,
) -> Result<EvaluateResult, ApiError> {
    decide_with_groups(db, user, groups, is_admin, roles, &action_spec(&action)).await
}

/// [`evaluate`] with already-loaded `groups`/`is_admin`; the per-object reach check still runs per
/// row unless [`evaluate_with_roles`] is used.
pub async fn evaluate_with_groups(
    db: &impl ConnectionTrait,
    user: &user::Model,
    groups: &[group::Model],
    is_admin: bool,
    action: Action,
) -> Result<EvaluateResult, ApiError> {
    evaluate_with_roles(db, user, groups, is_admin, &ObjectRoleCache::new(), action).await
}

/// Builds an [`ObjectRoleCache`] for every id in `ids` (one `object_type`) in one batched lookup,
/// for [`evaluate_with_roles`].
pub async fn prefetch_roles(
    db: &impl ConnectionTrait,
    user: &user::Model,
    groups: &[group::Model],
    is_admin: bool,
    object_type: &'static str,
    ids: &[i64],
) -> Result<ObjectRoleCache, ApiError> {
    let roles = access::object_roles_for(db, user, groups, is_admin, object_type, ids).await?;
    Ok(roles
        .into_iter()
        .map(|(id, role)| ((object_type, id), role))
        .collect())
}

/// `caller` block on an object read response: the effective role plus pre-evaluated action booleans
/// for UI buttons, so no client-side math can drift from enforcement.
///
/// `can_delete` needs Owner-tier reach on top of `can_edit`; `can_share` exists only on dashboards.
#[derive(Debug, Clone, Serialize)]
pub struct Caller {
    pub object_role: &'static str,
    pub can_edit: bool,
    pub can_delete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_share: Option<bool>,
    /// Query-only: whether the caller can run this query (capability∩reach AND data-source reach);
    /// omitted for non-runnable families.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_execute: Option<bool>,
    /// Query-only: why execute is blocked: `"capability"` | `"data_source"` | `"both"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execute_blocked_reason: Option<&'static str>,
}

impl Default for Caller {
    /// The no-access caller, used as a DTO's initial value before a handler overwrites it.
    fn default() -> Self {
        Self {
            object_role: "none",
            can_edit: false,
            can_delete: false,
            can_share: None,
            can_execute: None,
            execute_blocked_reason: None,
        }
    }
}

impl From<EvaluateResult> for Caller {
    fn from(r: EvaluateResult) -> Self {
        let object_role = match r.object_role {
            Some(ObjectRole::Owner) => "owner",
            Some(ObjectRole::Editor) => "editor",
            Some(ObjectRole::Viewer) => "viewer",
            None => "none",
        };
        Self {
            object_role,
            can_edit: r.can_edit,
            // Non-admin delete = edit capability ∩ Owner-tier reach, both already in
            // `EvaluateResult`; admin is true via evaluate's bypass.
            can_delete: r.can_edit && r.object_role == Some(ObjectRole::Owner),
            ..Self::default()
        }
    }
}

impl Caller {
    /// Dashboards additionally expose `can_share`: the `create_embed` capability AND
    /// creator-or-admin reach (`embed_service::require_can_share_object`), stricter than
    /// `can_edit`. The caller computes both inputs.
    #[must_use]
    pub const fn with_share(mut self, has_create_embed: bool, is_creator_or_admin: bool) -> Self {
        self.can_share = Some(has_create_embed && is_creator_or_admin);
        self
    }

    /// Queries additionally expose `can_execute` (Execute-tier decision AND data-source reach);
    /// `execute_blocked_reason` says which half failed without leaking role/group structure.
    #[must_use]
    pub const fn with_execute(mut self, capability_ok: bool, data_source_ok: bool) -> Self {
        self.can_execute = Some(capability_ok && data_source_ok);
        self.execute_blocked_reason = match (capability_ok, data_source_ok) {
            (true, true) => None,
            (false, true) => Some("capability"),
            (true, false) => Some("data_source"),
            (false, false) => Some("both"),
        };
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcodash_core::test_support::{access_permission_user, group_model, user_model};

    #[test]
    fn query_spec_matrix() {
        let cases = [
            (QueryVerb::View, "view_query", ObjectRole::Viewer),
            (QueryVerb::Edit, "edit_query", ObjectRole::Editor),
            (QueryVerb::Delete, "edit_query", ObjectRole::Owner),
            (QueryVerb::Revert, "edit_query", ObjectRole::Editor),
            (QueryVerb::Publish, "edit_query", ObjectRole::Editor),
            (QueryVerb::Restore, "edit_query", ObjectRole::Editor),
            (QueryVerb::DiscardChanges, "edit_query", ObjectRole::Editor),
            (QueryVerb::Execute, "execute_query", ObjectRole::Viewer),
            (QueryVerb::Schedule, "schedule_query", ObjectRole::Editor),
            (QueryVerb::Fork, "create_query", ObjectRole::Viewer),
        ];
        for (verb, key, required) in cases {
            let spec = action_spec(&Action::Query { verb, id: 42 });
            assert_eq!(spec.key, key);
            assert_eq!(spec.required, Some(required));
            assert_eq!(spec.object, ObjectRef::Query(42));
        }
    }

    #[test]
    fn dashboard_spec_matrix() {
        let cases = [
            (DashboardVerb::View, "view_dashboard", ObjectRole::Viewer),
            (DashboardVerb::Edit, "edit_dashboard", ObjectRole::Editor),
            (DashboardVerb::Delete, "edit_dashboard", ObjectRole::Owner),
            (DashboardVerb::Share, "edit_dashboard", ObjectRole::Editor),
            (
                DashboardVerb::RevokeShare,
                "edit_dashboard",
                ObjectRole::Editor,
            ),
        ];
        for (verb, key, required) in cases {
            let spec = action_spec(&Action::Dashboard { verb, id: 7 });
            assert_eq!(spec.key, key);
            assert_eq!(spec.required, Some(required));
            assert_eq!(spec.object, ObjectRef::Dashboard(7));
        }
    }

    #[test]
    fn monitor_spec_matrix() {
        let cases = [
            (MonitorVerb::View, "view_monitor", ObjectRole::Viewer),
            (MonitorVerb::Edit, "edit_monitor", ObjectRole::Editor),
            (MonitorVerb::Delete, "edit_monitor", ObjectRole::Owner),
            (MonitorVerb::Mute, "edit_monitor", ObjectRole::Editor),
            (MonitorVerb::Ack, "view_monitor", ObjectRole::Viewer),
            (MonitorVerb::Subscribe, "view_monitor", ObjectRole::Viewer),
            (MonitorVerb::Unsubscribe, "view_monitor", ObjectRole::Viewer),
        ];
        for (verb, key, required) in cases {
            let spec = action_spec(&Action::Monitor { verb, id: 3 });
            assert_eq!(spec.key, key);
            assert_eq!(spec.required, Some(required));
            assert_eq!(spec.object, ObjectRef::Monitor(3));
        }
    }

    #[test]
    fn data_source_spec_matrix() {
        let cases = [
            (DataSourceVerb::View, "view_data_source", ObjectRole::Viewer),
            (DataSourceVerb::Edit, "edit_data_source", ObjectRole::Editor),
            (
                DataSourceVerb::Delete,
                "edit_data_source",
                ObjectRole::Editor,
            ),
            (DataSourceVerb::Test, "edit_data_source", ObjectRole::Editor),
            (
                DataSourceVerb::Pause,
                "edit_data_source",
                ObjectRole::Editor,
            ),
            (
                DataSourceVerb::Resume,
                "edit_data_source",
                ObjectRole::Editor,
            ),
            (
                DataSourceVerb::RefreshSchema,
                "edit_data_source",
                ObjectRole::Editor,
            ),
        ];
        for (verb, key, required) in cases {
            let spec = action_spec(&Action::DataSource { verb, id: 9 });
            assert_eq!(spec.key, key);
            assert_eq!(spec.required, Some(required));
            assert_eq!(spec.object, ObjectRef::DataSource(9));
        }
    }

    #[test]
    fn creates_are_capability_only() {
        for (action, key) in [
            (Action::QueryCreate, "create_query"),
            (Action::DashboardCreate, "create_dashboard"),
            (Action::MonitorCreate, "create_monitor"),
            (Action::DataSourceCreate, "create_data_source"),
        ] {
            let spec = action_spec(&action);
            assert_eq!(spec.key, key);
            assert_eq!(spec.required, None);
            assert_eq!(spec.object, ObjectRef::None);
        }
    }

    #[test]
    fn caller_with_execute_reason_matrix() {
        let cases = [
            (true, true, Some(true), None),
            (false, true, Some(false), Some("capability")),
            (true, false, Some(false), Some("data_source")),
            (false, false, Some(false), Some("both")),
        ];
        for (capability_ok, data_source_ok, can_execute, reason) in cases {
            let caller = Caller::default().with_execute(capability_ok, data_source_ok);
            assert_eq!(
                caller.can_execute, can_execute,
                "cap={capability_ok} ds={data_source_ok}"
            );
            assert_eq!(
                caller.execute_blocked_reason, reason,
                "cap={capability_ok} ds={data_source_ok}"
            );
        }
    }

    #[test]
    fn widget_inherits_parent_dashboard_action() {
        let view = action_spec(&Action::Widget {
            verb: InheritedVerb::View,
            dashboard_id: 11,
        });
        assert_eq!(view.key, "view_dashboard");
        assert_eq!(view.required, Some(ObjectRole::Viewer));
        assert_eq!(view.object, ObjectRef::Dashboard(11));

        let edit = action_spec(&Action::Widget {
            verb: InheritedVerb::Edit,
            dashboard_id: 11,
        });
        assert_eq!(edit.key, "edit_dashboard");
        assert_eq!(edit.required, Some(ObjectRole::Editor));
        assert_eq!(edit.object, ObjectRef::Dashboard(11));
    }

    #[test]
    fn visualization_inherits_parent_query_action() {
        let view = action_spec(&Action::Visualization {
            verb: InheritedVerb::View,
            query_id: 21,
        });
        assert_eq!(view.key, "view_query");
        assert_eq!(view.required, Some(ObjectRole::Viewer));
        assert_eq!(view.object, ObjectRef::Query(21));

        let edit = action_spec(&Action::Visualization {
            verb: InheritedVerb::Edit,
            query_id: 21,
        });
        assert_eq!(edit.key, "edit_query");
        assert_eq!(edit.required, Some(ObjectRole::Editor));
        assert_eq!(edit.object, ObjectRef::Query(21));
    }

    #[test]
    fn result_inherits_parent_query_action() {
        let view = action_spec(&Action::Result {
            verb: InheritedVerb::View,
            query_id: 22,
        });
        assert_eq!(view.key, "view_query");
        assert_eq!(view.object, ObjectRef::Query(22));

        let edit = action_spec(&Action::Result {
            verb: InheritedVerb::Edit,
            query_id: 22,
        });
        assert_eq!(edit.key, "edit_query");
        assert_eq!(edit.object, ObjectRef::Query(22));
    }

    #[test]
    fn admin_bypasses_everything() {
        assert!(allow(true, false, Some(ObjectRole::Editor), None));
        assert!(allow(true, false, None, None));
    }

    #[test]
    fn owner_without_capability_is_denied() {
        // Owner satisfies any reach requirement, but capability is still required.
        assert!(!allow(
            false,
            false,
            Some(ObjectRole::Editor),
            Some(ObjectRole::Owner)
        ));
    }

    #[test]
    fn grant_without_capability_is_denied() {
        // An Editor grant clears reach but capability still gates.
        assert!(!allow(
            false,
            false,
            Some(ObjectRole::Editor),
            Some(ObjectRole::Editor)
        ));
    }

    #[test]
    fn capability_without_reach_is_denied() {
        assert!(!allow(false, true, Some(ObjectRole::Editor), None));
        assert!(!allow(
            false,
            true,
            Some(ObjectRole::Editor),
            Some(ObjectRole::Viewer)
        ));
    }

    #[test]
    fn capability_and_sufficient_reach_is_allowed() {
        assert!(allow(
            false,
            true,
            Some(ObjectRole::Editor),
            Some(ObjectRole::Editor)
        ));
        assert!(allow(
            false,
            true,
            Some(ObjectRole::Editor),
            Some(ObjectRole::Owner)
        ));
        assert!(allow(
            false,
            true,
            Some(ObjectRole::Viewer),
            Some(ObjectRole::Viewer)
        ));
    }

    #[test]
    fn create_is_capability_only_no_object_access_call() {
        assert!(allow(false, true, None, None));
        assert!(!allow(false, false, None, None));
        assert!(allow(false, true, None, Some(ObjectRole::Viewer)));
    }

    #[test]
    fn canonical_example_view_only_role_with_modify_grant_cannot_edit() {
        // A view_dashboard-only role with a modify grant must still be denied edit: capability wins
        // over a share grant.
        let group = group_model(1, 1, "viewer-role", vec!["view_dashboard".to_string()]);
        let user = user_model(2, 1, vec![1]);
        let grant = access_permission_user(1, "dashboard", 42, "modify", 1, 2);

        let has_edit_capability =
            permissions::user_has_permission(&user, &[group], "edit_dashboard");
        let object_role = access::object_role(
            2,
            &[],
            false,
            1,
            "dashboard",
            42,
            "restricted",
            false,
            &[grant],
        );

        assert_eq!(object_role, Some(ObjectRole::Editor));
        assert!(!allow(
            false,
            has_edit_capability,
            Some(ObjectRole::Editor),
            object_role
        ));

        let group_for_view = group_model(1, 1, "viewer-role", vec!["view_dashboard".to_string()]);
        let has_view_capability =
            permissions::user_has_permission(&user, &[group_for_view], "view_dashboard");
        assert!(allow(
            false,
            has_view_capability,
            Some(ObjectRole::Viewer),
            object_role
        ));
    }

    #[test]
    fn owner_ceiling_example_owner_without_edit_capability_is_denied() {
        // a stale ownership row is never a standing bypass of a revoked capability.
        let group = group_model(1, 1, "viewer-role", vec!["view_monitor".to_string()]);
        let user = user_model(1, 1, vec![1]);
        let object_role =
            access::object_role(1, &[], false, 1, "monitor", 5, "restricted", false, &[]);
        assert_eq!(object_role, Some(ObjectRole::Owner));

        let has_edit_capability = permissions::user_has_permission(&user, &[group], "edit_monitor");
        assert!(!allow(
            false,
            has_edit_capability,
            Some(ObjectRole::Editor),
            object_role
        ));
    }

    #[test]
    fn caller_object_role_string_mapping() {
        for (role, expected) in [
            (Some(ObjectRole::Owner), "owner"),
            (Some(ObjectRole::Editor), "editor"),
            (Some(ObjectRole::Viewer), "viewer"),
            (None, "none"),
        ] {
            let caller = Caller::from(EvaluateResult {
                object_role: role,
                can_edit: true,
            });
            assert_eq!(caller.object_role, expected);
        }
    }

    #[test]
    fn caller_can_delete_requires_owner_reach_on_top_of_can_edit() {
        for can_edit in [true, false] {
            let caller = Caller::from(EvaluateResult {
                object_role: Some(ObjectRole::Owner),
                can_edit,
            });
            assert_eq!(caller.can_delete, can_edit);
        }

        // Editor reach with can_edit can edit but not delete.
        let editor_caller = Caller::from(EvaluateResult {
            object_role: Some(ObjectRole::Editor),
            can_edit: true,
        });
        assert!(editor_caller.can_edit);
        assert!(!editor_caller.can_delete);
    }

    #[test]
    fn caller_can_share_omitted_unless_with_share_called() {
        let base = Caller::from(EvaluateResult {
            object_role: Some(ObjectRole::Editor),
            can_edit: true,
        });
        assert_eq!(base.can_share, None);
    }

    /// The four `(has_create_embed, is_creator_or_admin)` combinations: `can_share` requires both.
    #[test]
    fn with_share_requires_both_create_embed_and_creator_or_admin() {
        let caller = || {
            Caller::from(EvaluateResult {
                object_role: Some(ObjectRole::Editor),
                can_edit: true,
            })
        };
        assert_eq!(caller().with_share(true, true).can_share, Some(true));
        assert_eq!(caller().with_share(true, false).can_share, Some(false));
        assert_eq!(caller().with_share(false, true).can_share, Some(false));
        assert_eq!(caller().with_share(false, false).can_share, Some(false));
    }

    #[test]
    fn caller_default_is_the_no_access_value() {
        let caller = Caller::default();
        assert_eq!(caller.object_role, "none");
        assert!(!caller.can_edit && !caller.can_delete);
        assert_eq!(caller.can_share, None);
    }

    /// A prefetch-cache hit must be *used*: seed a role the disconnected `db` could never produce
    /// and confirm `evaluate_with_roles` returns it.
    #[tokio::test]
    async fn evaluate_with_roles_uses_a_prefetched_role() {
        let db = sea_orm::DatabaseConnection::default();
        let group = group_model(1, 1, "viewer-role", vec!["view_query".to_string()]);
        let user = user_model(2, 1, vec![1]);
        let groups = [group];

        let mut roles = ObjectRoleCache::new();
        roles.insert(("query", 999), Some(ObjectRole::Owner));

        let result = evaluate_with_roles(
            &db,
            &user,
            &groups,
            false,
            &roles,
            Action::Query {
                verb: QueryVerb::View,
                id: 999,
            },
        )
        .await
        .expect("a cache hit must never reach the (disconnected) database");

        assert_eq!(result.object_role, Some(ObjectRole::Owner));
        assert!(
            result.can_edit,
            "view_query capability + cached Owner role clears the Viewer bar"
        );
    }

    /// A DB-backed reach decision with an empty cache behaves as [`evaluate_with_groups`]. Skips if
    /// `TEST_DATABASE_URL` is unset.
    mod prefetched_roles {
        use super::*;
        use arcodash_core::entities::{data_source, organization, query};
        use arcodash_core::org_seed::seed_builtin_groups;
        use sea_orm::{
            ActiveModelTrait, ActiveValue::Set, DatabaseConnection, DbBackend, Statement,
        };

        async fn setup() -> Option<DatabaseConnection> {
            let database_url = std::env::var("TEST_DATABASE_URL").ok()?;
            Some(arcodash_core::db::connect(&database_url).await.unwrap())
        }

        struct OrgFixture {
            org_id: i64,
            viewer_group_id: i64,
        }

        async fn seed_org(db: &DatabaseConnection, slug: &str) -> OrgFixture {
            let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
            let org = organization::ActiveModel {
                name: Set(slug.to_string()),
                slug: Set(slug.to_string()),
                settings: Set(serde_json::json!({})),
                created_at: Set(now),
                updated_at: Set(now),
                ..Default::default()
            }
            .insert(db)
            .await
            .unwrap();
            let groups = seed_builtin_groups(db, org.id).await.unwrap();
            OrgFixture {
                org_id: org.id,
                viewer_group_id: groups.viewer_group_id,
            }
        }

        async fn seed_user(db: &DatabaseConnection, org_id: i64, groups: Vec<i64>) -> user::Model {
            let suffix = uuid::Uuid::new_v4();
            let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
            user::ActiveModel {
                org_id: Set(org_id),
                name: Set("authorize-test-user".to_string()),
                email: Set(format!("authorize-test-{suffix}@test.local")),
                password_hash: Set(None),
                groups: Set(groups),
                api_key_hash: Set(arcodash_core::hash::hash_api_key(&suffix.to_string())),
                disabled_at: Set(None),
                is_invitation_pending: Set(false),
                profile_image_url: Set(None),
                details: Set(serde_json::json!({})),
                created_at: Set(now),
                updated_at: Set(now),
                ..Default::default()
            }
            .insert(db)
            .await
            .unwrap()
        }

        async fn seed_data_source(db: &DatabaseConnection, org_id: i64) -> data_source::Model {
            let suffix = uuid::Uuid::new_v4();
            let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
            data_source::ActiveModel {
                org_id: Set(org_id),
                name: Set(format!("authorize-test-ds-{suffix}")),
                r#type: Set("pg".to_string()),
                encrypted_options: Set(None),
                scheduled_queue_name: Set(None),
                schema: Set(None),
                schema_refreshed_at: Set(None),
                paused_at: Set(None),
                pause_reason: Set(None),
                created_at: Set(now),
                updated_at: Set(now),
                ..Default::default()
            }
            .insert(db)
            .await
            .unwrap()
        }

        /// Published + `workspace` visibility, so a non-owner has reach without a grant.
        async fn seed_published_query(
            db: &DatabaseConnection,
            org_id: i64,
            ds_id: i64,
            owner_id: i64,
        ) -> query::Model {
            let suffix = uuid::Uuid::new_v4();
            let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
            let q = query::ActiveModel {
                org_id: Set(org_id),
                data_source_id: Set(ds_id),
                name: Set(format!("authorize-test-query-{suffix}")),
                description: Set(None),
                query: Set("SELECT 1".to_string()),
                query_hash: Set(format!("hash-{suffix}")),
                api_key: Set(suffix.to_string()),
                user_id: Set(owner_id),
                last_modified_by_id: Set(None),
                latest_query_data_id: Set(None),
                is_archived: Set(false),
                visibility: Set("workspace".to_string()),
                options: Set(serde_json::json!({})),
                tags: Set(None),
                last_execution_status: Set(String::new()),
                last_execution_at: Set(None),
                row_count: Set(None),
                last_execution_error: Set(None),
                created_at: Set(now),
                updated_at: Set(now),
                ..Default::default()
            }
            .insert(db)
            .await
            .unwrap();

            let change_row = db
                .query_one_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "INSERT INTO changes (object_type, object_id, user_id, change, created_at, updated_at) \
                     VALUES ('query', $1, $2, $3, now(), now()) RETURNING id",
                    [q.id.into(), owner_id.into(), serde_json::json!({}).into()],
                ))
                .await
                .unwrap()
                .unwrap();
            let change_id: i64 = change_row.try_get("", "id").unwrap();
            db.execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE queries SET published_change_id = $1, published_at = now() WHERE id = $2",
                [change_id.into(), q.id.into()],
            ))
            .await
            .unwrap();

            query::Entity::find_by_id(q.id)
                .one(db)
                .await
                .unwrap()
                .unwrap()
        }

        #[tokio::test]
        async fn evaluate_with_roles_falls_back_on_a_cache_miss() {
            let Some(db) = setup().await else {
                eprintln!("skipping: TEST_DATABASE_URL not set");
                return;
            };
            let suffix = uuid::Uuid::new_v4();
            let org = seed_org(&db, &format!("org-ewr-miss-{suffix}")).await;
            let owner = seed_user(&db, org.org_id, vec![]).await;
            let viewer = seed_user(&db, org.org_id, vec![org.viewer_group_id]).await;
            let ds = seed_data_source(&db, org.org_id).await;
            let q = seed_published_query(&db, org.org_id, ds.id, owner.id).await;

            let groups = permissions::load_effective_groups(&db, &viewer)
                .await
                .unwrap();
            let is_admin = permissions::is_admin(&groups);
            let action = Action::Query {
                verb: QueryVerb::View,
                id: q.id,
            };

            let via_groups = evaluate_with_groups(&db, &viewer, &groups, is_admin, action)
                .await
                .unwrap();
            let via_empty_cache = evaluate_with_roles(
                &db,
                &viewer,
                &groups,
                is_admin,
                &ObjectRoleCache::new(),
                action,
            )
            .await
            .unwrap();

            assert_eq!(via_groups, via_empty_cache);
            assert_eq!(via_groups.object_role, Some(ObjectRole::Viewer));
            assert!(via_groups.can_edit);
        }
    }
}
