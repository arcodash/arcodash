//! `GET /api/runs`, `GET /api/runs/:id/result.csv` — run history + per-run CSV download.
//!
//! Runs have no access rule of their own; every call is gated by
//! `schedule_service::authorize_entity` on the run's `(entity_type, entity_id)`.

use crate::extract::{Json, Path};
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};

use arcodash_core::entities::{run, user};
use arcodash_core::events::record_event;

use crate::auth::CurrentUser;
use crate::db::Db;
use crate::error::ApiError;
use crate::services::job_service;
use crate::services::run_service::{self, NotifiedDto, RunRow};
use crate::services::schedule_service::{self, EntityAction};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct RunResponse {
    pub id: i64,
    pub status: String,
    pub triggered_by: String,
    pub scheduled_for: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
    pub row_count: Option<i64>,
    pub error: Option<String>,
    pub data_age_secs: Option<i64>,
    pub waiting_on_run_id: Option<i64>,
    pub query_result_id: Option<i64>,
    /// Monitor runs only: whether the checked condition was met. `None` for a query run, a
    /// failed-to-check run, or a run recorded without this value.
    pub condition_met: Option<bool>,
    /// Monitor runs only; `[]` for a query run or a monitor run that sent nothing.
    pub notified: Vec<NotifiedDto>,
}

impl RunResponse {
    /// Build a response for a just-inserted run with no `notified` fan-out yet (used by the
    /// `queries`/`monitors` `run` handlers — a run they just created has no deliveries).
    #[must_use]
    pub fn from_model(r: run::Model) -> Self {
        Self::from(RunRow {
            run: r,
            notified: Vec::new(),
        })
    }
}

impl From<RunRow> for RunResponse {
    fn from(row: RunRow) -> Self {
        let r = row.run;
        Self {
            id: r.id,
            status: r.status,
            triggered_by: r.triggered_by,
            scheduled_for: r.scheduled_for.map(|t| t.with_timezone(&Utc)),
            started_at: r.started_at.map(|t| t.with_timezone(&Utc)),
            finished_at: r.finished_at.map(|t| t.with_timezone(&Utc)),
            duration_ms: r.duration_ms,
            row_count: r.row_count,
            error: r.error,
            data_age_secs: r.data_age_secs,
            waiting_on_run_id: r.waiting_on_run_id,
            query_result_id: r.query_result_id,
            condition_met: r.condition_met,
            notified: row.notified,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ListRunsQuery {
    pub entity_type: String,
    pub entity_id: i64,
}

/// `GET /api/runs?entity_type=&entity_id=` — always entity-scoped, ≤10 rows newest first.
pub async fn list(
    db: Db,
    CurrentUser(user): CurrentUser,
    Query(p): Query<ListRunsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    schedule_service::authorize_entity(&db, &user, &p.entity_type, p.entity_id, EntityAction::Read)
        .await?;
    let rows = run_service::list(&db, user.org_id, &p.entity_type, p.entity_id).await?;
    Ok(Json(
        rows.into_iter().map(RunResponse::from).collect::<Vec<_>>(),
    ))
}

/// `DELETE /api/runs/:id` — requests cancellation of a run started through the runs pipeline.
///
/// Best-effort, like `DELETE /api/jobs/:id`: only effective while the worker has not started the
/// job. Deliberately does not accept/return a bare `job_id` on `RunResponse` — a job id is an
/// unguessable UUID that acts as a bearer capability for `DELETE /api/jobs/:id`, and
/// `job_service::may_act_on` allows any org member to act on a scheduler-originated job (whose
/// status record has no `user_id`); publishing it on every list row would hand cancellation
/// rights to anyone with mere Read reach on the entity. This route re-derives the same
/// entity-scoped `RunNow` check every other run mutation uses instead.
pub async fn cancel(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let run = find_authorized_run(&db, &user, id, EntityAction::RunNow).await?;
    let job_id = run
        .job_id
        .ok_or_else(|| ApiError::not_found("this run has no cancellable job"))?;
    job_service::cancel(&db, &state.redis, &job_id, &user).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The org's run `id` (404 otherwise), once `action` on its entity is authorized.
async fn find_authorized_run(
    db: &Db,
    user: &user::Model,
    id: i64,
    action: EntityAction,
) -> Result<run::Model, ApiError> {
    let run = run::Entity::find_by_id(id)
        .one(db)
        .await?
        .filter(|r| r.org_id == user.org_id)
        .ok_or_else(|| ApiError::not_found("run not found"))?;
    schedule_service::authorize_entity(db, user, &run.entity_type, run.entity_id, action).await?;
    Ok(run)
}

/// `GET /api/runs/:id/result.csv` — streams the run's `query_results` row as CSV. 404s "result no
/// longer retained" for a failed run or one whose result has since been GC'd.
pub async fn result_csv(
    State(state): State<crate::state::AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let run = find_authorized_run(&db, &user, id, EntityAction::Read).await?;
    let (bytes, filename) =
        run_service::run_result_csv(&db, state.result_reader(), &user, &run).await?;
    record_event(
        &db,
        user.org_id,
        Some(user.id),
        "run.result_downloaded",
        Some("run"),
        Some(run.id),
        serde_json::json!({ "format": "csv", "run_id": run.id, "row_count": run.row_count }),
    )
    .await?;
    Response::builder()
        .header(CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(
            CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(Body::from(bytes))
        .map_err(|e| ApiError::internal(format!("failed to build CSV response: {e}")))
}
