//! Authorization layers, enforced server-side: group object permissions, data-source Full/View-only
//! checks, and per-object `access_permissions` sharing.
//!
//! Most functions here are pure over already-fetched data (no DB calls inside), so they're
//! unit-testable in isolation; the exception is
//! `permissions::load_effective_groups`/`effective_permission_keys`, the single chokepoint that
//! loads a user's groups and resolves each group's effective (role-derived) permissions — every
//! group-load call site routes through it so roles stay the source of truth.
//!
//! `data_source::in_scope`/`any_unrestricted_group_grants` and `permissions::user_has_permission`
//! are wired into `services::data_source_service`, `services::query_service`,
//! `services::result_service` and the `data_sources`/`groups` handlers. A group's reach is binary
//! (in scope or not) — the query-vs-manage distinction comes entirely from the caller's capability,
//! not from a stored per-row tier. `access::object_role`/`can_view_object` (per-object
//! `access_permissions` sharing + `visibility`) are the central enforcement chokepoint wired into
//! every query/dashboard/monitor `get`/`list` path.
//!
//! `authorize::authorize`/`evaluate` unify the capability layer and this object-access layer into
//! one capability-ceiling rule (capability AND reach, never reach alone) — see `authorize`'s
//! module doc.

pub mod access;
pub mod authorize;
pub mod data_source;
pub mod org_access;
pub mod permissions;
