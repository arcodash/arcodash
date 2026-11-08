//! `/api/jobs/<id>` — poll status / request cancellation.
//!
//! A job id is an opaque, unguessable UUID minted by `query_service::execute` and never
//! listed/enumerable, so possession of the id is itself the primary access control. It is ALSO
//! org-scoped: the Redis status record carries the submitting org's id, and
//! `job_service::{get_status,cancel}` reject a mismatch identically to "not found" — so a caller
//! of another org cannot poll/cancel a job by guessing/observing its id.

use crate::extract::{Json, Path};
use axum::extract::State;
use axum::response::IntoResponse;
use serde::Serialize;

use arcodash_core::queue::JobStatusRecord;

use crate::auth::CurrentUser;
use crate::db::Db;
use crate::error::ApiError;
use crate::services::job_service;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct JobEnvelope {
    pub job: JobResponse,
}

#[derive(Debug, Serialize)]
pub struct JobResponse {
    pub id: String,
    pub status: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_result_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn to_response(job_id: String, record: JobStatusRecord) -> JobResponse {
    JobResponse {
        id: job_id,
        status: record.status as u8,
        query_result_id: record.query_result_id,
        error: record.error,
    }
}

/// `GET /api/jobs/<id>` — poll status.
pub async fn get(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let record = job_service::get_status(&db, &state.redis, &id, &user).await?;
    Ok(Json(JobEnvelope {
        job: to_response(id, record),
    }))
}

/// `DELETE /api/jobs/<id>` — request cancellation (best-effort, see `services::job_service`).
pub async fn cancel(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    job_service::cancel(&db, &state.redis, &id, &user).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
