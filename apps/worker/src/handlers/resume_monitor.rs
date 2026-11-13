//! Resumes a monitor run parked on an in-flight query run: evaluates the fresh result on success,
//! fails the monitor run on failure/timeout (never evaluates stale data).

use sea_orm::EntityTrait;

use arcodash_core::entities::{monitor, run};
use arcodash_core::entitlements::WorkGate;

use crate::handlers::evaluate_monitor::{
    evaluate_and_finalize, finalize_terminal, query_recently_unpublished, resolve_monitor_result,
};
use crate::handlers::execute_query::ExecuteQueryContext;
use crate::run_lifecycle::RunStatus;

pub async fn handle(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    run_id: i64,
    query_run_id: i64,
    query_run_status: String,
) {
    if let Err(e) = resume(ctx, org_id, run_id, query_run_id, &query_run_status).await {
        if crate::handlers::evaluate_monitor::is_storage_switching(&e) {
            crate::handlers::evaluate_monitor::defer_for_storage_switch(ctx, run_id).await;
            return;
        }
        tracing::error!(run_id, error = %e, "resume_monitor failed");
    }
}

pub async fn resume(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    run_id: i64,
    query_run_id: i64,
    query_run_status: &str,
) -> anyhow::Result<()> {
    // The run is already 'running' (never call start_run); act only while it still is, so a
    // double-delivered resume is a no-op. The org check stops a mismatched run_id/org_id pairing
    // from finalizing another org's run.
    let Some(run) = run::Entity::find_by_id(run_id).one(&ctx.db).await? else {
        return Ok(());
    };
    if run.org_id != org_id {
        tracing::error!(
            run_id,
            job_org_id = org_id,
            run_org_id = run.org_id,
            "resume_monitor: run belongs to a different org — refusing to act on it"
        );
        return Ok(());
    }
    if run.status != "running" {
        return Ok(());
    }

    if let WorkGate::Blocked { reason } = ctx.entitlements.scheduled_work(&ctx.db, org_id).await {
        tracing::info!(org_id, reason, "skipping scheduled work");
        // Leaving it 'running' would hold `uq_runs_inflight` until a sweep expires the lease.
        return finalize_terminal(ctx, run_id, RunStatus::Skipped, Some(reason.to_string())).await;
    }

    if query_run_status != "success" {
        // Any other terminal state means no fresh data: fail the monitor run.
        let query_error = run::Entity::find_by_id(query_run_id)
            .one(&ctx.db)
            .await?
            .filter(|r| r.org_id == org_id)
            .and_then(|r| r.error)
            .filter(|e| !e.is_empty());
        let message = query_error.map_or_else(
            || format!("underlying query run #{query_run_id} failed ({query_run_status})"),
            |e| {
                format!(
                    "query run #{query_run_id} {query_run_status}: {}",
                    e.chars().take(300).collect::<String>()
                )
            },
        );
        return finalize_terminal(ctx, run_id, RunStatus::Failed, Some(message)).await;
    }

    let Some(monitor) = monitor::Entity::find_by_id(run.entity_id)
        .one(&ctx.db)
        .await?
        .filter(|m| m.org_id == org_id)
    else {
        return finalize_terminal(ctx, run_id, RunStatus::Success, None).await;
    };

    // Re-check the snooze and draft gates: either may have changed while parked.
    let options = arcodash_core::models::parse_monitor_options(monitor.id, &monitor.options);
    if options.is_silenced(chrono::Utc::now())
        || query_recently_unpublished(&ctx.db, monitor.query_id, org_id).await?
    {
        return finalize_terminal(ctx, run_id, RunStatus::Success, None).await;
    }

    let resolved = resolve_monitor_result(&ctx.db, &monitor, org_id).await?;
    let evaluated = resolved.map(|(id, ts)| (id, (chrono::Utc::now() - ts).num_seconds().max(0)));
    evaluate_and_finalize(ctx, org_id, &monitor, run_id, evaluated).await
}

#[cfg(test)]
mod tests {
    //! Real Postgres + Redis only (skips cleanly without `TEST_DATABASE_URL`/`TEST_REDIS_URL`).
    use super::*;
    use crate::scheduler::test_support::*;
    use arcodash_core::entities::{monitor_subscription, notification_delivery};
    use arcodash_core::queue;
    use chrono::Utc;

    use sea_orm::{ActiveModelTrait, ActiveValue::Set, ConnectionTrait, DbBackend, Statement};

    async fn exec(db: &sea_orm::DatabaseConnection, sql: &str, id: i64) {
        exec_sql(db, sql, [id.into()]).await;
    }

    async fn monitor_state(db: &sea_orm::DatabaseConnection, id: i64) -> String {
        monitor::Entity::find_by_id(id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
            .state
    }

    /// A fresh org with a published query and a `value > 50` monitor on it.
    async fn seed_watched(
        db: &sea_orm::DatabaseConnection,
    ) -> (
        i64,
        i64,
        arcodash_core::entities::query::Model,
        monitor::Model,
    ) {
        let org = seed_org(db, &format!("org-resume-{}", uuid::Uuid::new_v4())).await;
        let ds = seed_data_source(db, org.id).await;
        let q = seed_published_query(db, org.id, ds.id).await;
        let mon = seed_monitor(
            db,
            org.id,
            q.id,
            q.user_id,
            serde_json::json!({"column": "value", "op": ">", "value": 50.0}),
        )
        .await;
        (org.id, ds.id, q, mon)
    }

    /// Points the query's published result at a fresh one holding `value`; returns its id.
    async fn publish_result(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        ds_id: i64,
        query_id: i64,
        value: f64,
    ) -> i64 {
        let row = serde_json::json!({ "value": value });
        let qr = seed_query_result(db, org_id, ds_id, Utc::now(), row).await;
        set_published_latest_query_data(db, query_id, qr.id).await;
        qr.id
    }

    /// An `ok` monitor with an email subscriber whose published result just crossed the threshold
    /// (10, then 100), parked behind a finished query run. Returns `(org_id, monitor_id,
    /// above_result_id, waiter_run_id)`.
    async fn seed_crossing_waiter(db: &sea_orm::DatabaseConnection) -> (i64, i64, i64, i64) {
        let (org_id, ds_id, q, mon) = seed_watched(db).await;
        publish_result(db, org_id, ds_id, q.id, 10.0).await;
        exec(db, "UPDATE monitors SET state = 'ok' WHERE id = $1", mon.id).await;

        let now = sea_orm::prelude::DateTimeWithTimeZone::from(Utc::now());
        let dest = arcodash_core::entities::notification_destination::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("resume-test-dest-{}", uuid::Uuid::new_v4())),
            r#type: Set("email".to_string()),
            options: Set(None),
            user_id: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
        monitor_subscription::ActiveModel {
            monitor_id: Set(mon.id),
            user_id: Set(q.user_id),
            destination_id: Set(Some(dest.id)),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();

        let above = publish_result(db, org_id, ds_id, q.id, 100.0).await;
        let waiter_id = seed_parked_waiter(db, org_id, mon.id, None).await;
        (org_id, mon.id, above, waiter_id)
    }

    /// Park a `running` monitor run with `waiting_on_run_id` already cleared.
    async fn seed_parked_waiter(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        monitor_id: i64,
        schedule_id: Option<i64>,
    ) -> i64 {
        seed_run(
            db,
            org_id,
            "monitor",
            monitor_id,
            schedule_id,
            None,
            "running",
            None,
        )
        .await
    }

    #[tokio::test]
    async fn resume_success_evaluates_fresh_data_transitions_and_notifies() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon_id, above, waiter_id) = seed_crossing_waiter(&db).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        resume(&ctx, org_id, waiter_id, 999, "success")
            .await
            .unwrap();

        assert_eq!(monitor_state(&db, mon_id).await, "triggered");

        let run = load_run(&db, waiter_id).await;
        assert_eq!(run.status, "success");
        assert_eq!(run.query_result_id, Some(above));
        assert!(run.data_age_secs.is_some());
        assert_eq!(
            run.condition_met,
            Some(true),
            "the resumed evaluation crossed the threshold (100 > 50) — must record condition_met = true"
        );

        let key = queue::QueueName::Default.queue_key();
        let job = find_and_remove_notification_for_monitor(&redis, &key, mon_id)
            .await
            .expect("a SendNotification must have been enqueued");
        match job {
            queue::JobMessage::SendNotification {
                run_id: wire_delivery_id,
                new_state,
                ..
            } => {
                assert_eq!(new_state, "triggered");
                // The wire `run_id` is the delivery row's id; the evaluation run id (`waiter_id`)
                // is in its `run_id` column.
                let delivery_id = wire_delivery_id
                    .expect("the producer must pre-create a delivery row and carry its id");
                let delivery = notification_delivery::Entity::find_by_id(delivery_id)
                    .one(&db)
                    .await
                    .unwrap()
                    .expect("the pre-created delivery row must exist");
                assert_eq!(delivery.run_id, Some(waiter_id));
                assert_eq!(delivery.status, "pending");
            }
            other => panic!("expected SendNotification, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resume_failed_carries_the_query_runs_error() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, ds_id, q, mon) = seed_watched(&db).await;
        let waiter_id = seed_parked_waiter(&db, org_id, mon.id, None).await;
        let query_run_id = seed_run(
            &db,
            org_id,
            "query",
            q.id,
            None,
            Some(ds_id),
            "failed",
            None,
        )
        .await;
        let long = "x".repeat(1000);
        for msg in [
            "invalid Postgres connection options: missing field `host`",
            long.as_str(),
        ] {
            exec_sql(
                &db,
                "UPDATE runs SET error = $1 WHERE id = $2",
                [msg.into(), query_run_id.into()],
            )
            .await;
            exec(
                &db,
                "UPDATE runs SET status = 'running', error = NULL WHERE id = $1",
                waiter_id,
            )
            .await;

            let ctx = test_ctx(db.clone(), redis.clone());
            resume(&ctx, org_id, waiter_id, query_run_id, "failed")
                .await
                .unwrap();

            let error = load_run(&db, waiter_id).await.error.unwrap();
            let prefix = format!("query run #{query_run_id} failed: ");
            assert!(error.starts_with(&prefix), "got {error}");
            assert!(error.chars().count() <= prefix.chars().count() + 300);
            assert!(error.contains(&msg.chars().take(50).collect::<String>()));
        }
    }

    #[tokio::test]
    async fn resume_failed_fails_the_run_and_increments_consecutive_failures() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, _, _, mon) = seed_watched(&db).await;
        let sched = seed_schedule(
            &db,
            org_id,
            "monitor",
            mon.id,
            serde_json::json!({"kind": "interval", "every_secs": 300}),
            "skip",
            Utc::now(),
            0,
        )
        .await;
        let waiter_id = seed_parked_waiter(&db, org_id, mon.id, Some(sched.id)).await;

        let ctx = test_ctx(db.clone(), redis);
        resume(&ctx, org_id, waiter_id, 7, "failed").await.unwrap();

        let run = load_run(&db, waiter_id).await;
        assert_eq!(run.status, "failed");
        assert_eq!(
            run.error.as_deref(),
            Some("underlying query run #7 failed (failed)"),
            "a missing query run falls back to the status-only message"
        );
        assert_eq!(
            run.condition_met, None,
            "a failed refresh never resolves a condition"
        );

        let reloaded_sched = arcodash_core::entities::schedule::Entity::find_by_id(sched.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded_sched.consecutive_failures, 1);
    }

    /// A job whose `org_id` differs from the org owning `run_id` must be refused, not finalize
    /// another org's run.
    #[tokio::test]
    async fn resume_refuses_a_run_belonging_to_a_different_org() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let _db_lock = acquire_shared_test_db_lock(
            &std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL required"),
        )
        .await;
        let (owner_org_id, _, _, mon) = seed_watched(&db).await;
        let attacker_org = seed_org(
            &db,
            &format!("org-resume-attacker-{}", uuid::Uuid::new_v4()),
        )
        .await;
        let waiter_id = seed_parked_waiter(&db, owner_org_id, mon.id, None).await;

        let ctx = test_ctx(db.clone(), redis);
        // `org_id` here is the attacker org, but `run_id` names a run owned by the owner org.
        resume(&ctx, attacker_org.id, waiter_id, 999, "success")
            .await
            .unwrap();

        assert_eq!(
            load_run(&db, waiter_id).await.status,
            "running",
            "a cross-org resume must leave the run untouched, not finalize it"
        );
    }

    /// Double delivery: two concurrent `resume(success)` calls yield exactly one notify and one
    /// finalize.
    #[tokio::test]
    async fn concurrent_double_delivered_resume_notifies_and_finalizes_exactly_once() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, mon_id, _, waiter_id) = seed_crossing_waiter(&db).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        let ctx2 = test_ctx(db.clone(), redis.clone());
        let (r1, r2) = tokio::join!(
            resume(&ctx, org_id, waiter_id, 999, "success"),
            resume(&ctx2, org_id, waiter_id, 999, "success"),
        );
        r1.unwrap();
        r2.unwrap();

        // Scoped to this monitor: the shared queue key can carry other tests' jobs.
        let key = queue::QueueName::Default.queue_key();
        let mut notifications = 0;
        while find_and_remove_notification_for_monitor(&redis, &key, mon_id)
            .await
            .is_some()
        {
            notifications += 1;
        }
        assert_eq!(
            notifications, 1,
            "a double-delivered resume must notify exactly once"
        );
        assert_eq!(monitor_state(&db, mon_id).await, "triggered");
    }

    /// A blocked org's parked run finalizes as `skipped` and leaves monitor state untouched.
    #[tokio::test]
    async fn resume_of_a_blocked_org_skips_evaluation_and_finalizes_the_run() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let (org_id, _, _, mon) = seed_watched(&db).await;
        let waiter_id = seed_parked_waiter(&db, org_id, mon.id, None).await;

        let mut ctx = test_ctx(db.clone(), redis);
        ctx.entitlements = std::sync::Arc::new(BlockedByTest);
        resume(&ctx, org_id, waiter_id, 999, "success")
            .await
            .unwrap();

        assert_eq!(
            monitor_state(&db, mon.id).await,
            "unknown",
            "a blocked org's resume must not evaluate the monitor"
        );
        let run = load_run(&db, waiter_id).await;
        assert_eq!(run.status, "skipped");
        assert_eq!(
            run.error.as_deref(),
            Some("blocked by test"),
            "the skip reason must be recorded on the run"
        );
    }

    /// An `ok` monitor over a triggering (100 > 50) published result, with the `gate` statement
    /// applied while parked, resumed after a successful refresh. Returns `(monitor state, run
    /// status, whether a notification was queued)`.
    async fn resume_after_gate(
        gate: impl Fn(&monitor::Model) -> Statement,
    ) -> Option<(String, String, bool)> {
        let (db, redis) = setup().await?;
        let (org_id, ds_id, q, mon) = seed_watched(&db).await;
        publish_result(&db, org_id, ds_id, q.id, 100.0).await;
        exec(
            &db,
            "UPDATE monitors SET state = 'ok' WHERE id = $1",
            mon.id,
        )
        .await;
        db.execute_raw(gate(&mon)).await.unwrap();

        let waiter_id = seed_parked_waiter(&db, org_id, mon.id, None).await;
        let ctx = test_ctx(db.clone(), redis.clone());
        resume(&ctx, org_id, waiter_id, 999, "success")
            .await
            .unwrap();

        let key = queue::QueueName::Default.queue_key();
        let notified = find_and_remove_notification_for_monitor(&redis, &key, mon.id)
            .await
            .is_some();
        Some((
            monitor_state(&db, mon.id).await,
            load_run(&db, waiter_id).await.status,
            notified,
        ))
    }

    /// A monitor snoozed while parked must not evaluate on resume.
    #[tokio::test]
    async fn resume_does_not_evaluate_a_monitor_snoozed_while_parked() {
        let Some((state, status, notified)) = resume_after_gate(|m| {
            let future = (Utc::now() + chrono::Duration::minutes(30)).to_rfc3339();
            Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE monitors SET options = options || jsonb_build_object('muted_until', $1::text) \
                 WHERE id = $2",
                [future.into(), m.id.into()],
            )
        })
        .await
        else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        assert_eq!(
            state, "ok",
            "a monitor snoozed while parked must not transition on resume"
        );
        assert_eq!(status, "success");
        assert!(
            !notified,
            "a snoozed monitor's resume must not enqueue a notification"
        );
    }

    /// A monitor whose query went back to draft while parked must not evaluate on resume.
    #[tokio::test]
    async fn resume_does_not_evaluate_a_monitor_whose_query_was_unpublished_while_parked() {
        let Some((state, status, notified)) = resume_after_gate(|m| {
            Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE queries SET unpublished_at = now() WHERE id = $1",
                [m.query_id.into()],
            )
        })
        .await
        else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        assert_eq!(
            state, "ok",
            "a monitor whose query was unpublished while parked must not transition on resume"
        );
        assert_eq!(status, "success");
        assert!(
            !notified,
            "an unpublished-mid-wait monitor's resume must not enqueue a notification"
        );
    }
}
