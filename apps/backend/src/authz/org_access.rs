//! Extension point for a per-request org access gate (e.g. an entitlement check).
//! `AppState::org_access_check` is `None` by default, so this is inert unless a deployment sets it.

use arcodash_core::entities::organization;

use crate::error::ApiError;

/// A deployment can plug in a per-request org gate.
///
/// Checked once per request, right after the org resolves. `Ok(())` allows the request through;
/// `Err(ApiError)` short-circuits it before any request transaction opens. `path` lets an impl
/// exempt its own always-reachable routes (e.g. session/logout/org-switch) without a resolution
/// change.
pub trait OrgAccessCheck: Send + Sync {
    fn check(&self, org: &organization::Model, path: &str) -> Result<(), ApiError>;
}
