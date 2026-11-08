//! Producer-side routing helper for the `evaluate_monitor` job — same shape as `notify.rs`.
//!
//! Conveyor routing only applies when `run_id` is `None`. A `run_id = Some` evaluation can still
//! park behind a query run, and parking requires the waited-on query execution to itself be a
//! conveyor task — a query run that already has a real `TaskId` to park on, not a `runs` row.
//! Until query execution is itself a conveyor task, a `Some` evaluation always takes the legacy
//! queue regardless of the flag, so parking never has to wait on a task that doesn't exist yet.

use serde::{Deserialize, Serialize};

use arcodash_conveyor::{Conveyor, EnqueueRequest};

use crate::queue::{JobMessage, Queue, QueueName};

/// The handler payload for an `evaluate_monitor` conveyor task. Only ever produced for the
/// never-parks (`run_id = None`) evaluation path — see the module doc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluateMonitorPayload {
    pub org_id: i64,
    pub monitor_id: i64,
}

/// Route `evaluate_monitor` enqueues through the conveyor SDK instead of the legacy default
/// queue. Off/unset keeps the legacy queue.
#[must_use]
pub fn conveyor_evaluate_monitor_enabled() -> bool {
    crate::config::conveyor_flag("CONVEYOR_EVALUATE_MONITOR_ENABLED")
}

/// Enqueue one `evaluate_monitor`.
///
/// Routes through conveyor iff `run_id` is `None` AND `conveyor` is `Some` AND
/// [`conveyor_evaluate_monitor_enabled`]; otherwise the legacy default queue. A `Some(run_id)`
/// evaluation always takes the legacy branch regardless of the flag or whether a conveyor handle is
/// available — see the module doc. The legacy branch reproduces today's
/// `JobMessage::EvaluateMonitor` verbatim, so the flag-off wire output is unchanged.
pub async fn enqueue_evaluate_monitor(
    queue: &dyn Queue,
    conveyor: Option<&Conveyor>,
    org_id: i64,
    monitor_id: i64,
    run_id: Option<i64>,
) -> anyhow::Result<()> {
    if let Some(c) = conveyor.filter(|_| run_id.is_none() && conveyor_evaluate_monitor_enabled()) {
        let payload = serde_json::to_value(EvaluateMonitorPayload { org_id, monitor_id })?;
        // `entity_id = monitor_id`, not a per-attempt id: one evaluation in flight per
        // monitor at a time is the real invariant here, unlike notification fan-out where
        // the same monitor legitimately has N independent concurrent deliveries.
        let enqueued = crate::query_exec::enqueue_unless_inflight(
            c,
            EnqueueRequest {
                task_type: "evaluate_monitor".into(),
                payload,
                tenant_id: Some(org_id),
                entity_id: Some(monitor_id),
                resource_key: None,
                resource_cap: None,
            },
        )
        .await?;
        // An evaluation for this monitor is already pending|running. One evaluation in flight per
        // monitor at a time is the desired invariant here, not a failure — a no-op, but worth a
        // warn! rather than a silent swallow so an unexpectedly frequent hit is visible.
        if enqueued.is_none() {
            tracing::warn!(org_id, monitor_id, "enqueue_evaluate_monitor: conveyor task already in flight for this monitor, skipping enqueue");
        }
        return Ok(());
    }
    queue
        .enqueue(
            QueueName::Default,
            JobMessage::EvaluateMonitor {
                org_id,
                monitor_id,
                run_id,
            },
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::RecordingQueue;

    /// `CONVEYOR_EVALUATE_MONITOR_ENABLED` is process-global, so tests that set it must not run
    /// concurrently; held across `.await`, hence `tokio::sync::Mutex`.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn legacy_branch_reproduces_exact_evaluate_monitor_shape() {
        let queue = RecordingQueue::default();
        enqueue_evaluate_monitor(&queue, None, 1, 2, Some(3))
            .await
            .unwrap();

        let sent = queue.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let (queue_name, job) = &sent[0];
        assert_eq!(*queue_name, QueueName::Default);
        match job {
            JobMessage::EvaluateMonitor {
                org_id,
                monitor_id,
                run_id,
            } => {
                assert_eq!(*org_id, 1);
                assert_eq!(*monitor_id, 2);
                assert_eq!(*run_id, Some(3));
            }
            other => panic!("expected EvaluateMonitor, got {other:?}"),
        }
    }

    #[test]
    fn payload_round_trips_through_json() {
        let payload = EvaluateMonitorPayload {
            org_id: 1,
            monitor_id: 2,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: EvaluateMonitorPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.org_id, payload.org_id);
        assert_eq!(parsed.monitor_id, payload.monitor_id);
    }

    // One test, not two: a process-global env var can't safely split across parallel tests.
    #[test]
    fn enabled_reflects_the_env_var() {
        let _guard = ENV_LOCK.blocking_lock();
        std::env::remove_var("CONVEYOR_EVALUATE_MONITOR_ENABLED");
        assert!(!conveyor_evaluate_monitor_enabled());
        for (value, enabled) in [
            ("false", false),
            ("0", false),
            ("true", true),
            ("1", true),
            ("TRUE", true),
            (" 1 ", true),
        ] {
            std::env::set_var("CONVEYOR_EVALUATE_MONITOR_ENABLED", value);
            assert_eq!(
                conveyor_evaluate_monitor_enabled(),
                enabled,
                "value {value:?}"
            );
        }
        std::env::remove_var("CONVEYOR_EVALUATE_MONITOR_ENABLED");
    }

    /// Real Postgres + Redis only (skips cleanly without `TEST_DATABASE_URL`/`TEST_REDIS_URL`):
    /// proves a `Some(run_id)` evaluation takes the legacy queue even with a real `Conveyor` and
    /// the flag on — the function must short-circuit before ever calling `conveyor.enqueue()`.
    /// Holds [`ENV_LOCK`]; the flag is restored at the end.
    #[tokio::test]
    async fn some_run_id_always_takes_the_legacy_path_even_with_a_real_conveyor_and_the_flag_on() {
        let Some((db, valkey, redis_url)) = crate::test_support::test_db_and_valkey().await else {
            return;
        };

        let _guard = ENV_LOCK.lock().await;
        std::env::set_var("CONVEYOR_EVALUATE_MONITOR_ENABLED", "true");

        let conveyor =
            arcodash_conveyor::ConveyorBuilder::new(db.clone(), valkey.clone(), redis_url).build();

        let queue_impl = crate::queue::ValkeyQueue::new(valkey.clone());
        let key = QueueName::Default.queue_key();
        let mut conn = valkey.clone();
        use redis::AsyncCommands;
        while conn
            .lpop::<_, Option<String>>(&key, None)
            .await
            .unwrap()
            .is_some()
        {}

        let org_id = 111_222;
        let monitor_id = 333_444;
        let some_run_id = 555_666;
        enqueue_evaluate_monitor(
            &queue_impl,
            Some(&conveyor),
            org_id,
            monitor_id,
            Some(some_run_id),
        )
        .await
        .unwrap();

        std::env::remove_var("CONVEYOR_EVALUATE_MONITOR_ENABLED");

        let payload: String = conn
            .lpop::<_, Option<String>>(&key, None)
            .await
            .unwrap()
            .expect("a Some(run_id) evaluation must still land on the legacy queue");
        let message: JobMessage = serde_json::from_str(&payload).unwrap();
        match message {
            JobMessage::EvaluateMonitor {
                org_id: got_org_id,
                monitor_id: got_monitor_id,
                run_id: got_run_id,
            } => {
                assert_eq!(got_org_id, org_id);
                assert_eq!(got_monitor_id, monitor_id);
                assert_eq!(
                    got_run_id,
                    Some(some_run_id),
                    "the legacy message must carry the same run_id through"
                );
            }
            other => panic!("expected EvaluateMonitor, got {other:?}"),
        }
    }
}
