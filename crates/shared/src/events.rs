//! Audit events (`events` table) — shared by the backend (`middleware::events::record_event`
//! delegates here) and the worker (job success/failure events from `execute_query`).
//!
//! Lives in `crates/shared` since backend and worker are separate binaries that both need to insert
//! the same row shape.

use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::{ConnectionTrait, EntityTrait};
use serde_json::Value;

use crate::entities::event;

/// The `events.action` value written when a monitor transitions between states.
///
/// Stored verbatim and matched verbatim: `apps/worker`'s digest "Changes" section and notification
/// observed-value lookup both filter `events.action` on this exact string in SQL, so changing it
/// silently empties both rather than failing. The API presents it under a different name via the
/// backend's action map, which is where a display rename belongs.
pub const MONITOR_STATE_CHANGED: &str = "monitor_state_changed";

/// Insert one `events` row.
///
/// `user_id`/`object_type`/`object_id` are optional since some actions (e.g. a failed login with an
/// unknown email, or a job tied to a deleted query) have no associated user or object.
/// `additional_properties` is free-form JSON for action-specific context.
///
/// Callers must pick one of two dispositions for the returned `Result`, never swallow it silently:
///
/// - **Propagate (`?`)** when the call runs on a request transaction (an `apps/backend` handler
///   or service fn whose caller returns `ApiError`, driven by an RLS per-request
///   transaction). A failed `INSERT` there poisons that transaction regardless of what the call
///   site does with the error — every later statement in the request fails with "transaction is
///   aborted" either way. Swallowing the error only throws away the one clue to what actually
///   broke; propagating surfaces it as the response instead.
/// - **Log and continue** everywhere else: the worker, an owner pool (no request
///   transaction to poison), `after_commit` closures, and any call on a path that is already
///   about to return a different error (recording that failure must never mask it).
pub async fn record_event(
    db: &impl ConnectionTrait,
    org_id: i64,
    user_id: Option<i64>,
    action: &str,
    object_type: Option<&str>,
    object_id: Option<i64>,
    additional_properties: Value,
) -> Result<(), sea_orm::DbErr> {
    let model = event::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        user_id: Set(user_id),
        action: Set(action.to_string()),
        object_type: Set(object_type.map(str::to_string)),
        object_id: Set(object_id),
        additional_properties: Set(additional_properties),
        created_at: NotSet,
        updated_at: NotSet,
    };

    event::Entity::insert(model)
        .exec(db)
        .await
        // `debug!`, not `warn!`: every caller is contractually required to propagate or log this
        // already (see the doc above), so a second `warn!` here would double each failure.
        .inspect_err(|e| tracing::debug!(org_id, action, error = %e, "record_event: insert failed"))
        .map(drop)
}
