//! Escalation tick: advances one escalation step per still-`triggered`, unacked monitor per tick,
//! under the monitor scheduler's leader lock.

use chrono::Utc;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, QueryFilter,
    Statement, TransactionTrait,
};

use arcodash_conveyor::Conveyor;
use arcodash_core::entities::{monitor, notification_delivery};
use arcodash_core::entitlements::WorkGate;
use arcodash_core::events::record_event;
use arcodash_core::notify::{enqueue_send_notification, SendNotificationParams};
use arcodash_core::queue::Queue;

/// Caps DUE monitors considered per tick. The scan joins each monitor to its next step and filters
/// on due time, so not-yet-due monitors don't consume slots.
const ESCALATION_SCAN_LIMIT: u64 = 200;

struct DueEscalation {
    monitor_id: i64,
    org_id: i64,
    user_id: i64,
    options: serde_json::Value,
    escalation_step: i32,
    next_position: i32,
    destination_id: i64,
}

/// Outcome of one tick; a nonzero `failed` fails the whole tick (see below) rather than only being
/// logged.
pub struct EscalationSummary {
    pub due: usize,
    pub escalated: usize,
    /// Monitors whose step advance was attempted but didn't land (transaction, write, insert or
    /// commit failure); not-due or acked/muted monitors are correct no-ops.
    pub failed: usize,
}

pub async fn run_due_escalations(
    db: &DatabaseConnection,
    entitlements: &dyn arcodash_core::entitlements::OrgEntitlements,
    queue: &dyn Queue,
    conveyor: Option<&Conveyor>,
    storage: &dyn arcodash_core::org_storage::OrgStorageResolver,
) -> anyhow::Result<EscalationSummary> {
    let rows = db
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            format!(
                "SELECT m.id, m.org_id, m.user_id, m.options, m.escalation_step, \
                        s.position, s.destination_id \
                 FROM monitors m \
                 JOIN escalation_steps s \
                   ON s.monitor_id = m.id \
                  AND s.position = (SELECT MIN(position) FROM escalation_steps \
                                     WHERE monitor_id = m.id AND position > m.escalation_step) \
                 WHERE m.state = 'triggered' AND m.acked_at IS NULL \
                   AND CASE WHEN m.escalation_step = 0 THEN m.last_triggered_at ELSE m.last_escalated_at END \
                       + make_interval(mins => s.delay_minutes) <= now() \
                 ORDER BY m.id \
                 LIMIT {ESCALATION_SCAN_LIMIT}"
            ),
        ))
        .await
        .inspect_err(|e| tracing::error!(error = %e, "escalations: failed to load due escalations"))?;

    let parse = |row: &sea_orm::QueryResult| -> Result<DueEscalation, sea_orm::DbErr> {
        Ok(DueEscalation {
            monitor_id: row.try_get("", "id")?,
            org_id: row.try_get("", "org_id")?,
            user_id: row.try_get("", "user_id")?,
            options: row.try_get("", "options")?,
            escalation_step: row.try_get("", "escalation_step")?,
            next_position: row.try_get("", "position")?,
            destination_id: row.try_get("", "destination_id")?,
        })
    };
    let due: Vec<DueEscalation> = rows
        .iter()
        .filter_map(|row| {
            parse(row)
                .inspect_err(|_| {
                    tracing::error!("escalations: malformed due-escalation row, skipping");
                })
                .ok()
        })
        .collect();

    // The scan is cross-org, so the gate is cached per org rather than queried per monitor (bounded
    // by `ESCALATION_SCAN_LIMIT`).
    let mut gate_cache = std::collections::HashMap::new();

    let mut summary = EscalationSummary {
        due: due.len(),
        escalated: 0,
        failed: 0,
    };

    for m in due {
        let gate = if let Some(g) = gate_cache.get(&m.org_id) {
            *g
        } else {
            let g = entitlements.scheduled_work(db, m.org_id).await;
            gate_cache.insert(m.org_id, g);
            g
        };
        if let WorkGate::Blocked { reason } = gate {
            tracing::info!(org_id = m.org_id, reason, "skipping scheduled work");
            continue;
        }

        // A muted OR still-snoozed monitor must not escalate (`is_silenced` is the shared helper).
        let monitor_options =
            arcodash_core::models::parse_monitor_options(m.monitor_id, &m.options);
        if monitor_options.is_silenced(Utc::now()) {
            continue;
        }

        // Conditional advance: only the caller matching the CURRENT escalation_step, and only while
        // the monitor is still triggered/unacked, wins, so a double-leader tick can't double-fire a
        // step. The advance and the delivery-row insert share one transaction: otherwise a crash
        // between them would permanently lose that step's notification. Enqueue stays outside; a
        // lost message is recovered by the stale-pending sweep.
        let now = Utc::now();
        let txn = match db.begin().await {
            Ok(txn) => txn,
            Err(e) => {
                tracing::error!(monitor_id = m.monitor_id, error = %e, "escalations: failed to open transaction");
                summary.failed += 1;
                continue;
            }
        };

        let update_result = monitor::Entity::update_many()
            .set(monitor::ActiveModel {
                escalation_step: Set(m.next_position),
                last_escalated_at: Set(Some(now.into())),
                ..Default::default()
            })
            .filter(monitor::Column::Id.eq(m.monitor_id))
            .filter(monitor::Column::EscalationStep.eq(m.escalation_step))
            .filter(monitor::Column::State.eq("triggered"))
            .filter(monitor::Column::AckedAt.is_null())
            .exec(&txn)
            .await;

        let rows_affected = match update_result {
            Ok(r) => r.rows_affected,
            Err(e) => {
                tracing::error!(monitor_id = m.monitor_id, error = %e, "escalations: failed to advance escalation_step");
                summary.failed += 1;
                continue;
            }
        };
        if rows_affected != 1 {
            // Lost the race to another leader tick, or the monitor was acked/resolved meanwhile;
            // nothing to escalate.
            continue;
        }

        let delivery_id = match notification_delivery::insert_pending(
            &txn,
            m.org_id,
            m.monitor_id,
            None,
            m.destination_id,
            "triggered",
            None,
        )
        .await
        {
            Ok(id) => id,
            Err(e) => {
                // Rolls back on drop: the advance must not land without its delivery row.
                tracing::warn!(monitor_id = m.monitor_id, error = %e, "escalations: failed to pre-create delivery row, rolling back the step advance for a retry next tick");
                summary.failed += 1;
                continue;
            }
        };

        if let Err(e) = txn.commit().await {
            tracing::error!(monitor_id = m.monitor_id, error = %e, "escalations: failed to commit escalation step advance");
            summary.failed += 1;
            continue;
        }
        summary.escalated += 1;
        crate::health::ESCALATIONS_FIRED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let explanation =
            crate::handlers::explain_alert::latest_explanation(db, storage, m.org_id, m.monitor_id)
                .await;
        if let Err(e) = enqueue_send_notification(
            queue,
            conveyor,
            SendNotificationParams {
                org_id: m.org_id,
                monitor_id: m.monitor_id,
                destination_id: m.destination_id,
                new_state: "triggered".to_string(),
                subscriber_id: m.user_id,
                subscription_id: None,
                attempt: 1,
                delivery_id: Some(delivery_id),
                observed: None,
                explanation,
            },
        )
        .await
        {
            tracing::warn!(monitor_id = m.monitor_id, error = %e, "escalations: failed to enqueue SendNotification");
        }

        if let Err(e) = record_event(
            db,
            m.org_id,
            None,
            "monitor_escalated",
            Some("monitor"),
            Some(m.monitor_id),
            serde_json::json!({ "step": m.next_position, "destination_id": m.destination_id }),
        )
        .await
        {
            // A failing audit-trail write is logged but must not turn a committed escalation into
            // an error.
            tracing::warn!(monitor_id = m.monitor_id, error = %e, "escalations: failed to record monitor_escalated audit event");
        }
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    //! Escalation due-step computation: step 1's delay counts from `last_triggered_at`, later steps
    //! from `last_escalated_at`; ack and mute stop escalation. Needs Postgres + Redis (skipped
    //! without `TEST_DATABASE_URL`/`TEST_REDIS_URL`).
    use super::*;
    use crate::scheduler::test_support::{
        exec_sql, query_row, seed_data_source, seed_org, seed_query, setup, BlockOrg,
    };
    use arcodash_core::entities::{escalation_step, notification_destination, organization};
    use arcodash_core::entitlements::{OrgEntitlements, Unrestricted};
    use arcodash_core::queue::ValkeyQueue;
    use redis::AsyncCommands;
    use sea_orm::ActiveModelTrait;
    use std::sync::Arc;

    type Ts = Option<chrono::DateTime<Utc>>;

    /// Seed a `triggered` monitor (plus its query, owner and a destination) with the given
    /// options/ack/escalation fields; returns `(monitor, destination_id)`.
    async fn seed_triggered_monitor(
        db: &DatabaseConnection,
        org_id: i64,
        options: serde_json::Value,
        acked_at: Ts,
        escalation_step: i32,
        last_triggered_at: Ts,
        last_escalated_at: Ts,
    ) -> (monitor::Model, i64) {
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(Utc::now());
        let suffix = uuid::Uuid::new_v4();
        let ds = seed_data_source(db, org_id).await;
        let q = seed_query(db, org_id, ds.id).await;

        let dest = notification_destination::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("escalation-test-dest-{suffix}")),
            r#type: Set("webhook".to_string()),
            options: Set(None),
            user_id: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();

        let mon = monitor::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("escalation-test-monitor-{suffix}")),
            query_id: Set(q.id),
            user_id: Set(q.user_id),
            options: Set(options),
            state: Set("triggered".to_string()),
            last_triggered_at: Set(last_triggered_at.map(Into::into)),
            rearm: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            acked_at: Set(acked_at.map(Into::into)),
            acked_by: Set(None),
            escalation_step: Set(escalation_step),
            last_escalated_at: Set(last_escalated_at.map(Into::into)),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();

        (mon, dest.id)
    }

    /// An unacked, unmuted monitor at step 0 that triggered 10 minutes ago.
    async fn seed_due_monitor(db: &DatabaseConnection, org_id: i64) -> (monitor::Model, i64) {
        seed_triggered_monitor(
            db,
            org_id,
            serde_json::json!({}),
            None,
            0,
            Some(Utc::now() - chrono::Duration::minutes(10)),
            None,
        )
        .await
    }

    async fn reload(db: &DatabaseConnection, monitor_id: i64) -> monitor::Model {
        monitor::Entity::find_by_id(monitor_id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    async fn tick(db: &DatabaseConnection, redis: &redis::aio::ConnectionManager) {
        tick_with(db, redis, &Unrestricted).await;
    }

    async fn tick_with(
        db: &DatabaseConnection,
        redis: &redis::aio::ConnectionManager,
        entitlements: &dyn OrgEntitlements,
    ) {
        let queue = ValkeyQueue::new(redis.clone());
        run_due_escalations(
            db,
            entitlements,
            &queue,
            None,
            &crate::scheduler::test_support::test_storage(),
        )
        .await
        .unwrap();
    }

    async fn seed_step(
        db: &DatabaseConnection,
        org_id: i64,
        monitor_id: i64,
        position: i32,
        delay_minutes: i32,
        destination_id: i64,
    ) {
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(Utc::now());
        escalation_step::ActiveModel {
            org_id: Set(org_id),
            monitor_id: Set(monitor_id),
            position: Set(position),
            delay_minutes: Set(delay_minutes),
            destination_id: Set(destination_id),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    /// Non-destructive read of the shared `default` queue, returning only entries for `monitor_id`
    /// (other tests enqueue onto the same key).
    async fn notifications_for_monitor(
        redis: &redis::aio::ConnectionManager,
        monitor_id: i64,
    ) -> Vec<serde_json::Value> {
        let mut conn = redis.clone();
        let key = "arcodash:queue:default";
        let items: Vec<String> = conn.lrange(key, 0, -1).await.unwrap();
        items
            .iter()
            .filter_map(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .filter(|j| j["type"] == "SendNotification" && j["monitor_id"] == monitor_id)
            .collect()
    }

    /// `run_due_escalations` claims monitors system-wide, so a sibling test's call can win this
    /// monitor's step and push on its own timeline; poll rather than assert once.
    async fn poll_notifications_for_monitor(
        redis: &redis::aio::ConnectionManager,
        monitor_id: i64,
    ) -> Vec<serde_json::Value> {
        for _ in 0..50 {
            let jobs = notifications_for_monitor(redis, monitor_id).await;
            if !jobs.is_empty() {
                return jobs;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Vec::new()
    }

    async fn seed_observation(
        db: &DatabaseConnection,
        org_id: i64,
        monitor_id: i64,
        state: &str,
        age_secs: i64,
        explanation: Option<&str>,
    ) {
        exec_sql(
            db,
            "INSERT INTO monitor_observations (org_id, monitor_id, observed_at, value, state, explanation) \
             VALUES ($1, $2, now() - ($3 || ' seconds')::interval, 1, $4, $5)",
            [
                org_id.into(),
                monitor_id.into(),
                age_secs.to_string().into(),
                state.into(),
                explanation.map(str::to_string).into(),
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn escalation_resend_carries_current_incident_explanation() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-esc-expl-{suffix}")).await;
        let (mon, dest_id) = seed_due_monitor(&db, org.id).await;
        seed_step(&db, org.id, mon.id, 1, 5, dest_id).await;
        seed_observation(&db, org.id, mon.id, "triggered", 900, Some("old incident")).await;
        seed_observation(&db, org.id, mon.id, "ok", 800, None).await;
        seed_observation(&db, org.id, mon.id, "triggered", 600, Some("first")).await;
        seed_observation(&db, org.id, mon.id, "triggered", 300, Some("newest")).await;
        seed_observation(&db, org.id, mon.id, "triggered", 100, None).await;

        tick(&db, &redis).await;

        let jobs = poll_notifications_for_monitor(&redis, mon.id).await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["explanation"], "newest");
    }

    #[tokio::test]
    async fn step_one_fires_once_delay_from_last_triggered_at_has_elapsed() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-esc-step1-{suffix}")).await;
        let (mon, dest_id) = seed_due_monitor(&db, org.id).await;
        seed_step(&db, org.id, mon.id, 1, 5, dest_id).await;

        tick(&db, &redis).await;

        let reloaded = reload(&db, mon.id).await;
        assert_eq!(reloaded.escalation_step, 1);
        assert!(reloaded.last_escalated_at.is_some());

        // Scoped to this monitor: the shared "Default" queue can carry other tests' jobs.
        let jobs = poll_notifications_for_monitor(&redis, mon.id).await;
        assert_eq!(
            jobs.len(),
            1,
            "exactly one escalation notification must be enqueued"
        );
        assert_eq!(jobs[0]["type"], "SendNotification");
        assert_eq!(jobs[0]["monitor_id"], mon.id);
        assert_eq!(jobs[0]["destination_id"], dest_id);
        assert_eq!(jobs[0]["subscription_id"], serde_json::Value::Null);
        assert_eq!(jobs[0]["attempt"], 1);
    }

    #[tokio::test]
    async fn step_not_yet_due_does_not_fire() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-esc-notdue-{suffix}")).await;
        let (mon, dest_id) = seed_triggered_monitor(
            &db,
            org.id,
            serde_json::json!({}),
            None,
            0,
            Some(Utc::now()), // just triggered
            None,
        )
        .await;
        seed_step(&db, org.id, mon.id, 1, 30, dest_id).await; // 30 min delay, not elapsed

        tick(&db, &redis).await;

        let reloaded = reload(&db, mon.id).await;
        assert_eq!(reloaded.escalation_step, 0, "not due yet, must not advance");
        // Scoped to this monitor's own escalation notification: unrelated retries can land on the
        // shared "Default" queue.
        assert!(
            notifications_for_monitor(&redis, mon.id).await.is_empty(),
            "no escalation notification for this monitor must be enqueued"
        );
    }

    #[tokio::test]
    async fn second_step_fires_delay_from_last_escalated_at_not_last_triggered_at() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-esc-step2-{suffix}")).await;
        let (mon, dest_id) = seed_triggered_monitor(
            &db,
            org.id,
            serde_json::json!({}),
            None,
            1,                                                 // step 1 already fired
            Some(Utc::now() - chrono::Duration::minutes(200)), // long ago; irrelevant now
            Some(Utc::now() - chrono::Duration::minutes(10)),
        )
        .await;
        seed_step(&db, org.id, mon.id, 1, 5, dest_id).await;
        seed_step(&db, org.id, mon.id, 2, 5, dest_id).await;

        tick(&db, &redis).await;

        let reloaded = reload(&db, mon.id).await;
        assert_eq!(
            reloaded.escalation_step, 2,
            "step 2 must fire since last_escalated_at (10min ago) + 5min delay has elapsed, \
             regardless of how long ago last_triggered_at was"
        );

        // Scoped to this monitor: the shared "Default" queue can carry other tests' jobs.
        let jobs = poll_notifications_for_monitor(&redis, mon.id).await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["destination_id"], dest_id);
    }

    #[tokio::test]
    async fn acked_monitor_does_not_escalate() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-esc-acked-{suffix}")).await;
        let (mon, dest_id) = seed_triggered_monitor(
            &db,
            org.id,
            serde_json::json!({}),
            Some(Utc::now()), // acked
            0,
            Some(Utc::now() - chrono::Duration::minutes(100)),
            None,
        )
        .await;
        seed_step(&db, org.id, mon.id, 1, 1, dest_id).await;

        tick(&db, &redis).await;

        let reloaded = reload(&db, mon.id).await;
        assert_eq!(
            reloaded.escalation_step, 0,
            "an acked monitor must not escalate"
        );
        // Scoped to this monitor's own escalation notification: unrelated retries can land on the
        // shared "Default" queue.
        assert!(
            notifications_for_monitor(&redis, mon.id).await.is_empty(),
            "no escalation notification for this monitor must be enqueued"
        );
    }

    #[tokio::test]
    async fn muted_monitor_does_not_escalate() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-esc-muted-{suffix}")).await;
        let (mon, dest_id) = seed_triggered_monitor(
            &db,
            org.id,
            serde_json::json!({ "muted": true }),
            None,
            0,
            Some(Utc::now() - chrono::Duration::minutes(100)),
            None,
        )
        .await;
        seed_step(&db, org.id, mon.id, 1, 1, dest_id).await;

        tick(&db, &redis).await;

        let reloaded = reload(&db, mon.id).await;
        assert_eq!(
            reloaded.escalation_step, 0,
            "a muted monitor must not escalate"
        );
        // Scoped to this monitor's own escalation notification: unrelated retries can land on the
        // shared "Default" queue.
        assert!(
            notifications_for_monitor(&redis, mon.id).await.is_empty(),
            "no escalation notification for this monitor must be enqueued"
        );
    }

    #[tokio::test]
    async fn snoozed_monitor_does_not_escalate() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-esc-snoozed-{suffix}")).await;
        let (mon, dest_id) = seed_triggered_monitor(
            &db,
            org.id,
            serde_json::json!({
                "muted_until": (Utc::now() + chrono::Duration::minutes(30)).to_rfc3339()
            }),
            None,
            0,
            Some(Utc::now() - chrono::Duration::minutes(100)),
            None,
        )
        .await;
        seed_step(&db, org.id, mon.id, 1, 1, dest_id).await;

        tick(&db, &redis).await;

        let reloaded = reload(&db, mon.id).await;
        assert_eq!(
            reloaded.escalation_step, 0,
            "a snoozed monitor must not escalate"
        );
        // Scoped to this monitor's own escalation notification: unrelated retries can land on the
        // shared "Default" queue.
        assert!(
            notifications_for_monitor(&redis, mon.id).await.is_empty(),
            "no escalation notification for this monitor must be enqueued"
        );
    }

    /// Proves the `EscalationTickHandler` wiring end to end through a real `Conveyor::run()` loop.
    #[tokio::test]
    async fn escalation_tick_fires_through_conveyor_and_advances_the_step() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let redis_url = std::env::var("TEST_REDIS_URL").unwrap();
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-esc-conveyor-{suffix}")).await;
        let (mon, dest_id) = seed_due_monitor(&db, org.id).await;
        seed_step(&db, org.id, mon.id, 1, 5, dest_id).await;

        let ctx = Arc::new(crate::scheduler::test_support::test_ctx(
            db.clone(),
            redis.clone(),
        ));

        let task_type = format!("escalation_tick_test_{suffix}");
        let conveyor = arcodash_conveyor::ConveyorBuilder::new(db.clone(), redis, redis_url)
            .register(
                task_type.as_str(),
                crate::handlers::escalation_tick::EscalationTickHandler { ctx: ctx.clone() },
            )
            .tier("aux", &[task_type.as_str()], 2)
            .build();
        let _ = ctx.conveyor.set(conveyor.clone());

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let run_handle = {
            let conveyor = conveyor.clone();
            tokio::spawn(async move { conveyor.run(shutdown_rx).await })
        };

        let schedule_id = conveyor
            .schedule(arcodash_conveyor::ScheduleRequest {
                task_type: task_type.as_str().into(),
                entity_id: Some(1),
                payload: serde_json::json!({}),
                spec: arcodash_conveyor::schedule_spec::ScheduleSpec::Interval { every_secs: 300 },
                timezone: "UTC".to_string(),
                overlap_policy: arcodash_conveyor::OverlapPolicy::Skip,
                tenant_id: None,
                resource_key: None,
                resource_cap: None,
            })
            .await
            .unwrap();

        exec_sql(
            &db,
            "UPDATE conveyor_schedules SET next_run_at = now() - interval '5 seconds' WHERE id = $1",
            [schedule_id.0.into()],
        )
        .await;

        // 40s, not 5s: the conveyor scheduler's process-wide leader lock has a 35s TTL with no
        // release on shutdown, so a prior conveyor test can delay this claim.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
        let reloaded = loop {
            let reloaded = reload(&db, mon.id).await;
            if reloaded.escalation_step == 1 {
                break reloaded;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "escalation_tick never advanced the step within 40s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        assert_eq!(reloaded.escalation_step, 1);
        assert!(reloaded.last_escalated_at.is_some());

        let _ = shutdown_tx.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(10), run_handle)
            .await
            .unwrap()
            .unwrap();
    }

    /// The step CAS and delivery insert share one transaction: if the insert fails (forced via a
    /// primary-key collision on the next `notification_deliveries` id), the advance must roll back
    /// too.
    #[tokio::test]
    async fn failed_delivery_insert_rolls_back_the_escalation_step_advance() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-esc-txn-{suffix}")).await;
        let (mon, dest_id) = seed_due_monitor(&db, org.id).await;
        seed_step(&db, org.id, mon.id, 1, 5, dest_id).await;

        let claimed_id: i64 = query_row(
            &db,
            "SELECT nextval('notification_deliveries_id_seq') AS id",
            [],
        )
        .await
        .try_get("", "id")
        .unwrap();
        let blocked_id = claimed_id + 1;
        exec_sql(
            &db,
            "INSERT INTO notification_deliveries \
             (id, org_id, monitor_id, state, status, attempt_number, created_at) \
             VALUES ($1, $2, $3, 'triggered', 'pending', 1, now())",
            [blocked_id.into(), org.id.into(), mon.id.into()],
        )
        .await;

        tick(&db, &redis).await;

        let reloaded = reload(&db, mon.id).await;
        assert_eq!(
            reloaded.escalation_step, 0,
            "the step advance must roll back when its delivery-row insert fails, not commit alone"
        );
        assert!(
            reloaded.last_escalated_at.is_none(),
            "last_escalated_at must not be set either — same transaction, same rollback"
        );
        assert!(
            notifications_for_monitor(&redis, mon.id).await.is_empty(),
            "no notification should have been enqueued for a step that didn't actually commit"
        );
    }

    /// Proves the scan bound caps work per tick. Seeds `ESCALATION_SCAN_LIMIT + 5` due monitors
    /// with uuid-suffixed names. The query is unscoped, so other tests' monitors can consume slots:
    /// assert this batch never advances more than the limit and strictly fewer than all of it.
    /// Deletes its own org when done.
    #[tokio::test]
    async fn run_due_escalations_stops_at_the_scan_limit() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-esc-scanlimit-{suffix}")).await;

        let total = ESCALATION_SCAN_LIMIT + 5;
        let mut ids = Vec::with_capacity(total as usize);
        for _ in 0..total {
            let (mon, dest_id) = seed_due_monitor(&db, org.id).await;
            seed_step(&db, org.id, mon.id, 1, 1, dest_id).await;
            ids.push(mon.id);
        }

        tick(&db, &redis).await;

        let advanced = monitor::Entity::find()
            .filter(monitor::Column::Id.is_in(ids.clone()))
            .filter(monitor::Column::EscalationStep.eq(1))
            .all(&db)
            .await
            .unwrap()
            .len() as u64;

        organization::Entity::delete_by_id(org.id)
            .exec(&db)
            .await
            .unwrap();

        assert!(
            advanced <= ESCALATION_SCAN_LIMIT,
            "this batch alone must never advance more than the scan limit in one tick: got {advanced}"
        );
        assert!(
            advanced < total,
            "the scan limit must actually bind — some of this batch's {total} due monitors must \
             be left for the next tick, not all of them"
        );
    }

    /// The scan is due-aware: `ESCALATION_SCAN_LIMIT` low-id monitors that aren't due must not push
    /// a due, higher-id monitor out of the batch.
    #[tokio::test]
    async fn run_due_escalations_does_not_starve_a_due_high_id_monitor_behind_never_due_low_id_ones(
    ) {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-esc-starve-{suffix}")).await;

        let mut noise_ids = Vec::with_capacity(ESCALATION_SCAN_LIMIT as usize);
        for _ in 0..ESCALATION_SCAN_LIMIT {
            let (mon, dest_id) = seed_triggered_monitor(
                &db,
                org.id,
                serde_json::json!({}),
                None,
                0,
                Some(Utc::now()),
                None,
            )
            .await;
            seed_step(&db, org.id, mon.id, 1, 1000, dest_id).await;
            noise_ids.push(mon.id);
        }

        // Seeded last, so its id is the highest: a plain id-ordered scan would never reach it.
        let (due_mon, due_dest_id) = seed_due_monitor(&db, org.id).await;
        seed_step(&db, org.id, due_mon.id, 1, 1, due_dest_id).await;
        assert!(
            due_mon.id > *noise_ids.iter().max().unwrap(),
            "the due monitor must have the highest id in this batch for the test to prove anything"
        );

        tick(&db, &redis).await;

        let reloaded = reload(&db, due_mon.id).await;

        organization::Entity::delete_by_id(org.id)
            .exec(&db)
            .await
            .unwrap();

        assert_eq!(
            reloaded.escalation_step, 1,
            "the due high-id monitor must escalate even behind a full batch of never-due low-id \
             monitors"
        );
    }

    /// A blocked org's due escalation must neither advance nor enqueue, proven against a healthy
    /// org's identical monitor in the same run.
    #[tokio::test]
    async fn blocked_org_does_not_escalate_but_a_healthy_org_in_the_same_run_does() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let disabled_org = seed_org(&db, &format!("org-esc-blocked-{suffix}")).await;
        let healthy_org = seed_org(&db, &format!("org-esc-healthy-{suffix}")).await;

        let (disabled_mon, disabled_dest_id) = seed_due_monitor(&db, disabled_org.id).await;
        seed_step(
            &db,
            disabled_org.id,
            disabled_mon.id,
            1,
            5,
            disabled_dest_id,
        )
        .await;

        let (healthy_mon, healthy_dest_id) = seed_due_monitor(&db, healthy_org.id).await;
        seed_step(&db, healthy_org.id, healthy_mon.id, 1, 5, healthy_dest_id).await;

        tick_with(&db, &redis, &BlockOrg(disabled_org.id)).await;

        let reloaded_disabled = reload(&db, disabled_mon.id).await;
        assert_eq!(
            reloaded_disabled.escalation_step, 0,
            "a blocked org's due escalation must not advance"
        );
        assert!(
            notifications_for_monitor(&redis, disabled_mon.id)
                .await
                .is_empty(),
            "a blocked org's due escalation must not enqueue a notification"
        );

        let reloaded_healthy = reload(&db, healthy_mon.id).await;
        assert_eq!(
            reloaded_healthy.escalation_step, 1,
            "a healthy org's identical due escalation must still advance in the same run"
        );
        let jobs = poll_notifications_for_monitor(&redis, healthy_mon.id).await;
        assert_eq!(jobs.len(), 1);
    }

    /// A chain with a middle step removed mid-incident keeps escalating: the next step is the
    /// next-highest surviving position.
    #[tokio::test]
    async fn escalation_continues_when_a_step_is_removed_from_the_middle_of_the_chain() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-esc-midgap-{suffix}")).await;
        // Step 1 fired; position 2 was removed, so 3 is next.
        let (mon, dest_id) = seed_triggered_monitor(
            &db,
            org.id,
            serde_json::json!({}),
            None,
            1,
            Some(Utc::now() - chrono::Duration::minutes(200)),
            Some(Utc::now() - chrono::Duration::minutes(10)),
        )
        .await;
        seed_step(&db, org.id, mon.id, 1, 5, dest_id).await;
        seed_step(&db, org.id, mon.id, 3, 5, dest_id).await;

        tick(&db, &redis).await;

        let reloaded = reload(&db, mon.id).await;
        assert_eq!(
            reloaded.escalation_step, 3,
            "removing the middle step must not strand the monitor at step 1 forever — it must \
             advance to whatever step actually survives next"
        );
    }

    /// A chain shortened below the monitor's current step stops escalation; asserted explicitly.
    #[tokio::test]
    async fn escalation_stops_when_the_chain_is_shortened_below_the_current_step() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-esc-shortened-{suffix}")).await;
        let (mon, dest_id) = seed_triggered_monitor(
            &db,
            org.id,
            serde_json::json!({}),
            None,
            5,
            Some(Utc::now() - chrono::Duration::minutes(200)),
            Some(Utc::now() - chrono::Duration::minutes(100)),
        )
        .await;
        seed_step(&db, org.id, mon.id, 1, 5, dest_id).await;

        tick(&db, &redis).await;

        let reloaded = reload(&db, mon.id).await;
        assert_eq!(
            reloaded.escalation_step, 5,
            "a chain shortened below the current step must leave escalation_step untouched — \
             there is nothing left to escalate to"
        );
        assert!(
            notifications_for_monitor(&redis, mon.id).await.is_empty(),
            "no escalation notification must be enqueued when the chain has nothing left"
        );
    }
}
