//! `BLPOP` consumer loop, one task per subscribed [`QueueName`] (`WORKER_QUEUES`), with bounded
//! concurrency per tier and cooperative shutdown via `tokio::sync::watch`. Each loop opens its own
//! dedicated `MultiplexedConnection`: `BLPOP` blocks its connection, so sharing the multiplexed
//! `ConnectionManager` would stall other commands (e.g. job-status `SET`/`GET`) behind it.
//! `default`/`queries`/`schemas` share a `fast` semaphore; `scheduled_queries` (multi-hour queries)
//! gets its own `long` one, so a saturated long tier can't block fast loops.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis::{AsyncCommands, AsyncConnectionConfig, Client};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;

use arcodash_core::queue::{JobMessage, QueueName};

use crate::consumer::dispatch;
use crate::handlers::execute_query::ExecuteQueryContext;

/// A Redis-queue handler task ended in a panic rather than a plain return. Free-standing counter,
/// readable by path from anywhere.
pub static HANDLER_PANIC: AtomicU64 = AtomicU64::new(0);

/// A Redis-queue handler exceeded its wall-clock timeout and was abandoned mid-dispatch.
pub static HANDLER_TIMEOUT: AtomicU64 = AtomicU64::new(0);

/// Serializes tests that bump or assert on `HANDLER_PANIC`/`HANDLER_TIMEOUT`; they're
/// process-global, so a parallel test's increment would break a `before + 1` assertion.
#[cfg(test)]
pub(crate) static HANDLER_COUNTERS_TEST_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

/// How long each `BLPOP` blocks before retrying, bounding shutdown latency. The connection's
/// client-side `response_timeout` is disabled ([`connect_with_retry`]) so this can exceed the 500ms
/// default.
const POLL_TIMEOUT_SECS: f64 = 1.0;

/// Grace period fast-tier loops wait for in-flight jobs on shutdown. Fast jobs are sub-second, so
/// anything longer is a bug and gets killed and refired by the sweeper.
const FAST_DRAIN_SECS: u64 = 30;

/// Run one `BLPOP` loop per queue, each drawing permits from its tier's semaphore, until `shutdown`
/// fires. Returns once every per-queue task has drained (or hit its tier's grace budget).
/// `redis_url` opens one dedicated connection per queue; `ctx`'s `ConnectionManager` is never used
/// for `BLPOP`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    redis_url: String,
    ctx: Arc<ExecuteQueryContext>,
    queues: Vec<QueueName>,
    fast_concurrency: usize,
    long_concurrency: usize,
    long_drain_secs: u64,
    handler_timeout: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let fast_semaphore = Arc::new(Semaphore::new(fast_concurrency.max(1)));
    let long_semaphore = Arc::new(Semaphore::new(long_concurrency.max(1)));

    let tasks: Vec<_> = queues
        .into_iter()
        .map(|queue_name| {
            // Exhaustive on purpose: a new queue must pick its tier (see module docs).
            let (semaphore, drain_secs) = match queue_name {
                QueueName::ScheduledQueries => (&long_semaphore, long_drain_secs),
                QueueName::Queries | QueueName::Schemas | QueueName::Default => {
                    (&fast_semaphore, FAST_DRAIN_SECS)
                }
            };
            tokio::spawn(queue_loop(
                queue_name,
                redis_url.clone(),
                ctx.clone(),
                semaphore.clone(),
                drain_secs,
                handler_timeout,
                shutdown.clone(),
            ))
        })
        .collect();

    // Wait for shutdown, then let every per-queue task drain in-flight jobs up to its tier's grace
    // budget.
    let _ = shutdown.changed().await;
    for task in tasks {
        let _ = task.await;
    }
}

/// (Re)connect this queue's dedicated `BLPOP` connection, retrying with backoff.
async fn connect_with_retry(redis_url: &str, queue_name: QueueName) -> MultiplexedConnection {
    loop {
        let client = match Client::open(redis_url) {
            Ok(client) => client,
            Err(e) => {
                tracing::error!(queue = queue_name.as_str(), error = %e, "invalid VALKEY_URL, retrying");
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
                tracing::error!(queue = queue_name.as_str(), error = %e, "failed to open dedicated BLPOP connection, retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Logs and counts a handler task that panicked; otherwise a panic inside `dispatch` would vanish
/// without a log line.
fn log_if_panicked(queue_name: QueueName, joined: Result<(), tokio::task::JoinError>) {
    if let Err(e) = joined {
        tracing::error!(queue = queue_name.as_str(), error = %e, "job handler task ended abnormally");
        HANDLER_PANIC.fetch_add(1, Ordering::Relaxed);
    }
}

/// Drains finished handler tasks without blocking on running ones, so the set only holds truly
/// in-flight tasks.
fn reap(in_flight: &mut JoinSet<()>, queue_name: QueueName) {
    while let Some(joined) = in_flight.try_join_next() {
        log_if_panicked(queue_name, joined);
    }
}

async fn queue_loop(
    queue_name: QueueName,
    redis_url: String,
    ctx: Arc<ExecuteQueryContext>,
    semaphore: Arc<Semaphore>,
    drain_secs: u64,
    handler_timeout: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let key = queue_name.queue_key();
    tracing::info!(queue = queue_name.as_str(), %key, "starting queue consumer");

    let mut conn = connect_with_retry(&redis_url, queue_name).await;

    // Tracks unfinished handlers so shutdown can await them instead of dropping them mid-execution.
    let mut in_flight: JoinSet<()> = JoinSet::new();

    loop {
        if *shutdown.borrow() {
            break;
        }

        reap(&mut in_flight, queue_name);

        let popped: Result<Option<(String, String)>, redis::RedisError> =
            conn.blpop(&key, POLL_TIMEOUT_SECS).await;

        let (raw_key, raw_payload) = match popped {
            Ok(Some(pair)) => pair,
            Ok(None) => continue,
            Err(e) => {
                tracing::error!(queue = queue_name.as_str(), error = %e, "BLPOP failed, reconnecting after a short delay");
                tokio::time::sleep(Duration::from_secs(1)).await;
                conn = connect_with_retry(&redis_url, queue_name).await;
                continue;
            }
        };
        debug_assert_eq!(raw_key, key);

        let message: JobMessage = match serde_json::from_str(&raw_payload) {
            Ok(message) => message,
            Err(e) => {
                tracing::error!(queue = queue_name.as_str(), error = %e, payload = %raw_payload, "dropping malformed job message");
                continue;
            }
        };

        // Races the acquire against shutdown: on a saturated tier it could block past shutdown and
        // defeat the drain grace. The message is already popped, so abandoning it relies on the
        // normal lost-delivery recovery (e.g. `requeue_stale_pending`).
        let permit = tokio::select! {
            acquired = semaphore.clone().acquire_owned() => match acquired {
                Ok(permit) => permit,
                Err(_) => break, // semaphore closed: only happens if we're shutting down anyway.
            },
            _ = shutdown.changed() => {
                // BLPOP already removed this message; push it back on shutdown so a future consumer
                // picks it up.
                if let Err(e) = ctx.queue.enqueue(queue_name, message).await {
                    tracing::error!(queue = queue_name.as_str(), error = %e, "failed to push an in-flight-but-unclaimed job back on shutdown");
                }
                break;
            }
        };

        let task_ctx = ctx.clone();
        in_flight.spawn(async move {
            // `dispatch` is awaited inline so a timeout drops (and cancels) the future at its next
            // await; the run/job's lease-expiry + sweeper path recovers the rest.
            if tokio::time::timeout(handler_timeout, dispatch(&task_ctx, message))
                .await
                .is_err()
            {
                tracing::error!(
                    queue = queue_name.as_str(),
                    timeout_secs = handler_timeout.as_secs(),
                    "job handler exceeded the handler timeout, abandoning it"
                );
                HANDLER_TIMEOUT.fetch_add(1, Ordering::Relaxed);
            }
            drop(permit);
        });
    }

    let remaining = in_flight.len();
    if remaining > 0 {
        tracing::info!(
            queue = queue_name.as_str(),
            remaining,
            drain_secs,
            "queue consumer stopped, draining in-flight jobs"
        );
        let drained = tokio::time::timeout(Duration::from_secs(drain_secs), async {
            while let Some(joined) = in_flight.join_next().await {
                log_if_panicked(queue_name, joined);
            }
        })
        .await;
        if drained.is_err() {
            // Dropping `in_flight` here aborts what's left; lease-expiry + sweeper (or re-`BLPOP`
            // for non-run-backed jobs) recovers it.
            tracing::warn!(
                queue = queue_name.as_str(),
                remaining = in_flight.len(),
                "drain grace expired with jobs still in flight; abandoning them"
            );
        }
    }

    tracing::info!(queue = queue_name.as_str(), "queue consumer stopped");
}

#[cfg(test)]
mod tier_isolation_tests {
    //! Proves no head-of-line blocking between tiers: with one shared semaphore, long jobs would
    //! starve fast queues of permits. Needs real Postgres + Redis (skips without
    //! `TEST_DATABASE_URL`/`TEST_REDIS_URL`/`TEST_TARGET_DATABASE_URL`).
    use std::time::{Duration, Instant};

    use arcodash_core::queue::{self as core_queue, JobMessage, JobStatus, QueueName};
    use tokio::sync::watch;

    use super::run;
    use crate::scheduler::test_support::*;

    #[tokio::test]
    async fn saturated_long_tier_does_not_block_fast_tier_dispatch() {
        let Ok(database_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let redis_url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL required");
        let db = arcodash_core::db::connect(&database_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();

        let encryption_key = [7u8; 32];
        let org = seed_org(&db, &format!("org-tier-isolation-{}", uuid::Uuid::new_v4())).await;
        let Some(ds) = seed_target_pg_data_source(&db, org.id, &encryption_key).await else {
            eprintln!("skipping: TEST_TARGET_DATABASE_URL not set");
            return;
        };

        std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");

        let mut ctx = test_ctx(db.clone(), redis.clone());
        ctx.encryption_key = encryption_key;
        let ctx = std::sync::Arc::new(ctx);

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        // fast=1, long=1: two separate pools that never fall back on each other.
        let consumer = tokio::spawn(run(
            redis_url,
            ctx.clone(),
            vec![QueueName::Queries, QueueName::ScheduledQueries],
            1,
            1,
            5,
            Duration::from_secs(300),
            shutdown_rx,
        ));

        // Saturate the long tier two jobs deep (long_concurrency=1).
        for _ in 0..2 {
            let job_id = format!("long-{}", uuid::Uuid::new_v4());
            ctx.queue
                .enqueue(
                    QueueName::ScheduledQueries,
                    JobMessage::ExecuteQuery {
                        job_id,
                        org_id: org.id,
                        data_source_id: ds.id,
                        query_text: "SELECT pg_sleep(8)".to_string(),
                        parameters: serde_json::json!({}),
                        user_id: 1,
                        query_id: None,
                        scheduled: true,
                        run_id: None,
                        repoint_published: false,
                        max_rows: None,
                    },
                )
                .await
                .unwrap();
        }

        // Let the long tier claim its permit first, or the fast job could race ahead and prove
        // nothing.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let fast_job_id = format!("fast-{}", uuid::Uuid::new_v4());
        ctx.queue
            .enqueue(
                QueueName::Queries,
                JobMessage::ExecuteQuery {
                    job_id: fast_job_id.clone(),
                    org_id: org.id,
                    data_source_id: ds.id,
                    query_text: "SELECT 1".to_string(),
                    parameters: serde_json::json!({}),
                    user_id: 1,
                    query_id: None,
                    scheduled: false,
                    run_id: None,
                    repoint_published: false,
                    max_rows: None,
                },
            )
            .await
            .unwrap();

        // The fast job must be dispatched promptly (status leaves `None`); under a shared semaphore
        // it would wait for a long job to free a permit.
        let start = Instant::now();
        let pickup_deadline = Duration::from_secs(2);
        loop {
            if core_queue::get_job_status(&redis, &fast_job_id)
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
            assert!(
                start.elapsed() < pickup_deadline,
                "fast-tier job was never dispatched within {pickup_deadline:?} while the long \
                 tier was saturated — head-of-line blocking regression"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let pickup_elapsed = start.elapsed();

        let completion_deadline = Duration::from_secs(5);
        let status = loop {
            let status = core_queue::get_job_status(&redis, &fast_job_id)
                .await
                .unwrap()
                .expect("job status must still be present once seen");
            if status.status.is_terminal() {
                break status;
            }
            assert!(
                start.elapsed() < completion_deadline,
                "fast job never reached a terminal status within {completion_deadline:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(
            status.status,
            JobStatus::Success,
            "fast job (SELECT 1) should reach Success, not fail/cancel"
        );
        eprintln!(
            "fast job dispatched in {pickup_elapsed:?}, completed in {:?} while long tier saturated",
            start.elapsed()
        );

        let _ = shutdown_tx.send(true);
        // Long jobs are still mid pg_sleep(8); the drain grace (5s) caps shutdown, so this returns
        // well under 8s.
        let shutdown_start = Instant::now();
        tokio::time::timeout(Duration::from_secs(10), consumer)
            .await
            .expect("consumer must exit within its drain grace, not hang on the long job")
            .unwrap();
        assert!(
            shutdown_start.elapsed() < Duration::from_secs(8),
            "shutdown took {:?} — should be capped by the long tier's 5s drain grace, not the \
             full 8s pg_sleep",
            shutdown_start.elapsed()
        );
    }
}

#[cfg(test)]
mod handler_supervision_tests {
    //! Covers what a bare `try_join_next()`/`join_next()` would swallow: a handler panic and a
    //! wedged handler (permit held forever).
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;

    use arcodash_core::queue::QueueName;

    use super::{reap, HANDLER_COUNTERS_TEST_LOCK, HANDLER_PANIC, HANDLER_TIMEOUT};

    #[tokio::test]
    async fn a_panicking_handler_is_logged_and_counted() {
        let _guard = HANDLER_COUNTERS_TEST_LOCK.lock().await;
        let before = HANDLER_PANIC.load(Ordering::Relaxed);

        let mut in_flight: JoinSet<()> = JoinSet::new();
        in_flight.spawn(async { panic!("deliberate test panic") });
        // Let the task panic before reaping; `reap` only drains finished tasks.
        tokio::time::sleep(Duration::from_millis(100)).await;

        reap(&mut in_flight, QueueName::Default);

        assert_eq!(
            HANDLER_PANIC.load(Ordering::Relaxed),
            before + 1,
            "a panicking handler task must be counted, not silently swallowed"
        );
    }

    /// Exercises the timeout-then-drop-permit shape of `queue_loop`'s dispatch without the full
    /// stack: the permit is released when `tokio::time::timeout` drops its inner future.
    #[tokio::test]
    async fn a_hung_handler_hits_the_timeout_and_releases_its_permit() {
        let _guard = HANDLER_COUNTERS_TEST_LOCK.lock().await;
        let before = HANDLER_TIMEOUT.load(Ordering::Relaxed);

        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let handler_timeout = Duration::from_millis(50);
        let queue_name = QueueName::Default;

        let mut in_flight: JoinSet<()> = JoinSet::new();
        in_flight.spawn(async move {
            if tokio::time::timeout(handler_timeout, tokio::time::sleep(Duration::from_secs(60)))
                .await
                .is_err()
            {
                tracing::error!(
                    queue = queue_name.as_str(),
                    timeout_secs = handler_timeout.as_secs(),
                    "job handler exceeded the handler timeout, abandoning it"
                );
                HANDLER_TIMEOUT.fetch_add(1, Ordering::Relaxed);
            }
            drop(permit);
        });

        // A fresh acquire within 1s of a 60s sleep proves the permit came from the timeout path.
        let acquired =
            tokio::time::timeout(Duration::from_secs(1), semaphore.acquire_owned()).await;
        assert!(
            acquired.is_ok(),
            "permit was not released after the handler timeout fired"
        );
        assert_eq!(
            HANDLER_TIMEOUT.load(Ordering::Relaxed),
            before + 1,
            "a timed-out handler must be counted"
        );

        while in_flight.join_next().await.is_some() {}
    }
}
