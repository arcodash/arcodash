//! Registers the worker's two scheduler-driven task types (`escalation_tick`, `digest_fanout`)
//! as real Conveyor schedules at startup, behind `CONVEYOR_SCHEDULER_ENABLED`.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

use arcodash_conveyor::schedule_spec::ScheduleSpec;
use arcodash_conveyor::{Conveyor, OverlapPolicy, ScheduleRequest};

/// Earlier registrations used this literal as `entity_id`, which can collide with real serial ids;
/// `register_aux_schedules` deletes any row still carrying it before upserting under the
/// NULL-`entity_id` convention.
const LEGACY_SINGLETON_SCHEDULE_ENTITY_ID: i64 = 1;

pub fn scheduler_enabled() -> bool {
    arcodash_core::config::parse_bool_env("CONVEYOR_SCHEDULER_ENABLED", false).unwrap_or_else(|e| {
        tracing::error!("{e}");
        false
    })
}

/// `MONITOR_TICK_SECS` (default 300s) parsed via `env_parsed` and floored at
/// `arcodash_conveyor::schedule_spec::EVERY_SECS_FLOOR` (60s); read locally to avoid depending on
/// the Redis-queue tick module.
///
/// Deliberately not unified with that module's 1s `MONITOR_TICK_SECS_MIN`: this value becomes a
/// Conveyor `spec.every_secs`, and sub-floor values are hard-rejected (failing worker startup).
/// Warns whenever the clamp changes the value.
fn escalation_interval_secs() -> i64 {
    let configured = arcodash_core::config::env_parsed("MONITOR_TICK_SECS", 300i64);
    let floored = configured.max(arcodash_conveyor::schedule_spec::EVERY_SECS_FLOOR);
    if floored != configured {
        tracing::warn!(
            configured_secs = configured,
            floored_secs = floored,
            "conveyor_schedules: MONITOR_TICK_SECS is below the conveyor crate's schedule-interval \
             floor, clamping — the escalation tick will run slower than configured"
        );
    }
    floored
}

/// A UTC, overlap-skipping schedule with an empty payload.
fn schedule_request(
    task_type: &str,
    entity_id: Option<i64>,
    spec: ScheduleSpec,
) -> ScheduleRequest {
    ScheduleRequest {
        task_type: task_type.into(),
        entity_id,
        payload: serde_json::json!({}),
        spec,
        timezone: "UTC".to_string(),
        overlap_policy: OverlapPolicy::Skip,
        tenant_id: None,
        resource_key: None,
        resource_cap: None,
    }
}

/// Deletes a leftover `entity_id = 1` row for `task_type`, since `upsert_schedule`'s `ON CONFLICT`
/// only matches the NULL-keyed row. Returns the error rather than swallowing it: upserting anyway
/// would leave two active rows and double-fire the schedule.
async fn delete_legacy_singleton_row(
    db: &DatabaseConnection,
    task_type: &str,
) -> Result<(), sea_orm::DbErr> {
    let res = db
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DbBackend::Postgres,
            "DELETE FROM conveyor_schedules WHERE task_type = $1 AND entity_id = $2",
            [task_type.into(), LEGACY_SINGLETON_SCHEDULE_ENTITY_ID.into()],
        ))
        .await?;
    if res.rows_affected() > 0 {
        tracing::info!(
            task_type,
            rows = res.rows_affected(),
            "conveyor_schedules: replaced a legacy entity_id=1 singleton row with the NULL-entity_id convention"
        );
    }
    Ok(())
}

/// Idempotent across restarts/replicas: `Conveyor::upsert_schedule` updates the existing
/// (`task_type`, `entity_id`) row in place, so a changed spec reaches an already-registered schedule.
pub async fn register_aux_schedules(db: &DatabaseConnection, conveyor: &Conveyor) {
    let schedules = [
        (
            "escalation_tick",
            ScheduleSpec::Interval {
                every_secs: escalation_interval_secs(),
            },
        ),
        // Fires at the top of every UTC hour; `digest_fanout::fan_out_digests` checks each user's
        // own hour/timezone. Cron rather than `Interval`, whose jitter could drift off the hour and
        // miss a user's send window.
        (
            "digest_fanout",
            ScheduleSpec::Cron {
                expr: "0 0 * * * *".to_string(),
            },
        ),
    ];
    for (task_type, spec) in schedules {
        if let Err(e) = delete_legacy_singleton_row(db, task_type).await {
            tracing::error!(
                error = %e,
                "conveyor_schedules: failed to clean up a legacy entity_id=1 singleton row for \
                 {task_type}, skipping this cycle's upsert to avoid doubling its cadence"
            );
        } else if let Err(e) = conveyor
            .upsert_schedule(schedule_request(task_type, None, spec))
            .await
        {
            tracing::error!(error = %e, "conveyor: failed to register {task_type} schedule");
        }
    }
}

#[cfg(test)]
mod tests {
    //! Real Postgres + Redis only (skips without `TEST_DATABASE_URL`/`TEST_REDIS_URL`); connects
    //! directly because `arcodash_conveyor::test_support` is crate-private.
    use super::*;
    use crate::scheduler::test_support::query_row;

    #[test]
    fn scheduler_enabled_is_off_by_default() {
        std::env::remove_var("CONVEYOR_SCHEDULER_ENABLED");
        assert!(!scheduler_enabled());
    }

    /// A configured `MONITOR_TICK_SECS` below the 60s floor must still clamp to a valid,
    /// floor-respecting interval (the warn! isn't asserted).
    #[test]
    fn interval_below_the_crate_floor_clamps_up_to_it() {
        std::env::set_var("MONITOR_TICK_SECS", "5");
        assert_eq!(
            escalation_interval_secs(),
            arcodash_conveyor::schedule_spec::EVERY_SECS_FLOOR
        );

        std::env::set_var("MONITOR_TICK_SECS", "120");
        assert_eq!(
            escalation_interval_secs(),
            120,
            "a value already above the floor must pass through unchanged"
        );

        std::env::remove_var("MONITOR_TICK_SECS");
    }

    fn interval_request(
        task_type: &str,
        entity_id: Option<i64>,
        every_secs: i64,
    ) -> ScheduleRequest {
        schedule_request(task_type, entity_id, ScheduleSpec::Interval { every_secs })
    }

    async fn setup() -> Option<(
        sea_orm::DatabaseConnection,
        redis::aio::ConnectionManager,
        String,
    )> {
        let database_url = std::env::var("TEST_DATABASE_URL").ok()?;
        let redis_url = std::env::var("TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:6380".to_string());
        let db = arcodash_core::db::connect(&database_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();
        Some((db, redis, redis_url))
    }

    #[tokio::test]
    async fn upsert_schedule_is_idempotent_across_concurrent_boots() {
        let Some((db, redis, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("escalation_tick_race_{}", uuid::Uuid::new_v4());
        let conveyor =
            arcodash_conveyor::ConveyorBuilder::new(db.clone(), redis, redis_url).build();

        let req = || interval_request(&task_type, Some(1), 300);

        let (a, b) = tokio::join!(
            conveyor.upsert_schedule(req()),
            conveyor.upsert_schedule(req())
        );
        a.unwrap();
        b.unwrap();

        let rows = db
            .query_all_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "SELECT id FROM conveyor_schedules WHERE task_type = $1 AND entity_id = $2",
                [task_type.into(), 1i64.into()],
            ))
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "exactly one schedule row, not two, under a concurrent race"
        );
    }

    /// `schedule()` is a plain INSERT and would be swallowed as "already registered";
    /// re-registering at a different interval must still change the row.
    #[tokio::test]
    async fn re_registering_with_a_changed_interval_updates_the_existing_row() {
        let Some((db, redis, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("upsert_interval_{}", uuid::Uuid::new_v4());
        let conveyor =
            arcodash_conveyor::ConveyorBuilder::new(db.clone(), redis, redis_url).build();

        let req = |every_secs| interval_request(&task_type, Some(1), every_secs);

        conveyor.upsert_schedule(req(300)).await.unwrap();
        conveyor.upsert_schedule(req(120)).await.unwrap();

        let rows = db
            .query_all_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "SELECT spec FROM conveyor_schedules WHERE task_type = $1 AND entity_id = $2",
                [task_type.into(), 1i64.into()],
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "re-registering must update, not duplicate");
        let spec: serde_json::Value = rows[0].try_get("", "spec").unwrap();
        assert_eq!(
            spec,
            serde_json::json!({"kind": "interval", "every_secs": 120}),
            "the row's spec must reflect the second, re-registered interval: {spec}"
        );
    }

    /// `enabled`/`paused_until`/`consecutive_failures` are not touched on conflict, so a boot never
    /// un-pauses an operator-paused or auto-paused schedule.
    #[tokio::test]
    async fn re_registering_does_not_un_pause_a_paused_schedule() {
        let Some((db, redis, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("upsert_pause_{}", uuid::Uuid::new_v4());
        let conveyor =
            arcodash_conveyor::ConveyorBuilder::new(db.clone(), redis, redis_url).build();

        let req = || interval_request(&task_type, Some(1), 300);

        let id = conveyor.upsert_schedule(req()).await.unwrap();
        let until = chrono::Utc::now() + chrono::Duration::days(1);
        assert!(
            conveyor.pause_schedule_until(id, until).await.unwrap(),
            "pausing a schedule that exists must report true"
        );
        assert!(
            !conveyor
                .pause_schedule_until(arcodash_conveyor::ScheduleId(-1), until)
                .await
                .unwrap(),
            "pausing an unknown schedule id must report false, not silently succeed"
        );

        conveyor.upsert_schedule(req()).await.unwrap();

        let row = query_row(
            &db,
            "SELECT paused_until FROM conveyor_schedules WHERE id = $1",
            [id.0.into()],
        )
        .await;
        let paused_until: Option<chrono::DateTime<chrono::Utc>> =
            row.try_get("", "paused_until").unwrap();
        assert!(
            paused_until.is_some(),
            "re-registering must not clear an operator-set pause"
        );
    }

    /// `register_aux_schedules` must write the real singleton schedules with a NULL `entity_id`,
    /// not the placeholder literal.
    #[tokio::test]
    async fn singleton_schedules_are_registered_with_a_null_entity_id() {
        let Some((db, redis, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let conveyor =
            arcodash_conveyor::ConveyorBuilder::new(db.clone(), redis, redis_url).build();

        register_aux_schedules(&db, &conveyor).await;

        for task_type in ["escalation_tick", "digest_fanout"] {
            let rows = db
                .query_all_raw(sea_orm::Statement::from_sql_and_values(
                    sea_orm::DbBackend::Postgres,
                    "SELECT entity_id FROM conveyor_schedules WHERE task_type = $1",
                    [task_type.into()],
                ))
                .await
                .unwrap();
            assert_eq!(
                rows.len(),
                1,
                "exactly one {task_type} schedule row must exist"
            );
            let entity_id: Option<i64> = rows[0].try_get("", "entity_id").unwrap();
            assert_eq!(
                entity_id, None,
                "{task_type}'s entity_id must be NULL, not a sentinel value"
            );
        }

        db.execute_raw(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Postgres,
            "DELETE FROM conveyor_schedules WHERE task_type IN ('escalation_tick', 'digest_fanout')",
        ))
        .await
        .ok();
    }

    /// A database with a legacy `entity_id = 1` singleton row ends up with exactly one
    /// (NULL-entity_id) row after re-registration.
    #[tokio::test]
    async fn re_registering_replaces_an_entity_id_1_singleton_instead_of_duplicating_it() {
        let Some((db, redis, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("escalation_tick_entity1_{}", uuid::Uuid::new_v4());
        let conveyor =
            arcodash_conveyor::ConveyorBuilder::new(db.clone(), redis, redis_url).build();

        // Seed an `entity_id = 1` row as earlier registrations wrote it.
        conveyor
            .upsert_schedule(interval_request(
                &task_type,
                Some(LEGACY_SINGLETON_SCHEDULE_ENTITY_ID),
                300,
            ))
            .await
            .unwrap();

        delete_legacy_singleton_row(&db, &task_type).await.unwrap();
        conveyor
            .upsert_schedule(interval_request(&task_type, None, 300))
            .await
            .unwrap();

        let rows = db
            .query_all_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "SELECT entity_id FROM conveyor_schedules WHERE task_type = $1",
                [task_type.clone().into()],
            ))
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "exactly one row must remain for {task_type}, not a duplicate"
        );
        let entity_id: Option<i64> = rows[0].try_get("", "entity_id").unwrap();
        assert_eq!(
            entity_id, None,
            "the surviving row must use the NULL convention"
        );
    }
}
