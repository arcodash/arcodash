//! `/api/status` — system health: this org's own job counts.
//!
//! `jobs` is this org's own queued/in-flight/recent counts (mechanism + cost notes on
//! `job_service::org_job_counts`); an admin of one org never sees a cross-org aggregate.
//! `workers` stays a global aggregate (a worker process isn't
//! owned by any org), but only a count — never per-worker identity (hostname/PID), which would
//! expose operator infrastructure to a customer org admin (`job_service::WorkersStatus`).

use crate::extract::Json;
use axum::extract::State;
use serde::Serialize;

use crate::auth::{CurrentOrg, CurrentUser};
use crate::db::Db;
use crate::error::ApiError;
use crate::services::job_service::{self, OrgJobCounts, WorkersStatus};
use crate::state::AppState;

#[derive(Serialize)]
pub struct StatusResponse {
    /// Derived from `workers`: "degraded" when no worker is currently online, else "ok".
    pub status: String,
    pub jobs: OrgJobCounts,
    pub version: String,
    pub workers: WorkersStatus,
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

const fn overall_status(workers: &WorkersStatus) -> &'static str {
    if workers.online == 0 {
        "degraded"
    } else {
        "ok"
    }
}

pub async fn get(
    State(state): State<AppState>,
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    super::require_admin(&db, &caller.0).await?;

    let jobs = job_service::org_job_counts(&state.redis, Some(org.id)).await?;
    let workers = job_service::worker_status(&state.redis).await?;

    Ok(Json(StatusResponse {
        status: overall_status(&workers).to_string(),
        jobs,
        version: VERSION.to_string(),
        workers,
    }))
}

pub async fn health() -> impl axum::response::IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overall_status_reflects_worker_presence() {
        assert_eq!(overall_status(&WorkersStatus { online: 0 }), "degraded");
        assert_eq!(overall_status(&WorkersStatus { online: 1 }), "ok");
        assert_eq!(overall_status(&WorkersStatus { online: 5 }), "ok");
    }
}
