//! Per-task-type consumer loops, one per `(tier, task_type)`, each drawing permits from its tier's
//! `Semaphore`. Each blocks on `BLPOP` over its own dedicated connection (a multiplexed one would
//! stall other commands). Cooperative shutdown via `watch::Receiver<bool>`: loops stop pulling work
//! and drain in-flight tasks up to a grace budget.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use redis::aio::{ConnectionManager, MultiplexedConnection};
use redis::{AsyncConnectionConfig, Client};
use sea_orm::DatabaseConnection;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;

use crate::conveyor::{TaskState, TierSpec};
use crate::envelope::Envelope;
use crate::handler::{TaskContext, TaskHandler, TaskOutcome, TaskResult};
use crate::ids::{TaskId, TaskType};
use crate::lifecycle;
use crate::queue;

/// How a spawned handler task left the dispatch select.
enum Dispatched {
    Finished(Result<TaskResult, tokio::task::JoinError>),
    TimedOut,
    LeaseLost,
}

/// Best-effort extraction of a panic's message, with a generic fallback for non-string payloads.
fn panic_message(err: tokio::task::JoinError) -> String {
    if !err.is_panic() {
        return "handler task was cancelled".to_string();
    }
    let payload = err.into_panic();
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "handler panicked".to_string())
}

/// How long each `BLPOP` blocks before retrying; bounds how long shutdown takes to notice. The
/// dedicated connection has its response timeout disabled ([`connect_with_retry`]) so this can
/// exceed the default.
const POLL_TIMEOUT_SECS: f64 = 1.0;
/// Default grace period a stopped loop waits for in-flight tasks before abandoning them to the
/// lease-expiry sweep (see [`crate::conveyor::TierSpec`] for per-tier values).
pub const DEFAULT_DRAIN_SECS: u64 = 30;

/// Run one consumer loop per `(tier, task_type)` until `shutdown` fires; returns once every loop
/// has drained (or hit the grace budget).
pub async fn run_tiers(
    db: DatabaseConnection,
    valkey: ConnectionManager,
    redis_url: String,
    registry: Arc<HashMap<TaskType, Arc<dyn TaskHandler>>>,
    tiers: Vec<TierSpec>,
    shutdown: watch::Receiver<bool>,
) {
    let mut tasks = Vec::new();
    for tier in tiers {
        let semaphore = Arc::new(Semaphore::new(tier.concurrency.max(1)));
        let drain_secs = tier.drain_secs;
        for task_type in tier.task_types {
            let db = db.clone();
            let valkey = valkey.clone();
            let redis_url = redis_url.clone();
            let registry = registry.clone();
            let semaphore = semaphore.clone();
            let mut shutdown = shutdown.clone();
            tasks.push(tokio::spawn(async move {
                queue_loop(
                    task_type,
                    db,
                    valkey,
                    &redis_url,
                    registry,
                    semaphore,
                    drain_secs,
                    &mut shutdown,
                )
                .await;
            }));
        }
    }

    let mut shutdown = shutdown;
    let _ = shutdown.changed().await;
    for task in tasks {
        let _ = task.await;
    }
}

/// Open a dedicated connection for a blocking `BLPOP` loop, retrying forever. `ConnectionManager`
/// can't be used: a blocking BLPOP would stall its other commands.
pub async fn connect_with_retry(redis_url: &str, queue: &str) -> MultiplexedConnection {
    loop {
        let client = match Client::open(redis_url) {
            Ok(client) => client,
            Err(e) => {
                tracing::error!(queue, error = %e, "invalid redis url for a BLPOP connection, retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        let config = AsyncConnectionConfig::new().set_response_timeout(None);
        match client
            .get_multiplexed_async_connection_with_config(&config)
            .await
        {
            Ok(conn) => return conn,
            Err(e) => {
                tracing::error!(queue, error = %e, "failed to open dedicated BLPOP connection, retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn queue_loop(
    task_type: TaskType,
    db: DatabaseConnection,
    valkey: ConnectionManager,
    redis_url: &str,
    registry: Arc<HashMap<TaskType, Arc<dyn TaskHandler>>>,
    semaphore: Arc<Semaphore>,
    drain_secs: u64,
    shutdown: &mut watch::Receiver<bool>,
) {
    let key = queue::queue_key(&task_type.0);
    let mut conn = connect_with_retry(redis_url, &task_type.0).await;
    let mut in_flight: JoinSet<()> = JoinSet::new();

    loop {
        if *shutdown.borrow() {
            break;
        }
        while in_flight.try_join_next().is_some() {}

        let envelope = match queue::blpop(&conn, &key, POLL_TIMEOUT_SECS).await {
            Ok(Some(envelope)) => envelope,
            Ok(None) => continue,
            Err(e) => {
                tracing::error!(task_type = %task_type, error = %e, "conveyor: BLPOP failed, reconnecting after a short delay");
                tokio::time::sleep(Duration::from_secs(1)).await;
                conn = connect_with_retry(redis_url, &task_type.0).await;
                continue;
            }
        };

        // Race the acquire against shutdown: on a saturated tier it could block past shutdown and
        // defeat the drain grace.
        let permit = tokio::select! {
            acquired = semaphore.clone().acquire_owned() => match acquired {
                Ok(permit) => permit,
                Err(_) => break,
            },
            _ = shutdown.changed() => {
                // BLPOP already removed this envelope; push it back before breaking so another
                // consumer picks it up instead of losing it.
                if let Ok(raw) = serde_json::to_string(&envelope) {
                    if let Err(e) = queue::push_raw(&valkey, &key, &raw).await {
                        tracing::error!(task_type = %task_type, task_id = envelope.task_id.0, error = %e, "conveyor: failed to push an in-flight-but-unclaimed envelope back on shutdown");
                    }
                }
                break;
            }
        };

        let db = db.clone();
        let valkey = valkey.clone();
        let registry = registry.clone();
        in_flight.spawn(async move {
            dispatch(&db, &valkey, &registry, envelope).await;
            drop(permit);
        });
    }

    let remaining = in_flight.len();
    if remaining > 0 {
        tracing::info!(task_type = %task_type, remaining, "conveyor: queue consumer stopped, draining in-flight tasks");
        let drained = tokio::time::timeout(Duration::from_secs(drain_secs), async {
            while in_flight.join_next().await.is_some() {}
        })
        .await
        .is_ok();

        if !drained {
            tracing::warn!(task_type = %task_type, remaining = in_flight.len(), "conveyor: drain grace expired with tasks still in flight; abandoning them");
        }
    }
}

/// Route one dequeued envelope: resolve its handler (dead-letter if unregistered), claim it (unless
/// resume), run it, and finalize/park on the outcome.
pub async fn dispatch(
    db: &DatabaseConnection,
    valkey: &ConnectionManager,
    registry: &HashMap<TaskType, Arc<dyn TaskHandler>>,
    envelope: Envelope,
) {
    let Some(handler) = registry.get(&envelope.task_type) else {
        dead_letter(db, valkey, &envelope).await;
        return;
    };

    if !envelope.resume {
        match lifecycle::start(db, envelope.task_id).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(task_id = envelope.task_id.0, task_type = %envelope.task_type, "conveyor: task not claimable, dropping this delivery");
                return;
            }
            Err(e) => {
                tracing::error!(task_id = envelope.task_id.0, error = %e, "conveyor: failed to claim task");
                return;
            }
        }
    }

    let (tenant_id, entity_id) = match fetch_task_scope(db, envelope.task_id).await {
        Ok(Some(scope)) => scope,
        Ok(None) => {
            tracing::error!(
                task_id = envelope.task_id.0,
                "conveyor: claimed task row vanished before dispatch"
            );
            return;
        }
        Err(e) => {
            tracing::error!(task_id = envelope.task_id.0, error = %e, "conveyor: failed to read the claimed task row");
            return;
        }
    };

    let heartbeat = lifecycle::spawn_heartbeat(db.clone(), envelope.task_id);

    let ctx = TaskContext {
        task_id: envelope.task_id,
        task_type: envelope.task_type.clone(),
        attempt: envelope.attempt,
        tenant_id,
        entity_id,
    };

    // Run the handler on its own task so a panic surfaces as a `JoinError` and a hang can be
    // stopped with `abort()`.
    let spawned_handler = handler.clone();
    let payload = envelope.payload.clone();
    let mut handle = tokio::spawn(async move { spawned_handler.run(ctx, payload).await });

    // Crate-wide backstop against a hung handler (the heartbeat keeps renewing its lease, so the
    // sweep can't reclaim it). The lease-lost arm covers the renewal loop losing the lease. Both
    // abort the handle, which only takes effect at the handler's next await point.
    let timeout_secs = crate::config::task_timeout_secs();
    let dispatched = tokio::select! {
        joined = &mut handle => Dispatched::Finished(joined),
        () = tokio::time::sleep(Duration::from_secs(timeout_secs)) => {
            handle.abort();
            Dispatched::TimedOut
        }
        () = heartbeat.lease_lost() => {
            handle.abort();
            Dispatched::LeaseLost
        }
    };

    // Covers a lease loss that won the select and one flagged just before the handler finished:
    // either way the row may be owned elsewhere, so don't persist the result.
    if matches!(dispatched, Dispatched::LeaseLost) || heartbeat.lost() {
        tracing::warn!(
            task_id = envelope.task_id.0,
            task_type = %envelope.task_type,
            "conveyor: lease lost while the handler was running; abandoning its result — the row has been reclaimed elsewhere"
        );
        drop(heartbeat);
        return;
    }

    // `heartbeat` is a `HeartbeatGuard`: dropping it here stops the renewal task.
    drop(heartbeat);

    let finalize = |outcome| lifecycle::finalize(db, valkey, envelope.task_id, outcome);
    let outcome = match dispatched {
        Dispatched::Finished(Ok(Ok(outcome))) => outcome,
        Dispatched::Finished(Ok(Err(e))) => TaskOutcome::Failed {
            error: format!("{e:#}"),
        },
        Dispatched::Finished(Err(join_err)) => {
            let msg = panic_message(join_err);
            tracing::error!(
                task_id = envelope.task_id.0,
                task_type = %envelope.task_type,
                error = %msg,
                "conveyor: handler task ended abnormally"
            );
            TaskOutcome::Failed {
                error: format!("handler panicked: {msg}"),
            }
        }
        Dispatched::TimedOut => {
            tracing::error!(
                task_id = envelope.task_id.0,
                timeout_secs,
                "conveyor: handler exceeded the task timeout, finalizing as timed_out"
            );
            let finalize_result = finalize(lifecycle::Outcome::new(
                TaskState::TimedOut,
                Some(format!("handler exceeded the {timeout_secs}s task timeout")),
            ))
            .await;
            if let Err(e) = finalize_result {
                tracing::error!(task_id = envelope.task_id.0, error = %e, "conveyor: failed to finalize a timed-out task");
            }
            return;
        }
        Dispatched::LeaseLost => unreachable!("handled above, before dispatched is matched here"),
    };

    let finalize_result = match outcome {
        TaskOutcome::Success { result_ref } => {
            finalize(lifecycle::Outcome {
                status: TaskState::Success,
                error: None,
                result_ref,
            })
            .await
        }
        TaskOutcome::Failed { error } => {
            finalize(lifecycle::Outcome::new(TaskState::Failed, Some(error))).await
        }
        TaskOutcome::Cancelled => {
            finalize(lifecycle::Outcome::new(TaskState::Cancelled, None)).await
        }
        TaskOutcome::WaitOn { task_id } => lifecycle::park(db, envelope.task_id, task_id).await,
    };
    if let Err(e) = finalize_result {
        tracing::error!(task_id = envelope.task_id.0, error = %e, "conveyor: failed to finalize/park task after handler run");
    }
}

/// Park an unroutable envelope on the dead-letter queue and finalize its row, so it surfaces once
/// instead of being retried forever. Every path that fails to record it says so.
async fn dead_letter(db: &DatabaseConnection, valkey: &ConnectionManager, envelope: &Envelope) {
    tracing::warn!(task_type = %envelope.task_type, task_id = envelope.task_id.0, "conveyor: no handler registered for task type, dead-lettering");
    let reason = match serde_json::to_string(envelope) {
        Ok(raw) => match queue::push_raw(valkey, queue::DEAD_LETTER_QUEUE, &raw).await {
            Ok(()) => "no handler registered for this task type",
            Err(e) => {
                // The row is left `pending`: the envelope that proved it unroutable is gone, so the
                // sweeper's requeue is the only path left.
                tracing::error!(task_id = envelope.task_id.0, error = %e, "conveyor: failed to push an unroutable envelope to the dead-letter queue — it is now lost");
                return;
            }
        },
        Err(e) => {
            tracing::error!(task_id = envelope.task_id.0, error = %e, "conveyor: failed to serialize an unroutable envelope for dead-lettering — it is now lost");
            "failed to serialize envelope for dead-lettering"
        }
    };
    // Terminalize the `pending` row rather than letting it be re-enqueued every sweep.
    let outcome = lifecycle::Outcome::new(TaskState::Failed, Some(reason.to_string()));
    if let Err(e) = lifecycle::finalize(db, valkey, envelope.task_id, outcome).await {
        tracing::error!(task_id = envelope.task_id.0, error = %e, reason, "conveyor: failed to finalize an unroutable task");
    }
}

async fn fetch_task_scope(
    db: &DatabaseConnection,
    task_id: TaskId,
) -> anyhow::Result<Option<(Option<i64>, Option<i64>)>> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    let Some(row) = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT tenant_id, entity_id FROM conveyor_tasks WHERE id = $1",
            [task_id.0.into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    let tenant_id: Option<i64> = row.try_get("", "tenant_id")?;
    let entity_id: Option<i64> = row.try_get("", "entity_id")?;
    Ok(Some((tenant_id, entity_id)))
}

#[cfg(test)]
mod panic_safety_tests {
    //! A handler panic must not leak the heartbeat task or escape `dispatch`: the handler runs on
    //! its own task, and `heartbeat` (a `HeartbeatGuard`) is dropped once the select resolves.
    use super::*;
    use crate::handler::TaskResult;
    use crate::ids::TaskId;
    use crate::test_support::*;
    use async_trait::async_trait;
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    use std::collections::HashMap;

    struct PanickingHandler;

    #[async_trait]
    impl TaskHandler for PanickingHandler {
        async fn run(&self, _ctx: TaskContext, _payload: serde_json::Value) -> TaskResult {
            panic!("simulated handler panic");
        }
    }

    async fn lease_expires_at(
        db: &DatabaseConnection,
        task_id: i64,
    ) -> Option<sea_orm::prelude::DateTimeWithTimeZone> {
        db.query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT lease_expires_at FROM conveyor_tasks WHERE id = $1",
            [task_id.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "lease_expires_at")
        .unwrap()
    }

    #[tokio::test]
    async fn handler_panic_does_not_leak_the_heartbeat() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        // Shrinks the heartbeat period so the test doesn't wait 30s. Process-global env var: not
        // parallel-safe.
        std::env::set_var("CONVEYOR_LEASE_SECS", "3");

        let task_type = format!("panic-toy_{}", uuid::Uuid::new_v4());
        let entity_id = unique_entity_id();
        let task_id = insert_task(&db, &task_type, Some(entity_id), None, None, "pending").await;
        let envelope = Envelope {
            task_id: TaskId(task_id),
            task_type: task_type.as_str().into(),
            payload: serde_json::json!({}),
            attempt: 1,
            enqueued_at: chrono::Utc::now(),
            resume: false,
        };

        let mut registry: HashMap<TaskType, std::sync::Arc<dyn TaskHandler>> = HashMap::new();
        registry.insert(
            task_type.as_str().into(),
            std::sync::Arc::new(PanickingHandler),
        );
        let registry = std::sync::Arc::new(registry);

        // Called directly: a panicking handler must not escape `dispatch`.
        dispatch(&db, &valkey, &registry, envelope).await;

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT status, error FROM conveyor_tasks WHERE id = $1",
                [task_id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let status: String = row.try_get("", "status").unwrap();
        let error: Option<String> = row.try_get("", "error").unwrap();
        assert_eq!(
            status, "failed",
            "a panicking handler must be finalized as failed, not leave the row stuck running"
        );
        assert!(
            error
                .as_deref()
                .unwrap_or_default()
                .contains("simulated handler panic"),
            "the panic message must be recorded as the error: {error:?}"
        );

        // uq_conveyor_tasks_inflight blocks a second pending/running row for the same (task_type,
        // entity_id); this insert succeeding proves the slot was released.
        let insert = db
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "INSERT INTO conveyor_tasks (task_type, entity_id, triggered_by, status, attempt, created_at) \
                 VALUES ($1, $2, 'enqueue', 'pending', 1, now())",
                [task_type.into(), entity_id.into()],
            ))
            .await;
        assert!(
            insert.is_ok(),
            "in-flight slot should be free after the panic is finalized"
        );

        tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
        let lease_after_panic = lease_expires_at(&db, task_id).await;
        tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
        let lease_later = lease_expires_at(&db, task_id).await;
        std::env::remove_var("CONVEYOR_LEASE_SECS");

        assert_eq!(
            lease_after_panic, lease_later,
            "lease_expires_at must not keep advancing after the handler panicked — a still-\
             advancing lease means the heartbeat task leaked"
        );
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::test_support::*;
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    use std::collections::HashMap;

    async fn task_status(db: &DatabaseConnection, task_id: i64) -> String {
        db.query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT status FROM conveyor_tasks WHERE id = $1",
            [task_id.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "status")
        .unwrap()
    }

    #[tokio::test]
    async fn an_unroutable_envelope_terminalizes_its_task_row() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let task_type = format!("unroutable_{}", uuid::Uuid::new_v4());
        let entity_id = unique_entity_id();
        let task_id = insert_task(&db, &task_type, Some(entity_id), None, None, "pending").await;

        let envelope = Envelope {
            task_id: crate::ids::TaskId(task_id),
            task_type: task_type.clone().into(),
            payload: serde_json::json!({}),
            attempt: 1,
            enqueued_at: chrono::Utc::now(),
            resume: false,
        };
        let registry: HashMap<TaskType, Arc<dyn TaskHandler>> = HashMap::new();

        dispatch(&db, &valkey, &registry, envelope).await;

        assert_eq!(
            task_status(&db, task_id).await,
            "failed",
            "an unroutable task must be terminalized, not left pending for the sweeper to retry"
        );

        // uq_conveyor_tasks_inflight blocks a second pending/running row for the same (task_type,
        // entity_id); this insert succeeding proves the slot was released.
        insert_task(&db, &task_type, Some(entity_id), None, None, "pending").await;
    }

    #[tokio::test]
    async fn a_missing_task_row_and_a_failed_read_are_distinguishable() {
        let Some((db, _valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let task_id = insert_task(
            &db,
            "vanished-toy",
            Some(unique_entity_id()),
            None,
            None,
            "pending",
        )
        .await;
        assert!(lifecycle::start(&db, crate::ids::TaskId(task_id))
            .await
            .unwrap());
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM conveyor_tasks WHERE id = $1",
            [task_id.into()],
        ))
        .await
        .unwrap();
        assert!(
            fetch_task_scope(&db, crate::ids::TaskId(task_id))
                .await
                .unwrap()
                .is_none(),
            "a genuinely deleted row must read back as Ok(None), not an error"
        );

        let database_url = std::env::var("TEST_DATABASE_URL").unwrap();
        let closed_db = sea_orm::Database::connect(&database_url).await.unwrap();
        closed_db.close_by_ref().await.unwrap();
        assert!(
            fetch_task_scope(&closed_db, crate::ids::TaskId(task_id))
                .await
                .is_err(),
            "a closed connection must surface as Err, not collapse into Ok(None) like a missing row"
        );
    }
}

#[cfg(test)]
mod timeout_tests {
    //! A handler hanging past `CONVEYOR_TASK_TIMEOUT_SECS` must be finalized `timed_out` by the
    //! `tokio::time::timeout` backstop in `dispatch`, not left running behind a renewing heartbeat.
    use super::*;
    use crate::handler::TaskResult;
    use crate::ids::TaskId;
    use crate::test_support::*;
    use async_trait::async_trait;
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    use std::collections::HashMap;

    struct SleepForeverHandler;

    #[async_trait]
    impl TaskHandler for SleepForeverHandler {
        async fn run(&self, _ctx: TaskContext, _payload: serde_json::Value) -> TaskResult {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            unreachable!("must be cancelled by the timeout backstop first");
        }
    }

    #[tokio::test]
    async fn a_hung_handler_is_finalized_as_timed_out_and_frees_its_slot() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        // Process-global env var: not parallel-safe.
        std::env::set_var("CONVEYOR_TASK_TIMEOUT_SECS", "1");

        let entity_id = unique_entity_id();
        let task_id = insert_task(&db, "hang-toy", Some(entity_id), None, None, "pending").await;
        let envelope = Envelope {
            task_id: TaskId(task_id),
            task_type: "hang-toy".into(),
            payload: serde_json::json!({}),
            attempt: 1,
            enqueued_at: chrono::Utc::now(),
            resume: false,
        };

        let mut registry: HashMap<TaskType, std::sync::Arc<dyn TaskHandler>> = HashMap::new();
        registry.insert("hang-toy".into(), std::sync::Arc::new(SleepForeverHandler));
        let registry = std::sync::Arc::new(registry);

        dispatch(&db, &valkey, &registry, envelope).await;
        std::env::remove_var("CONVEYOR_TASK_TIMEOUT_SECS");

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT status, error FROM conveyor_tasks WHERE id = $1",
                [task_id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let status: String = row.try_get("", "status").unwrap();
        let error: Option<String> = row.try_get("", "error").unwrap();
        assert_eq!(status, "timed_out");
        assert!(error.map(|e| !e.is_empty()).unwrap_or(false));

        let insert = db
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "INSERT INTO conveyor_tasks (task_type, entity_id, triggered_by, status, attempt, created_at) \
                 VALUES ('hang-toy', $1, 'enqueue', 'pending', 1, now())",
                [entity_id.into()],
            ))
            .await;
        assert!(
            insert.is_ok(),
            "in-flight slot should be free after the timeout backstop finalizes the task"
        );

        // A dropped `JoinHandle` detaches, so the timeout arm must call `handle.abort()`
        // explicitly.
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        let lease_after_wait = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT lease_expires_at FROM conveyor_tasks WHERE id = $1",
                [task_id.into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<Option<sea_orm::prelude::DateTimeWithTimeZone>>("", "lease_expires_at")
            .unwrap();
        assert!(
            lease_after_wait.is_none(),
            "lease_expires_at must stay cleared after the timeout finalize — any change means \
             the aborted handler kept running detached instead of actually stopping"
        );
    }
}

#[cfg(test)]
mod lease_lost_tests {
    //! A heartbeat renewal loss mid-handler (row reclaimed elsewhere) must abort the handler at its
    //! next await point and discard its result, not hang or clobber the other writer.
    use super::*;
    use crate::handler::TaskResult;
    use crate::ids::TaskId;
    use crate::test_support::*;
    use async_trait::async_trait;
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Signals `started`, then awaits a `Notify` nobody fires; `reached_after_await` flips only if
    /// the abort failed to land.
    struct StallingHandler {
        started: Arc<AtomicBool>,
        never_notified: Arc<tokio::sync::Notify>,
        reached_after_await: Arc<AtomicBool>,
    }

    #[async_trait]
    impl TaskHandler for StallingHandler {
        async fn run(&self, _ctx: TaskContext, _payload: serde_json::Value) -> TaskResult {
            self.started.store(true, Ordering::SeqCst);
            self.never_notified.notified().await;
            self.reached_after_await.store(true, Ordering::SeqCst);
            Ok(TaskOutcome::Success { result_ref: None })
        }
    }

    #[tokio::test]
    async fn a_handler_whose_lease_is_lost_is_aborted_and_its_result_discarded() {
        let Some((db, valkey, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        // Process-global env var: not parallel-safe.
        std::env::set_var("CONVEYOR_LEASE_SECS", "3");

        let task_type = format!("lease-lost-toy_{}", uuid::Uuid::new_v4());
        let entity_id = unique_entity_id();
        let task_id = insert_task(&db, &task_type, Some(entity_id), None, None, "pending").await;
        // Claimed directly so the envelope can carry `resume: true` and skip dispatch's claim,
        // matching a resumed delivery.
        assert!(lifecycle::start(&db, TaskId(task_id)).await.unwrap());

        let started = Arc::new(AtomicBool::new(false));
        let never_notified = Arc::new(tokio::sync::Notify::new());
        let reached_after_await = Arc::new(AtomicBool::new(false));

        let mut registry: HashMap<TaskType, std::sync::Arc<dyn TaskHandler>> = HashMap::new();
        registry.insert(
            task_type.as_str().into(),
            std::sync::Arc::new(StallingHandler {
                started: started.clone(),
                never_notified: never_notified.clone(),
                reached_after_await: reached_after_await.clone(),
            }),
        );
        let registry = std::sync::Arc::new(registry);

        let envelope = Envelope {
            task_id: TaskId(task_id),
            task_type: task_type.as_str().into(),
            payload: serde_json::json!({}),
            attempt: 1,
            enqueued_at: chrono::Utc::now(),
            resume: true,
        };

        let db_dispatch = db.clone();
        let valkey_dispatch = valkey.clone();
        let dispatch_handle = tokio::spawn(async move {
            dispatch(&db_dispatch, &valkey_dispatch, &registry, envelope).await;
        });

        let start_wait = std::time::Instant::now();
        while !started.load(Ordering::SeqCst) {
            assert!(
                start_wait.elapsed() < Duration::from_secs(5),
                "handler never started"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Simulate an external reclaim: another writer terminalized the row, so the next heartbeat
        // renewal finds nothing to update.
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE conveyor_tasks SET status = 'failed', finished_at = now(), lease_expires_at = NULL \
             WHERE id = $1",
            [task_id.into()],
        ))
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(10), dispatch_handle)
            .await
            .expect(
                "dispatch must return promptly once the lease is lost, not hang until the task timeout",
            )
            .unwrap();

        std::env::remove_var("CONVEYOR_LEASE_SECS");

        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT status, error FROM conveyor_tasks WHERE id = $1",
                [task_id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let status: String = row.try_get("", "status").unwrap();
        let error: Option<String> = row.try_get("", "error").unwrap();
        assert_eq!(
            status, "failed",
            "the externally-written terminal status must survive — dispatch must not finalize over it"
        );
        assert!(
            error.is_none(),
            "dispatch must not have written its own error over the external write: {error:?}"
        );
        assert!(
            !reached_after_await.load(Ordering::SeqCst),
            "the handler must have been aborted at its await point, not allowed to run past it"
        );
    }
}
