//! `GET/POST/PATCH/DELETE /api/schedules[/:id]` — schedule CRUD plus the "what's scheduled" read
//! model.
//!
//! A schedule has no access rule of its own; every call routes through
//! `schedule_service::authorize_entity`, which applies the target entity's own rule.

use crate::extract::{Json, Path};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use arcodash_core::entities::{group, schedule, user};

use super::deserialize_present;
use crate::auth::CurrentUser;
use crate::authz::access;
use crate::authz::permissions;
use crate::db::Db;
use crate::error::ApiError;
use crate::services::schedule_service::{self, EntityAction, SchedulePatch, ScheduleRow};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct LastRunDto {
    pub status: String,
    pub finished_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ScheduleResponse {
    pub id: i64,
    pub entity_type: String,
    pub entity_id: i64,
    pub spec: serde_json::Value,
    pub timezone: String,
    pub enabled: bool,
    pub paused_until: Option<DateTime<Utc>>,
    pub auto_paused_at: Option<DateTime<Utc>>,
    pub overlap_policy: String,
    pub next_run_at: Option<DateTime<Utc>>,
    pub consecutive_failures: i32,
    /// `None` when the entity has never run.
    pub last_run: Option<LastRunDto>,
}

impl From<ScheduleRow> for ScheduleResponse {
    fn from(r: ScheduleRow) -> Self {
        let last_run = r.last_status.map(|status| LastRunDto {
            status,
            finished_at: r.last_finished_at.map(|t| t.with_timezone(&Utc)),
            duration_ms: r.last_duration_ms,
            error: r.last_error,
        });
        Self {
            id: r.id,
            entity_type: r.entity_type,
            entity_id: r.entity_id,
            spec: r.spec,
            timezone: r.timezone,
            enabled: r.enabled,
            paused_until: r.paused_until.map(|t| t.with_timezone(&Utc)),
            auto_paused_at: r.auto_paused_at.map(|t| t.with_timezone(&Utc)),
            overlap_policy: r.overlap_policy,
            next_run_at: r.next_run_at.map(|t| t.with_timezone(&Utc)),
            consecutive_failures: r.consecutive_failures,
            last_run,
        }
    }
}

/// The create/update responses skip the lateral join (a schedule that was just written has
/// nothing new to report about its last run), so `last_run` is always `None` here.
impl From<schedule::Model> for ScheduleResponse {
    fn from(s: schedule::Model) -> Self {
        Self {
            id: s.id,
            entity_type: s.entity_type,
            entity_id: s.entity_id,
            spec: s.spec,
            timezone: s.timezone,
            enabled: s.enabled,
            paused_until: s.paused_until.map(|t| t.with_timezone(&Utc)),
            auto_paused_at: s.auto_paused_at.map(|t| t.with_timezone(&Utc)),
            overlap_policy: s.overlap_policy,
            next_run_at: s.next_run_at.map(|t| t.with_timezone(&Utc)),
            consecutive_failures: s.consecutive_failures,
            last_run: None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ListSchedulesQuery {
    pub entity_type: Option<String>,
    pub entity_id: Option<i64>,
}

/// `true` iff `user` holds the view capability for `entity_type` (`"query"` -> `view_query`,
/// `"monitor"` -> `view_monitor`). Admin always passes.
fn view_capability(
    user: &user::Model,
    groups: &[group::Model],
    entity_type: &str,
    is_admin: bool,
) -> bool {
    let key = if entity_type == "monitor" {
        "view_monitor"
    } else {
        "view_query"
    };
    is_admin || permissions::user_has_permission(user, groups, key)
}

/// `GET /api/schedules?entity_type=&entity_id=`.
///
/// Targeted (0 or 1 row) with a single `authorize_entity(Read)` gate, or (no params) every org
/// schedule the caller can view — feeds a labels-across-queries view.
pub async fn list(
    db: Db,
    CurrentUser(user): CurrentUser,
    Query(p): Query<ListSchedulesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    match (p.entity_type.as_deref(), p.entity_id) {
        (Some(et), Some(eid)) => {
            schedule_service::authorize_entity(&db, &user, et, eid, EntityAction::Read).await?;
            let rows = schedule_service::list(&db, user.org_id, Some((et, eid))).await?;
            Ok(Json(
                rows.into_iter()
                    .map(ScheduleResponse::from)
                    .collect::<Vec<_>>(),
            ))
        }
        (None, None) => {
            let rows = schedule_service::list(&db, user.org_id, None).await?;
            // Groups/admin status are per-user, not per-row — load once and reuse across every
            // row's visibility check (see `authorize::evaluate_with_groups`).
            let groups = permissions::load_effective_groups(&db, &user).await?;
            let is_admin = permissions::is_admin(&groups);

            let mut out = Vec::new();
            for row in rows {
                // `can_view_object_with_groups` is the same predicate the targeted branch's
                // `authorize_entity` applies (via `can_view_object`) — it folds in the
                // data-source gate for queries and resolves a monitor's own parent query.
                if access::can_view_object_with_groups(
                    &db,
                    &user,
                    &groups,
                    is_admin,
                    &row.entity_type,
                    row.entity_id,
                )
                .await?
                    && view_capability(&user, &groups, &row.entity_type, is_admin)
                {
                    out.push(ScheduleResponse::from(row));
                }
            }
            Ok(Json(out))
        }
        _ => Err(ApiError::bad_request(
            "entity_type and entity_id must be given together",
        )),
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateScheduleRequest {
    pub entity_type: String,
    pub entity_id: i64,
    pub spec: serde_json::Value,
    pub timezone: Option<String>,
    pub overlap_policy: Option<String>,
    pub enabled: Option<bool>,
}

/// `POST /api/schedules`
pub async fn create(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Json(p): Json<CreateScheduleRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let s = schedule_service::create(
        &db,
        user.org_id,
        &user,
        p.entity_type,
        p.entity_id,
        p.spec,
        p.timezone,
        p.overlap_policy,
        p.enabled,
    )
    .await?;
    state.publish_wake().await;
    Ok((StatusCode::CREATED, Json(ScheduleResponse::from(s))))
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateScheduleRequest {
    pub spec: Option<serde_json::Value>,
    pub timezone: Option<String>,
    pub overlap_policy: Option<String>,
    pub enabled: Option<bool>,
    /// Absent = leave unchanged; `null` = clear; a value = set.
    #[serde(default, deserialize_with = "deserialize_present")]
    pub paused_until: Option<Option<DateTime<Utc>>>,
}

impl From<UpdateScheduleRequest> for SchedulePatch {
    fn from(p: UpdateScheduleRequest) -> Self {
        Self {
            spec: p.spec,
            timezone: p.timezone,
            overlap_policy: p.overlap_policy,
            enabled: p.enabled,
            paused_until: p.paused_until.map(|opt| opt.map(Into::into)),
        }
    }
}

/// `PATCH /api/schedules/:id`
pub async fn update(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(p): Json<UpdateScheduleRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let s = schedule_service::update(&db, user.org_id, &user, id, p.into()).await?;
    state.publish_wake().await;
    Ok(Json(ScheduleResponse::from(s)))
}

/// `DELETE /api/schedules/:id`
pub async fn delete(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    schedule_service::delete(&db, user.org_id, &user, id).await?;
    state.publish_wake().await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::UpdateScheduleRequest;

    #[test]
    fn paused_until_absent_leaves_unchanged() {
        let req: UpdateScheduleRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req.paused_until, None);
    }

    #[test]
    fn paused_until_null_clears() {
        let req: UpdateScheduleRequest = serde_json::from_str(r#"{"paused_until":null}"#).unwrap();
        assert_eq!(req.paused_until, Some(None));
    }

    #[test]
    fn paused_until_value_sets() {
        let req: UpdateScheduleRequest =
            serde_json::from_str(r#"{"paused_until":"2026-01-01T00:00:00Z"}"#).unwrap();
        assert!(req.paused_until.flatten().is_some());
    }
}
