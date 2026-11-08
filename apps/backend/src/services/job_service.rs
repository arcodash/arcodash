//! Job status: reads/cancels jobs tracked in Redis by `arcodash_core::queue`. No HTTP types here.

use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use sea_orm::ConnectionTrait;
use serde::{Deserialize, Serialize};

use arcodash_core::entities::user;
use arcodash_core::events::record_event;
use arcodash_core::queue::{self, JobStatus, JobStatusRecord};
use arcodash_core::redis_util::scan_keys;

use crate::error::ApiError;

/// `true` iff `user` may act on `record`: the submitter, an admin, or anyone in the org if the
/// record has no `user_id`.
async fn may_act_on(
    db: &impl ConnectionTrait,
    user: &user::Model,
    record: &JobStatusRecord,
) -> Result<bool, ApiError> {
    match record.user_id {
        Some(submitter) if submitter == user.id => Ok(true),
        Some(_) => crate::services::user_service::is_admin_user(db, user).await,
        None => Ok(true),
    }
}

/// `GET /api/jobs/<id>`: status, or 404 if unknown/expired or owned by another org or user (uniform
/// 404 so ids can't be probed).
pub async fn get_status(
    db: &impl ConnectionTrait,
    redis: &ConnectionManager,
    job_id: &str,
    user: &user::Model,
) -> Result<JobStatusRecord, ApiError> {
    let record = queue::get_job_status(redis, job_id)
        .await
        .map_err(|e| ApiError::internal(format!("failed to read job status: {e}")))?
        .ok_or_else(|| ApiError::not_found("job not found"))?;
    if record.org_id != user.org_id || !may_act_on(db, user, &record).await? {
        return Err(ApiError::not_found("job not found"));
    }
    Ok(record)
}

/// `DELETE /api/jobs/<id>`: best-effort cancel (only effective before the worker starts the job);
/// marks the job CANCELLED immediately if non-terminal.
///
/// Ownership is checked before flipping the flag; mismatches are 404.
pub async fn cancel(
    db: &impl ConnectionTrait,
    redis: &ConnectionManager,
    job_id: &str,
    user: &user::Model,
) -> Result<(), ApiError> {
    get_status(db, redis, job_id, user).await?;

    queue::request_cancellation(redis, job_id)
        .await
        .map_err(|e| ApiError::internal(format!("failed to request job cancellation: {e}")))?;

    // Atomic in one Redis round trip, so a worker's terminal write can't be clobbered.
    queue::set_cancelled_if_not_terminal(redis, job_id)
        .await
        .map_err(|e| ApiError::internal(format!("failed to mark job cancelled: {e}")))?;

    record_event(
        db,
        user.org_id,
        Some(user.id),
        "job.cancelled",
        Some("job"),
        None,
        serde_json::json!({ "job_id": job_id }),
    )
    .await?;

    Ok(())
}

/// This org's own job counts, for `GET /api/status`.
#[derive(Debug, Default, Serialize)]
pub struct OrgJobCounts {
    pub queued: i64,
    pub in_flight: i64,
    pub recent_success: i64,
    pub recent_failure: i64,
    pub recent_cancelled: i64,
}

/// Count jobs by status.
///
/// `Some(org_id)` scopes to one org (never cross-org totals); `None` counts all, for the
/// operator-only `/metrics` gauge. Scans the small TTL'd set of `job:<id>` records; revisit with a
/// per-org counter if it grows large.
pub async fn org_job_counts(
    redis: &ConnectionManager,
    org_id: Option<i64>,
) -> Result<OrgJobCounts, ApiError> {
    let mut conn = redis.clone();
    let keys: Vec<String> = scan_keys(&mut conn, "job:*")
        .await
        .map_err(|e| ApiError::internal(format!("failed to list job keys: {e}")))?;

    let mut counts = OrgJobCounts::default();
    // Exclude the separate `job:<id>:cancel` flag keys (plain "1", not a JSON job record).
    for key in keys.iter().filter(|k| !k.ends_with(":cancel")) {
        // One GET per key, not MGET: cluster mode rejects the cross-slot command.
        let raw: Option<String> = conn
            .get(key)
            .await
            .map_err(|e| ApiError::internal(format!("failed to read job records: {e}")))?;
        let Some(record) = raw.and_then(|raw| serde_json::from_str::<JobStatusRecord>(&raw).ok())
        else {
            continue;
        };
        if org_id.is_some_and(|id| record.org_id != id) {
            continue;
        }
        match record.status {
            JobStatus::Pending => counts.queued += 1,
            JobStatus::Started => counts.in_flight += 1,
            JobStatus::Success => counts.recent_success += 1,
            JobStatus::Failure => counts.recent_failure += 1,
            JobStatus::Cancelled => counts.recent_cancelled += 1,
        }
    }

    Ok(counts)
}

/// Worker fleet liveness: a count only, never per-worker identity, since workers aren't owned by
/// any org.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkersStatus {
    /// Number of live `worker:<id>` heartbeat keys (30s TTL).
    pub online: usize,
}

/// Count of live worker processes from `worker:<id>` heartbeat keys; exposes no job content or
/// worker ids.
pub async fn worker_status(redis: &ConnectionManager) -> Result<WorkersStatus, ApiError> {
    let mut conn = redis.clone();
    let keys: Vec<String> = scan_keys(&mut conn, "worker:*")
        .await
        .map_err(|e| ApiError::internal(format!("failed to list worker keys: {e}")))?;
    Ok(WorkersStatus { online: keys.len() })
}
