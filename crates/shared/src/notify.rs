//! Producer-side helpers for the `send_notification` job.
//!
//! One payload type shared by every
//! producer and the conveyor-side handler (so they can never drift), plus a routing helper that
//! picks the conveyor path or the legacy default queue based on whether a `Conveyor` handle was
//! passed in.

use std::sync::Arc;

use redis::aio::ConnectionManager;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};

use arcodash_conveyor::{Conveyor, ConveyorBuilder, EnqueueRequest};

use crate::queue::{JobMessage, Queue, QueueName};

/// The handler payload for a `send_notification` conveyor task. Does NOT carry the delivery id —
/// that is the task's `entity_id` (fan-out granularity), read from `TaskContext.entity_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendNotificationPayload {
    pub org_id: i64,
    pub monitor_id: i64,
    pub destination_id: i64,
    pub new_state: String,
    pub subscriber_id: i64,
    #[serde(default)]
    pub subscription_id: Option<i64>,
    #[serde(default = "crate::queue::default_attempt")]
    pub attempt: u32,
    /// The value that decided this transition, captured at evaluation time — `None` for a
    /// payload enqueued before this field existed, in which case the handler falls back to
    /// re-resolving the monitor's current result at send time.
    #[serde(default)]
    pub observed: Option<String>,
    /// Root-cause narrative; `None` for a payload enqueued before this field existed.
    #[serde(default)]
    pub explanation: Option<String>,
}

/// One notification enqueue's inputs. `delivery_id` is the pre-created `notification_deliveries`
/// row id: the conveyor task's `entity_id`, and the legacy message's `run_id`.
pub struct SendNotificationParams {
    pub org_id: i64,
    pub monitor_id: i64,
    pub destination_id: i64,
    pub new_state: String,
    pub subscriber_id: i64,
    pub subscription_id: Option<i64>,
    pub attempt: u32,
    pub delivery_id: Option<i64>,
    /// See [`SendNotificationPayload::observed`].
    pub observed: Option<String>,
    /// See [`SendNotificationPayload::explanation`].
    pub explanation: Option<String>,
}

/// Route `send_notification` enqueues through the conveyor SDK instead of the legacy
/// default queue. Off/unset keeps the legacy queue.
#[must_use]
pub fn conveyor_send_notification_enabled() -> bool {
    crate::config::conveyor_flag("CONVEYOR_SEND_NOTIFICATION_ENABLED")
}

/// A producer-only conveyor (no tiers, never runs a consumer/scheduler), or `None` when the flag is
/// off.
///
/// `redis_url` is empty because a producer never opens the dedicated BLPOP/pubsub connections that
/// field feeds.
#[must_use]
pub fn producer_conveyor_if_enabled(
    db: &DatabaseConnection,
    valkey: &ConnectionManager,
) -> Option<Arc<Conveyor>> {
    conveyor_send_notification_enabled()
        .then(|| ConveyorBuilder::new(db.clone(), valkey.clone(), String::new()).build())
}

/// Enqueue one `send_notification`.
///
/// Routes through conveyor iff `conveyor` is `Some` AND [`conveyor_send_notification_enabled`];
/// otherwise the legacy default queue. Self-gating on the flag (not just `conveyor.is_some()`)
/// matters once the worker can hold a `Conveyor` handle for a *different* task type's flag — that
/// handle alone must not reroute this one. The legacy branch reproduces today's
/// `JobMessage::SendNotification` verbatim, so the flag-off wire output is unchanged.
pub async fn enqueue_send_notification(
    queue: &dyn Queue,
    conveyor: Option<&Conveyor>,
    p: SendNotificationParams,
) -> anyhow::Result<()> {
    match conveyor.filter(|_| conveyor_send_notification_enabled()) {
        Some(c) => {
            let payload = serde_json::to_value(SendNotificationPayload {
                org_id: p.org_id,
                monitor_id: p.monitor_id,
                destination_id: p.destination_id,
                new_state: p.new_state,
                subscriber_id: p.subscriber_id,
                subscription_id: p.subscription_id,
                attempt: p.attempt,
                observed: p.observed,
                explanation: p.explanation,
            })?;
            let enqueued = crate::query_exec::enqueue_unless_inflight(
                c,
                EnqueueRequest {
                    task_type: "send_notification".into(),
                    payload,
                    tenant_id: Some(p.org_id),
                    entity_id: p.delivery_id,
                    resource_key: None,
                    resource_cap: None,
                },
            )
            .await?;
            match enqueued {
                Some(res) if !res.queued => {
                    tracing::warn!(monitor_id = p.monitor_id, delivery_id = ?p.delivery_id, "enqueue_send_notification: conveyor reported the task already in flight, not actually queued");
                }
                Some(_) => {}
                // A send for this delivery id is already pending|running — most often the retry
                // re-enqueue racing the still-running attempt that scheduled it. Not a failure:
                // the in-flight attempt owns the outcome, so this is a no-op, not an error.
                None => {
                    tracing::warn!(monitor_id = p.monitor_id, delivery_id = ?p.delivery_id, "enqueue_send_notification: conveyor task already in flight for this delivery, skipping enqueue");
                }
            }
            Ok(())
        }
        None => {
            queue
                .enqueue(
                    QueueName::Default,
                    JobMessage::SendNotification {
                        org_id: p.org_id,
                        monitor_id: p.monitor_id,
                        destination_id: p.destination_id,
                        new_state: p.new_state,
                        subscriber_id: p.subscriber_id,
                        subscription_id: p.subscription_id,
                        attempt: p.attempt,
                        run_id: p.delivery_id,
                        observed: p.observed,
                        explanation: p.explanation,
                    },
                )
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::RecordingQueue;
    use async_trait::async_trait;

    /// `CONVEYOR_SEND_NOTIFICATION_ENABLED` is process-global, so tests that set it must not run
    /// concurrently; held across `.await`, hence `tokio::sync::Mutex`.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn legacy_branch_reproduces_exact_send_notification_shape() {
        let queue = RecordingQueue::default();
        enqueue_send_notification(
            &queue,
            None,
            SendNotificationParams {
                org_id: 1,
                monitor_id: 2,
                destination_id: 3,
                new_state: "triggered".to_string(),
                subscriber_id: 4,
                subscription_id: Some(5),
                attempt: 1,
                delivery_id: Some(6),
                observed: Some("31,204.00".to_string()),
                explanation: Some("why".to_string()),
            },
        )
        .await
        .unwrap();

        let sent = queue.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let (queue_name, job) = &sent[0];
        assert_eq!(*queue_name, QueueName::Default);
        match job {
            JobMessage::SendNotification {
                org_id,
                monitor_id,
                destination_id,
                new_state,
                subscriber_id,
                subscription_id,
                attempt,
                run_id,
                observed,
                explanation,
            } => {
                assert_eq!(*org_id, 1);
                assert_eq!(*monitor_id, 2);
                assert_eq!(*destination_id, 3);
                assert_eq!(new_state, "triggered");
                assert_eq!(*subscriber_id, 4);
                assert_eq!(*subscription_id, Some(5));
                assert_eq!(*attempt, 1);
                assert_eq!(
                    *run_id,
                    Some(6),
                    "delivery_id must land on the wire as run_id"
                );
                assert_eq!(observed.as_deref(), Some("31,204.00"));
                assert_eq!(explanation.as_deref(), Some("why"));
            }
            other => panic!("expected SendNotification, got {other:?}"),
        }
    }

    #[test]
    fn payload_round_trips_through_json() {
        let payload = SendNotificationPayload {
            org_id: 1,
            monitor_id: 2,
            destination_id: 3,
            new_state: "triggered".to_string(),
            subscriber_id: 4,
            subscription_id: Some(5),
            attempt: 2,
            observed: Some("12,480.55".to_string()),
            explanation: Some("why".to_string()),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: SendNotificationPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.org_id, payload.org_id);
        assert_eq!(parsed.monitor_id, payload.monitor_id);
        assert_eq!(parsed.destination_id, payload.destination_id);
        assert_eq!(parsed.new_state, payload.new_state);
        assert_eq!(parsed.subscriber_id, payload.subscriber_id);
        assert_eq!(parsed.subscription_id, payload.subscription_id);
        assert_eq!(parsed.attempt, payload.attempt);
        assert_eq!(parsed.observed, payload.observed);
        assert_eq!(parsed.explanation, payload.explanation);
    }

    #[test]
    fn payload_without_explanation_deserializes() {
        let json = serde_json::json!({
            "org_id": 1,
            "monitor_id": 2,
            "destination_id": 3,
            "new_state": "triggered",
            "subscriber_id": 4,
        });
        let parsed: SendNotificationPayload = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.explanation, None);
    }

    /// Mirrors `JobMessage::SendNotification`'s `default_attempt` tolerance — a conveyor task row
    /// is retained 14 days, so a payload written by an old binary missing this field is a real,
    /// days-long window, unlike the legacy queue's seconds-long one.
    #[test]
    fn send_notification_payload_defaults_attempt_when_absent() {
        let json = serde_json::json!({
            "org_id": 1,
            "monitor_id": 2,
            "destination_id": 3,
            "new_state": "triggered",
            "subscriber_id": 4,
        });
        let parsed: SendNotificationPayload = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.attempt, 1);
    }

    #[test]
    fn payload_without_subscription_id_key_defaults_to_none() {
        let json = serde_json::json!({
            "org_id": 1,
            "monitor_id": 2,
            "destination_id": 3,
            "new_state": "triggered",
            "subscriber_id": 4,
            "attempt": 1,
        });
        let parsed: SendNotificationPayload = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.subscription_id, None);
    }

    /// A random, practically-unique id for a test-run's `delivery_id` — a hardcoded literal would
    /// collide with a `pending`/`running` conveyor task row an earlier interrupted run of this
    /// test left behind in the shared test database (mirrors `query_exec.rs`'s `unique_query_id`).
    fn unique_delivery_id() -> i64 {
        (uuid::Uuid::new_v4().as_u128() % (i64::MAX as u128 / 2)) as i64
    }

    /// Real Postgres + Redis only (skips cleanly without `TEST_DATABASE_URL`/`TEST_REDIS_URL`):
    /// a second `send_notification` enqueue for the same delivery id, while the first is still
    /// pending, must downcast the conveyor's `AlreadyInflight` into a benign `Ok(())` — not
    /// propagate it as a hard error (the retry re-enqueue in `send_notification.rs` legitimately
    /// races the still-running attempt that scheduled it).
    #[tokio::test]
    async fn an_already_inflight_send_notification_enqueue_is_not_an_error() {
        let Some((db, valkey, redis_url)) = crate::test_support::test_db_and_valkey().await else {
            return;
        };

        let _guard = ENV_LOCK.lock().await;
        std::env::set_var("CONVEYOR_SEND_NOTIFICATION_ENABLED", "true");
        let conveyor = ConveyorBuilder::new(db, valkey, redis_url).build();

        let delivery_id = unique_delivery_id();
        let params = |attempt: u32| SendNotificationParams {
            org_id: 1,
            monitor_id: 2,
            destination_id: 3,
            new_state: "triggered".to_string(),
            subscriber_id: 4,
            subscription_id: None,
            attempt,
            delivery_id: Some(delivery_id),
            observed: None,
            explanation: None,
        };

        enqueue_send_notification(&NoopQueue, Some(&conveyor), params(1))
            .await
            .unwrap();

        let second = enqueue_send_notification(&NoopQueue, Some(&conveyor), params(2)).await;
        std::env::remove_var("CONVEYOR_SEND_NOTIFICATION_ENABLED");

        assert!(
            second.is_ok(),
            "an in-flight duplicate must be downcast into a benign no-op, not a hard error: {second:?}"
        );
    }

    /// Never actually reached in the conveyor branch (the legacy branch is only hit when
    /// `conveyor` is `None`) — required only to satisfy `enqueue_send_notification`'s signature.
    #[derive(Default)]
    struct NoopQueue;

    #[async_trait]
    impl Queue for NoopQueue {
        async fn enqueue(&self, _queue: QueueName, _job: JobMessage) -> anyhow::Result<()> {
            Ok(())
        }
    }

    // One test, not two: a process-global env var can't safely split across parallel tests.
    #[test]
    fn enabled_reflects_the_env_var() {
        let _guard = ENV_LOCK.blocking_lock();
        std::env::remove_var("CONVEYOR_SEND_NOTIFICATION_ENABLED");
        assert!(!conveyor_send_notification_enabled());
        for (raw, expected) in [
            ("false", false),
            ("0", false),
            ("true", true),
            ("1", true),
            ("TRUE", true),
            (" 1 ", true),
        ] {
            std::env::set_var("CONVEYOR_SEND_NOTIFICATION_ENABLED", raw);
            assert_eq!(conveyor_send_notification_enabled(), expected, "{raw:?}");
        }
        std::env::remove_var("CONVEYOR_SEND_NOTIFICATION_ENABLED");
    }
}
