//! Wraps the escalation tick (`scheduler::escalations::run_due_escalations`) as a
//! Conveyor-scheduled task.

use std::sync::Arc;

use async_trait::async_trait;

use arcodash_conveyor::{TaskContext, TaskHandler, TaskOutcome, TaskResult};

use crate::handlers::execute_query::ExecuteQueryContext;
use crate::scheduler::escalations::run_due_escalations;

pub struct EscalationTickHandler {
    pub ctx: Arc<ExecuteQueryContext>,
}

#[async_trait]
impl TaskHandler for EscalationTickHandler {
    async fn run(&self, _task: TaskContext, _payload: serde_json::Value) -> TaskResult {
        let summary = run_due_escalations(
            &self.ctx.db,
            self.ctx.entitlements.as_ref(),
            self.ctx.queue.as_ref(),
            self.ctx.conveyor.get().map(std::convert::AsRef::as_ref),
        )
        .await?;
        if summary.failed > 0 {
            // Any per-item failure fails the tick: auto-pause exists to stop a schedule that is
            // failing persistently, and a partial failure that resolves next tick resets the
            // streak anyway (the streak, not one failure, is what pauses).
            return Err(anyhow::anyhow!(
                "{} of {} due escalations failed",
                summary.failed,
                summary.due
            ));
        }
        Ok(TaskOutcome::Success { result_ref: None })
    }
}

#[cfg(test)]
mod tests {
    //! Real Postgres + Redis only (skips cleanly without `TEST_DATABASE_URL`/`TEST_REDIS_URL`).
    use super::*;
    use crate::scheduler::test_support::{
        exec_sql, query_row, seed_data_source, seed_org, seed_query, setup, test_ctx,
    };
    use arcodash_core::entities::{escalation_step, monitor, notification_destination};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};

    fn task_ctx() -> TaskContext {
        TaskContext {
            task_id: arcodash_conveyor::TaskId(0),
            task_type: "escalation_tick".into(),
            attempt: 1,
            tenant_id: None,
            entity_id: None,
        }
    }

    /// Seed a due, triggered, unacked monitor with one escalation step — everything
    /// `run_due_escalations` needs to attempt a step advance for it.
    async fn seed_due_monitor(db: &sea_orm::DatabaseConnection, org_id: i64) -> monitor::Model {
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
        let suffix = uuid::Uuid::new_v4();

        let ds = seed_data_source(db, org_id).await;
        let q = seed_query(db, org_id, ds.id).await;

        let dest = notification_destination::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("escalation-tick-test-dest-{suffix}")),
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
            name: Set(format!("escalation-tick-test-monitor-{suffix}")),
            query_id: Set(q.id),
            user_id: Set(q.user_id),
            options: Set(serde_json::json!({})),
            state: Set("triggered".to_string()),
            last_triggered_at: Set(Some(
                (chrono::Utc::now() - chrono::Duration::minutes(10)).into(),
            )),
            rearm: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            acked_at: Set(None),
            acked_by: Set(None),
            escalation_step: Set(0),
            last_escalated_at: Set(None),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();

        escalation_step::ActiveModel {
            org_id: Set(org_id),
            monitor_id: Set(mon.id),
            position: Set(1),
            delay_minutes: Set(1),
            destination_id: Set(dest.id),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();

        mon
    }

    #[tokio::test]
    async fn handler_reports_success_when_nothing_was_due() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let ctx = Arc::new(test_ctx(db, redis));
        let handler = EscalationTickHandler { ctx };
        let outcome = handler.run(task_ctx(), serde_json::json!({})).await;
        match outcome {
            Ok(TaskOutcome::Success { .. }) => {}
            Ok(_) => panic!("a tick with nothing due must report Success"),
            Err(e) => panic!("a tick with nothing due must not fail: {e}"),
        }
    }

    /// Forces a genuine per-monitor failure (a delivery-row insert collision, same technique
    /// `escalations.rs`'s own transaction-rollback test uses) and proves the handler surfaces it as
    /// a failed `TaskResult` rather than reporting `Success` — otherwise `auto_pause_threshold`
    /// could never fire for escalation schedules, because every tick would report success
    /// regardless of what happened inside it.
    #[tokio::test]
    async fn handler_reports_failure_when_the_body_fails() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-esc-tick-fail-{suffix}")).await;
        let mon = seed_due_monitor(&db, org.id).await;

        // Pre-claim the id `insert_pending`'s own insert will collide on, forcing the
        // delivery-insert step (inside the transaction) to fail deterministically.
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

        let ctx = Arc::new(test_ctx(db, redis));
        let handler = EscalationTickHandler { ctx };
        let outcome = handler.run(task_ctx(), serde_json::json!({})).await;
        assert!(
            outcome.is_err(),
            "a tick where a due escalation's delivery insert fails must report failure"
        );
    }
}
