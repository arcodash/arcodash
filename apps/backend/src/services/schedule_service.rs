//! Schedule CRUD + the "what's scheduled" read model. A schedule inherits the access rule of its
//! target entity (`query` or `monitor`) via [`authorize_entity`].
//!
//! Functions are generic over `db: &impl ConnectionTrait` and every query is explicitly
//! `.filter(OrgId)`, never relying on RLS alone (OSS has none).

use chrono::Utc;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, DbBackend, EntityTrait, FromQueryResult, Statement,
};
use serde_json::json;

use arcodash_core::entities::{monitor, query, schedule, user};
use arcodash_core::events::record_event;
use arcodash_core::schedule_spec::{self, ScheduleSpec};

use crate::authz::authorize::{self, Action, MonitorVerb, QueryVerb};
use crate::error::{is_unique_violation, ApiError};
use crate::services::query_service;

/// `SCHEDULE_EVERY_SECS_FLOOR` env override for the interval floor `schedule_spec::validate`
/// enforces. `pub` so `main.rs` can warn at startup if it disagrees with
/// `schedule_spec::EVERY_SECS_FLOOR`.
#[must_use]
pub fn every_secs_floor() -> i64 {
    arcodash_core::config::env_parsed("SCHEDULE_EVERY_SECS_FLOOR", schedule_spec::EVERY_SECS_FLOOR)
}

/// Parse + validate an incoming spec, timezone and overlap policy. Rejects bad cron/`at`, a
/// sub-floor interval, an unresolvable timezone, and the unimplemented `parallel`/`replace`
/// policies.
fn validate_spec_and_policy(
    spec_json: &serde_json::Value,
    timezone: &str,
    overlap_policy: &str,
) -> Result<ScheduleSpec, ApiError> {
    let spec: ScheduleSpec = serde_json::from_value(spec_json.clone())
        .map_err(|e| ApiError::bad_request(format!("invalid schedule spec: {e}")))?;
    schedule_spec::validate(&spec, every_secs_floor())
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    // The timezone is only checked in compute_next; a throwaway compute turns a bad tz into a 400
    // instead of a poison row.
    schedule_spec::compute_next(&spec, timezone, Utc::now(), 0, 0)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    if !matches!(overlap_policy, "skip" | "coalesce") {
        return Err(ApiError::bad_request(format!(
            "overlap_policy must be 'skip' or 'coalesce' (got '{overlap_policy}')"
        )));
    }
    Ok(spec)
}

/// Which tier of an entity's own access rule a schedule/run action needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityAction {
    Read,
    ScheduleCrud,
    RunNow,
}

/// Authorize `action`-tier access to the entity a schedule/run targets (`"query"` | `"monitor"`).
/// Every denial, including a missing entity, is an indistinguishable 404.
///
/// `entity_id` is a bare id off the request, so existence is checked explicitly: `authorize()`'s
/// admin short-circuit would otherwise let an admin through on a nonexistent entity.
pub async fn authorize_entity(
    db: &impl ConnectionTrait,
    user: &user::Model,
    entity_type: &str,
    entity_id: i64,
    action: EntityAction,
) -> Result<(), ApiError> {
    let authz_action = match entity_type {
        "query" => {
            query_service::get_for_user(db, user.org_id, user, entity_id)
                .await
                .map_err(|_| ApiError::not_found("query not found"))?;
            let verb = match action {
                EntityAction::Read => QueryVerb::View,
                EntityAction::ScheduleCrud => QueryVerb::Schedule,
                EntityAction::RunNow => QueryVerb::Execute,
            };
            Action::Query {
                verb,
                id: entity_id,
            }
        }
        "monitor" => {
            let m = monitor::Entity::find_by_id(entity_id)
                .one(db)
                .await?
                .filter(|m| m.org_id == user.org_id)
                .ok_or_else(|| ApiError::not_found("monitor not found"))?;
            // Like `monitor_service::get_for_user`: a monitor is only reachable through a query the
            // caller can see, data-source axis included.
            query_service::get_for_user(db, user.org_id, user, m.query_id)
                .await
                .map_err(|_| ApiError::not_found("monitor not found"))?;
            let verb = match action {
                EntityAction::Read => MonitorVerb::View,
                EntityAction::ScheduleCrud | EntityAction::RunNow => MonitorVerb::Edit,
            };
            Action::Monitor {
                verb,
                id: entity_id,
            }
        }
        _ => {
            return Err(ApiError::bad_request(
                "entity_type must be 'query' or 'monitor'",
            ))
        }
    };
    authorize::authorize(db, user, authz_action).await?;

    // `authorize()` is capability + reach only; a workspace-open query is also gated on the
    // data-source axis (`can_view_object`), with the same not_found string as `get_for_user` so a
    // prober can't tell the surfaces apart.
    if entity_type == "query"
        && !crate::authz::access::can_view_object(db, user, "query", entity_id).await?
    {
        return Err(ApiError::not_found("query not found"));
    }

    Ok(())
}

/// Org-scoped query fetch with no additional authz (the caller already gated via
/// [`authorize_entity`]). `pub(crate)` for reuse by `run_service::enqueue_manual_run`.
pub(crate) async fn load_query_org(
    db: &impl ConnectionTrait,
    org_id: i64,
    id: i64,
) -> Result<query::Model, ApiError> {
    query::Entity::find_by_id(id)
        .one(db)
        .await?
        .filter(|q| q.org_id == org_id)
        .ok_or_else(|| ApiError::not_found("query not found"))
}

/// The raw row the lateral join in [`list`] returns: one per schedule, with its latest run's
/// terminal fields (`None` if never run).
#[derive(Debug, FromQueryResult)]
pub struct ScheduleRow {
    pub id: i64,
    pub entity_type: String,
    pub entity_id: i64,
    pub spec: serde_json::Value,
    pub timezone: String,
    pub enabled: bool,
    pub paused_until: Option<sea_orm::prelude::DateTimeWithTimeZone>,
    pub auto_paused_at: Option<sea_orm::prelude::DateTimeWithTimeZone>,
    pub overlap_policy: String,
    pub next_run_at: Option<sea_orm::prelude::DateTimeWithTimeZone>,
    pub consecutive_failures: i32,
    pub last_status: Option<String>,
    pub last_finished_at: Option<sea_orm::prelude::DateTimeWithTimeZone>,
    pub last_duration_ms: Option<i64>,
    pub last_error: Option<String>,
}

/// All schedules for `(entity_type, entity_id)` if both given (0 or 1 row via `UNIQUE`), otherwise
/// every org schedule.
///
/// A `LEFT JOIN LATERAL` attaches the latest run. Per-row authorization for the all-schedules case
/// is the caller's job; the targeted case is gated by [`authorize_entity`].
pub async fn list(
    db: &impl ConnectionTrait,
    org_id: i64,
    entity: Option<(&str, i64)>,
) -> Result<Vec<ScheduleRow>, ApiError> {
    let (filter_sql, values): (&str, Vec<sea_orm::Value>) = match entity {
        Some((et, eid)) => (
            " AND s.entity_type = $2 AND s.entity_id = $3",
            vec![org_id.into(), et.into(), eid.into()],
        ),
        None => ("", vec![org_id.into()]),
    };
    // The cap applies uniformly; a targeted lookup is already bounded to 0/1 by `UNIQUE`.
    let candidate_cap: u64 =
        arcodash_core::config::env_parsed("LIST_SCHEDULES_MAX_CANDIDATES", 2000);
    let sql = format!(
        "SELECT s.id, s.entity_type, s.entity_id, s.spec, s.timezone, s.enabled, \
                s.paused_until, s.auto_paused_at, s.overlap_policy, s.next_run_at, \
                s.consecutive_failures, \
                r.status AS last_status, r.finished_at AS last_finished_at, \
                r.duration_ms AS last_duration_ms, r.error AS last_error \
         FROM schedules s \
         LEFT JOIN LATERAL ( \
             SELECT status, finished_at, duration_ms, error FROM runs \
             WHERE runs.entity_type = s.entity_type AND runs.entity_id = s.entity_id \
             ORDER BY runs.id DESC LIMIT 1 \
         ) r ON TRUE \
         WHERE s.org_id = $1{filter_sql} \
         ORDER BY s.id \
         LIMIT {candidate_cap}",
    );
    let rows = ScheduleRow::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Postgres,
        sql,
        values,
    ))
    .all(db)
    .await?;
    if entity.is_none() && rows.len() as u64 == candidate_cap {
        tracing::warn!(
            org_id,
            candidate_cap,
            "schedule_service::list: candidate schedule load hit its cap; some schedules may be \
             missing from this listing"
        );
    }
    Ok(rows)
}

/// `POST /api/schedules`. `entity_type` must be `"query"`/`"monitor"`; a query target must already
/// be published, since a never-published query has nothing for the worker to run.
pub async fn create(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    entity_type: String,
    entity_id: i64,
    spec_json: serde_json::Value,
    timezone: Option<String>,
    overlap_policy: Option<String>,
    enabled: Option<bool>,
) -> Result<schedule::Model, ApiError> {
    if !matches!(entity_type.as_str(), "query" | "monitor") {
        return Err(ApiError::bad_request(
            "entity_type must be 'query' or 'monitor'",
        ));
    }
    authorize_entity(
        db,
        user,
        &entity_type,
        entity_id,
        EntityAction::ScheduleCrud,
    )
    .await?;

    let timezone = timezone.unwrap_or_else(|| "UTC".to_string());
    let overlap_policy = overlap_policy.unwrap_or_else(|| "skip".to_string());
    let spec = validate_spec_and_policy(&spec_json, &timezone, &overlap_policy)?;

    if entity_type == "query" {
        let q = load_query_org(db, org_id, entity_id).await?;
        if !query_service::is_published(&q) {
            return Err(ApiError::new(
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "publish this query before scheduling it",
            ));
        }
        // The worker enqueues a scheduled tick's query text raw with `parameters: Null`, so a parameterized query would fail every tick until auto-pause. Checked against the resolved *serving* content, not the draft.
        let resolved = query_service::resolve_serving_content(&q);
        if !crate::services::parameters::referenced_names(&resolved.query).is_empty() {
            return Err(ApiError::new(
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "this query has parameters; a schedule runs with no parameter values supplied — \
                 remove the parameters, or publish a parameter-free version, before scheduling it",
            ));
        }
    }

    let enabled = enabled.unwrap_or(true);
    let now = Utc::now();
    let next = schedule_spec::compute_next(&spec, &timezone, now, 0, 0)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let model = schedule::ActiveModel {
        org_id: Set(org_id),
        entity_type: Set(entity_type),
        entity_id: Set(entity_id),
        spec: Set(spec_json.clone()),
        timezone: Set(timezone),
        enabled: Set(enabled),
        overlap_policy: Set(overlap_policy),
        next_run_at: Set(Some(next.into())),
        consecutive_failures: Set(0),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
        ..Default::default()
    };
    let created = model.insert(db).await.map_err(|e| {
        if is_unique_violation(&e) {
            ApiError::conflict("this entity already has a schedule")
        } else {
            ApiError::from(e)
        }
    })?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "schedule.created",
        Some("schedule"),
        Some(created.id),
        json!({ "entity_type": created.entity_type, "entity_id": created.entity_id, "cron": spec_json }),
    )
    .await?;

    Ok(created)
}

/// Any-subset patch. `paused_until: Some(None)` clears it; `None` (absent) leaves it untouched.
pub struct SchedulePatch {
    pub spec: Option<serde_json::Value>,
    pub timezone: Option<String>,
    pub overlap_policy: Option<String>,
    pub enabled: Option<bool>,
    pub paused_until: Option<Option<sea_orm::prelude::DateTimeWithTimeZone>>,
}

/// Whether `patch` touches any field that can move the firing deadline; [`update`] recomputes
/// `next_run_at` only then.
const fn patch_affects_next_run(patch: &SchedulePatch) -> bool {
    patch.spec.is_some()
        || patch.timezone.is_some()
        || patch.enabled.is_some()
        || patch.paused_until.is_some()
}

/// `PATCH /api/schedules/:id`. Setting `enabled: true` or clearing `paused_until` clears the
/// failure auto-pause (`auto_paused_at`, `consecutive_failures`) and recomputes `next_run_at` from
/// now without backoff.
pub async fn update(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    id: i64,
    patch: SchedulePatch,
) -> Result<schedule::Model, ApiError> {
    let existing = schedule::Entity::find_by_id(id)
        .one(db)
        .await?
        .filter(|s| s.org_id == org_id)
        .ok_or_else(|| ApiError::not_found("schedule not found"))?;
    authorize_entity(
        db,
        user,
        &existing.entity_type,
        existing.entity_id,
        EntityAction::ScheduleCrud,
    )
    .await?;

    let affects_next_run = patch_affects_next_run(&patch);
    let clears_pause =
        matches!(patch.enabled, Some(true)) || matches!(patch.paused_until, Some(None));
    let consecutive_failures = if clears_pause {
        0
    } else {
        existing.consecutive_failures
    };
    let mut fence = existing.next_run_at;

    let timezone = patch.timezone.unwrap_or_else(|| existing.timezone.clone());
    let overlap_policy = patch
        .overlap_policy
        .unwrap_or_else(|| existing.overlap_policy.clone());
    let spec_changed = patch.spec.is_some();
    let spec_json = patch.spec.unwrap_or_else(|| existing.spec.clone());
    let spec = validate_spec_and_policy(&spec_json, &timezone, &overlap_policy)?;

    let now = Utc::now();

    if clears_pause {
        // A conditional statement, not the whole-row write, so it can't clobber a concurrent worker
        // failure-count increment (`run_lifecycle::finalize_run`).
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE schedules SET auto_paused_at = NULL, consecutive_failures = 0 \
             WHERE id = $1 AND org_id = $2",
            [id.into(), org_id.into()],
        ))
        .await?;
        crate::metrics::incr_schedule_auto_pause_cleared();
    }

    let mut active: schedule::ActiveModel = existing.into();
    if spec_changed {
        active.spec = Set(spec_json);
    }
    active.timezone = Set(timezone.clone());
    active.overlap_policy = Set(overlap_policy);
    if let Some(en) = patch.enabled {
        active.enabled = Set(en);
    }
    if let Some(pu) = patch.paused_until {
        active.paused_until = Set(pu);
    }
    // Written by the conditional statement above (or left alone); excluded here so the stale
    // snapshot isn't echoed back.
    active.auto_paused_at = sea_orm::ActiveValue::NotSet;
    active.consecutive_failures = sea_orm::ActiveValue::NotSet;
    // Written by the fenced loop below; the worker advances this column on every run, so a plain
    // overwrite would race it.
    active.next_run_at = sea_orm::ActiveValue::NotSet;
    active.updated_at = Set(now.into());
    let mut updated = active.update(db).await?;

    if affects_next_run {
        // The jitter seed is the schedule's id: a stable, herd-desyncing offset across edits.
        let next = schedule_spec::compute_next(
            &spec,
            &timezone,
            now,
            consecutive_failures,
            id.cast_unsigned(),
        )
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

        // Fenced on the snapshot's `next_run_at`: the worker's finalize also advances it. One
        // reload-and-retry; if that loses the race, the worker's advance wins.
        for _ in 0..2 {
            let stmt = Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE schedules SET next_run_at = $1, updated_at = now() \
                 WHERE id = $2 AND org_id = $3 AND next_run_at IS NOT DISTINCT FROM $4",
                [next.into(), id.into(), org_id.into(), fence.into()],
            );
            if db.execute_raw(stmt).await?.rows_affected() > 0 {
                updated.next_run_at = Some(next.into());
                break;
            }
            let reloaded = schedule::Entity::find_by_id(id)
                .one(db)
                .await?
                .ok_or_else(|| ApiError::not_found("schedule not found"))?;
            fence = reloaded.next_run_at;
            updated.next_run_at = reloaded.next_run_at;
        }
    }

    record_event(
        db,
        org_id,
        Some(user.id),
        "schedule.updated",
        Some("schedule"),
        Some(updated.id),
        json!({ "entity_type": updated.entity_type, "entity_id": updated.entity_id, "cron": updated.spec }),
    )
    .await?;

    Ok(updated)
}

/// `DELETE /api/schedules/:id`. `runs.schedule_id` is `ON DELETE SET NULL`, so run history
/// survives.
pub async fn delete(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
    id: i64,
) -> Result<(), ApiError> {
    let existing = schedule::Entity::find_by_id(id)
        .one(db)
        .await?
        .filter(|s| s.org_id == org_id)
        .ok_or_else(|| ApiError::not_found("schedule not found"))?;
    authorize_entity(
        db,
        user,
        &existing.entity_type,
        existing.entity_id,
        EntityAction::ScheduleCrud,
    )
    .await?;
    schedule::Entity::delete_by_id(id).exec(db).await?;

    record_event(
        db,
        org_id,
        Some(user.id),
        "schedule.deleted",
        Some("schedule"),
        Some(existing.id),
        json!({ "entity_type": existing.entity_type, "entity_id": existing.entity_id, "cron": existing.spec }),
    )
    .await?;

    Ok(())
}

/// The default schedule a new monitor gets: enabled, every 15 minutes, skip-overlap, UTC. Called
/// from `monitor_service::create` on the same `db` handle.
pub async fn create_default_monitor_schedule(
    db: &impl ConnectionTrait,
    org_id: i64,
    monitor_id: i64,
) -> Result<schedule::Model, ApiError> {
    let spec_json = serde_json::json!({ "kind": "interval", "every_secs": 900 });
    let now = Utc::now();
    let next = schedule_spec::compute_next(
        &ScheduleSpec::Interval { every_secs: 900 },
        "UTC",
        now,
        0,
        monitor_id.cast_unsigned(),
    )
    .expect("a fixed 900s interval spec always computes a next deadline");
    schedule::ActiveModel {
        org_id: Set(org_id),
        entity_type: Set("monitor".to_string()),
        entity_id: Set(monitor_id),
        spec: Set(spec_json),
        timezone: Set("UTC".to_string()),
        enabled: Set(true),
        overlap_policy: Set("skip".to_string()),
        next_run_at: Set(Some(next.into())),
        consecutive_failures: Set(0),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
        ..Default::default()
    }
    .insert(db)
    .await
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_spec_and_policy_rejects_sub_floor_interval() {
        let err = validate_spec_and_policy(
            &serde_json::json!({ "kind": "interval", "every_secs": 30 }),
            "UTC",
            "skip",
        )
        .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_spec_and_policy_rejects_bad_cron() {
        let err = validate_spec_and_policy(
            &serde_json::json!({ "kind": "cron", "expr": "not a cron" }),
            "UTC",
            "skip",
        )
        .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_spec_and_policy_rejects_bad_timezone() {
        let err = validate_spec_and_policy(
            &serde_json::json!({ "kind": "interval", "every_secs": 300 }),
            "Not/AZone",
            "skip",
        )
        .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_spec_and_policy_rejects_reserved_overlap_policies() {
        for policy in ["parallel", "replace"] {
            let err = validate_spec_and_policy(
                &serde_json::json!({ "kind": "interval", "every_secs": 300 }),
                "UTC",
                policy,
            )
            .unwrap_err();
            assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
            assert!(err.message.contains(policy));
        }
    }

    #[test]
    fn validate_spec_and_policy_accepts_skip_and_coalesce() {
        for policy in ["skip", "coalesce"] {
            assert!(validate_spec_and_policy(
                &serde_json::json!({ "kind": "interval", "every_secs": 300 }),
                "UTC",
                policy,
            )
            .is_ok());
        }
    }

    #[test]
    fn every_secs_floor_defaults_to_the_shared_constant_when_unset() {
        if std::env::var("SCHEDULE_EVERY_SECS_FLOOR").is_err() {
            assert_eq!(every_secs_floor(), schedule_spec::EVERY_SECS_FLOOR);
        }
    }

    /// Table test over every `SchedulePatch` field: only `spec`/`timezone`/`enabled`/`paused_until`
    /// may move `next_run_at`.
    #[test]
    fn patch_affects_next_run_only_for_deadline_relevant_fields() {
        fn blank() -> SchedulePatch {
            SchedulePatch {
                spec: None,
                timezone: None,
                overlap_policy: None,
                enabled: None,
                paused_until: None,
            }
        }

        assert!(!patch_affects_next_run(&blank()), "no-op patch");
        assert!(
            !patch_affects_next_run(&SchedulePatch {
                overlap_policy: Some("coalesce".to_string()),
                ..blank()
            }),
            "overlap_policy alone must not move next_run_at"
        );
        assert!(patch_affects_next_run(&SchedulePatch {
            spec: Some(serde_json::json!({ "kind": "interval", "every_secs": 300 })),
            ..blank()
        }));
        assert!(patch_affects_next_run(&SchedulePatch {
            timezone: Some("UTC".to_string()),
            ..blank()
        }));
        assert!(patch_affects_next_run(&SchedulePatch {
            enabled: Some(true),
            ..blank()
        }));
        assert!(patch_affects_next_run(&SchedulePatch {
            paused_until: Some(None),
            ..blank()
        }));
    }
}
