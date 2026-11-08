//! `GET /health` (liveness probe) and `GET /metrics` (Prometheus gauges for queue/task wedges) on
//! one listener; the worker does no other HTTP, but container platforms often require a listener on
//! `$PORT`.

use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use serde_json::{json, Value};
use tokio::sync::watch;

/// Overall budget for building the `/metrics` response. A gauge still in flight past this is
/// dropped from the response rather than holding a scraper open on a sick database.
const METRICS_TIMEOUT: Duration = Duration::from_secs(2);
/// Rows older than this with no claim are a leak, not an honest queueing delay — chosen well
/// above any legitimate wait under normal tier concurrency.
const LEAKED_PENDING_WINDOW: &str = "10 minutes";
const DEAD_LETTER_QUEUE_KEY: &str = "arcodash:queue:dead";

/// Process-wide counters bumped where the event happens and only read here on scrape; reset on
/// restart, as Prometheus counters expect.
pub static SWEEPER_RECLAIMED_EXPIRED_LEASE: AtomicU64 = AtomicU64::new(0);
pub static SWEEPER_RECLAIMED_ABANDONED_PENDING: AtomicU64 = AtomicU64::new(0);
pub static SWEEPER_RECLAIMED_STALE_PENDING: AtomicU64 = AtomicU64::new(0);
pub static SWEEPER_RECLAIMED_STALE_PENDING_NOTIFICATIONS: AtomicU64 = AtomicU64::new(0);
pub static SWEEPER_RECLAIMED_STALE_PENDING_MAIL: AtomicU64 = AtomicU64::new(0);
pub static SWEEPER_RECLAIMED_STRANDED_MONITOR_RUN: AtomicU64 = AtomicU64::new(0);
pub static LEADER_ACQUIRE_FAILED: AtomicU64 = AtomicU64::new(0);
pub static RUN_TASK_DIVERGENCE: AtomicU64 = AtomicU64::new(0);

/// A sweep's own row-fetch query failing outright (per-row failures are logged instead), bumped
/// once per failed fetch by `scheduler/sweeper.rs`.
pub static SWEEP_FAILED: AtomicU64 = AtomicU64::new(0);
/// Duration of the last completed sweep cycle in ms (a gauge, not a total).
pub static SWEEP_CYCLE_DURATION_MS: AtomicU64 = AtomicU64::new(0);

pub static QUERY_RUNS_SUCCESS: AtomicU64 = AtomicU64::new(0);
pub static QUERY_RUNS_FAILED: AtomicU64 = AtomicU64::new(0);
pub static QUERY_RUNS_TIMED_OUT: AtomicU64 = AtomicU64::new(0);

pub static MONITOR_EVALS_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static MONITOR_TRANSITIONS_OK: AtomicU64 = AtomicU64::new(0);
pub static MONITOR_TRANSITIONS_TRIGGERED: AtomicU64 = AtomicU64::new(0);
pub static MONITOR_TRANSITIONS_UNEVALUABLE: AtomicU64 = AtomicU64::new(0);
pub static MONITOR_TRANSITIONS_UNKNOWN: AtomicU64 = AtomicU64::new(0);

pub static NOTIFICATIONS_SENT: AtomicU64 = AtomicU64::new(0);
pub static NOTIFICATIONS_FAILED: AtomicU64 = AtomicU64::new(0);

pub static ESCALATIONS_FIRED: AtomicU64 = AtomicU64::new(0);

/// Records a monitor state transition by its closed set of `new_state` values.
pub fn record_monitor_transition(new_state: &str) {
    let counter = match new_state {
        "ok" => &MONITOR_TRANSITIONS_OK,
        "triggered" => &MONITOR_TRANSITIONS_TRIGGERED,
        "unevaluable" => &MONITOR_TRANSITIONS_UNEVALUABLE,
        _ => &MONITOR_TRANSITIONS_UNKNOWN,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Records a terminal delivery outcome; `"retrying"`/`"skipped"` aren't terminal and aren't
/// counted.
pub fn record_notification_outcome(recorded_status: &str) {
    match recorded_status {
        "sent" => {
            NOTIFICATIONS_SENT.fetch_add(1, Ordering::Relaxed);
        }
        "failed" => {
            NOTIFICATIONS_FAILED.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

#[derive(Clone)]
struct MetricsState {
    db: DatabaseConnection,
    valkey: ConnectionManager,
}

/// Serves `/health` and `/metrics` on `addr` until `shutdown_rx` reports `true`.
pub async fn run(
    addr: SocketAddr,
    mut shutdown_rx: watch::Receiver<bool>,
    db: DatabaseConnection,
    valkey: ConnectionManager,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .with_state(MetricsState { db, valkey });
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.wait_for(|v| *v).await;
        })
        .await?;
    Ok(())
}

async fn metrics(State(state): State<MetricsState>) -> Response {
    let body = Arc::new(Mutex::new(String::new()));
    let collect = collect_metrics(state.db, state.valkey, body.clone());
    if tokio::time::timeout(METRICS_TIMEOUT, collect)
        .await
        .is_err()
    {
        tracing::warn!(
            "conveyor metrics: handler exceeded its time budget, returning whatever gauges resolved so far"
        );
    }
    let text = body
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], text).into_response()
}

/// Appends each gauge's text to `body` as it resolves, so a caller racing an overall deadline keeps
/// whatever finished. Gauges run concurrently and fail independently: one failing is logged and
/// omitted, never a 500.
async fn collect_metrics(
    db: DatabaseConnection,
    valkey: ConnectionManager,
    body: Arc<Mutex<String>>,
) {
    tokio::join!(
        append_status_counts(
            &body,
            &db,
            "runs",
            "arcodash_runs_by_status",
            "Rows in runs with status pending or running, by status.",
            "status IN ('pending', 'running')",
        ),
        append_leaked_pending(
            &body,
            &db,
            "runs",
            "arcodash_runs_leaked_pending",
            "Pending runs enqueued but still unclaimed after 10 minutes.",
        ),
        append_status_counts(
            &body,
            &db,
            "conveyor_tasks",
            "arcodash_conveyor_tasks_by_status",
            "Rows in conveyor_tasks with status pending or running, by status.",
            "status IN ('pending', 'running')",
        ),
        append_leaked_pending(
            &body,
            &db,
            "conveyor_tasks",
            "arcodash_conveyor_tasks_leaked_pending",
            "Pending conveyor tasks enqueued but still unclaimed after 10 minutes.",
        ),
        append_status_counts(
            &body,
            &db,
            "runs",
            "arcodash_runs_terminal_recent",
            "Runs that reached a terminal status in the last 10 minutes, by status.",
            "status NOT IN ('pending', 'running') AND finished_at > now() - interval '10 minutes'",
        ),
        append_status_counts(
            &body,
            &db,
            "conveyor_tasks",
            "arcodash_conveyor_tasks_terminal_recent",
            "Conveyor tasks that reached a terminal status in the last 10 minutes, by status.",
            "status NOT IN ('pending', 'running') AND finished_at > now() - interval '10 minutes'",
        ),
        append_status_counts(
            &body,
            &db,
            "notification_deliveries",
            "arcodash_notification_deliveries_by_status",
            "Rows in notification_deliveries with status pending, running, or retrying, by status.",
            "status IN ('pending', 'running', 'retrying')",
        ),
        append_status_counts(
            &body,
            &db,
            "mail_deliveries",
            "arcodash_mail_deliveries_by_status",
            "Rows in mail_deliveries with status pending, running, or retrying, by status.",
            "status IN ('pending', 'running', 'retrying')",
        ),
        append_status_counts(
            &body,
            &db,
            "notification_deliveries",
            "arcodash_notification_deliveries_terminal_recent",
            "Notification deliveries that reached a terminal status in the last 10 minutes, by status.",
            "status NOT IN ('pending', 'running', 'retrying') AND finished_at > now() - interval '10 minutes'",
        ),
        append_status_counts(
            &body,
            &db,
            "mail_deliveries",
            "arcodash_mail_deliveries_terminal_recent",
            "Mail deliveries that reached a terminal status in the last 10 minutes, by status.",
            "status NOT IN ('pending', 'running', 'retrying') AND finished_at > now() - interval '10 minutes'",
        ),
        append_oldest_pending_age(
            &body,
            &db,
            &["runs", "conveyor_tasks"],
            "arcodash_oldest_pending_age_seconds",
        ),
        append_queue_depths(&body, &valkey),
        append_dead_letter_depth(&body, &valkey),
    );
    append_process_counters(&body);
    arcodash_core::metrics::render(
        &mut body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
}

/// `kind` is `gauge` or `counter`.
fn metric_header(kind: &str, name: &str, help: &str) -> String {
    format!("# HELP {name} {help}\n# TYPE {name} {kind}\n")
}

/// Rows in `runs`/`conveyor_tasks` older than [`LEAKED_PENDING_WINDOW`] whose queue message is gone
/// but are still `pending`. Keys on `enqueued_at`, not `created_at`, so parked or re-enqueued rows
/// don't match. Shared by the gauge and the test that pins it.
fn leaked_pending_where() -> String {
    format!(
        "status = 'pending' AND enqueued_at IS NOT NULL \
         AND enqueued_at < now() - interval '{LEAKED_PENDING_WINDOW}'"
    )
}

/// `column` of the one row `sql` returns (an aggregate without `GROUP BY`).
async fn query_scalar<T: sea_orm::TryGetable>(
    db: &DatabaseConnection,
    sql: String,
    column: &str,
) -> Result<T, sea_orm::DbErr> {
    db.query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
        .await?
        .ok_or_else(|| sea_orm::DbErr::Custom(format!("{column} query returned no row")))?
        .try_get("", column)
}

async fn append_status_counts(
    body: &Mutex<String>,
    db: &DatabaseConnection,
    table: &str,
    metric_name: &str,
    help: &str,
    where_clause: &str,
) {
    // `table` and `where_clause` are always this file's own fixed literals, never external input.
    let sql =
        format!("SELECT status, count(*) AS c FROM {table} WHERE {where_clause} GROUP BY status");
    match db
        .query_all_raw(Statement::from_string(DbBackend::Postgres, sql))
        .await
    {
        Ok(rows) => {
            let mut text = metric_header("gauge", metric_name, help);
            for row in rows {
                let (Ok(status), Ok(c)) = (
                    row.try_get::<String>("", "status"),
                    row.try_get::<i64>("", "c"),
                ) else {
                    continue;
                };
                let _ = writeln!(text, "{metric_name}{{status=\"{status}\"}} {c}");
            }
            push(body, &text);
        }
        Err(e) => {
            tracing::warn!(error = %e, table, metric_name, "conveyor metrics: gauge query failed, omitting it");
        }
    }
}

async fn append_leaked_pending(
    body: &Mutex<String>,
    db: &DatabaseConnection,
    table: &str,
    metric_name: &str,
    help: &str,
) {
    let sql = format!(
        "SELECT count(*) AS c FROM {table} WHERE {}",
        leaked_pending_where()
    );
    match query_scalar::<i64>(db, sql, "c").await {
        Ok(c) => {
            let text = format!(
                "{}{metric_name} {c}\n",
                metric_header("gauge", metric_name, help)
            );
            push(body, &text);
        }
        Err(e) => {
            tracing::warn!(error = %e, table, metric_name, "conveyor metrics: gauge query failed, omitting it");
        }
    }
}

/// Age of the oldest still-unclaimed `pending` row (an operator signal for "is anything stuck");
/// `0` via `COALESCE` when none, so the series never gaps.
async fn append_oldest_pending_age(
    body: &Mutex<String>,
    db: &DatabaseConnection,
    tables: &[&str],
    metric_name: &str,
) {
    // One metric name across all `tables`, so the header is written once: samples are collected
    // first, then written in a single step.
    let mut samples = String::new();
    for &table in tables {
        // `::float8`: EXTRACT/COALESCE otherwise resolve to `numeric`, which sqlx cannot decode
        // directly into `f64`.
        let sql = format!(
            "SELECT COALESCE(EXTRACT(EPOCH FROM (now() - min(enqueued_at))), 0)::float8 AS age \
             FROM {table} WHERE status = 'pending' AND enqueued_at IS NOT NULL"
        );
        match query_scalar::<f64>(db, sql, "age").await {
            Ok(age) => {
                let _ = writeln!(samples, "{metric_name}{{table=\"{table}\"}} {age}");
            }
            Err(e) => {
                tracing::warn!(error = %e, table, metric_name, "conveyor metrics: gauge query failed, omitting it");
            }
        }
    }
    if !samples.is_empty() {
        let text = format!(
            "{}{samples}",
            metric_header(
                "gauge",
                metric_name,
                "Age in seconds of the oldest unclaimed pending row."
            )
        );
        push(body, &text);
    }
}

fn push(body: &Mutex<String>, text: &str) {
    body.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push_str(text);
}

/// One unlabeled sample under its own HELP/TYPE header.
fn single(text: &mut String, kind: &str, name: &str, help: &str, value: u64) {
    let _ = writeln!(text, "{}{name} {value}", metric_header(kind, name, help));
}

/// One sample per `(label value, count)` under a single HELP/TYPE header.
fn labeled_counter(
    text: &mut String,
    name: &str,
    help: &str,
    label: &str,
    samples: &[(&str, u64)],
) {
    text.push_str(&metric_header("counter", name, help));
    for (value, count) in samples {
        let _ = writeln!(text, "{name}{{{label}=\"{value}\"}} {count}");
    }
}

/// Renders the process-local counters (bumped where the event happens; never fails).
///
/// `arcodash_sweeper_reclaimed_total`'s path labels match the sweep functions that increment them
/// 1:1; the `conveyor_*` labels come from a distinct set of statics in `crates/conveyor`, kept
/// separate because they sweep different tables. `arcodash_leader_acquire_failed_total` sums the
/// worker's and conveyor scheduler's leader-lock failures into one leader-election health number.
fn append_process_counters(body: &Mutex<String>) {
    use crate::consumer::valkey_consumer::{HANDLER_PANIC, HANDLER_TIMEOUT};
    use arcodash_conveyor as conveyor;
    let n = |c: &AtomicU64| c.load(Ordering::Relaxed);
    let mut text = String::new();
    labeled_counter(
        &mut text,
        "arcodash_sweeper_reclaimed_total",
        "Rows reclaimed by each worker sweep function.",
        "path",
        &[
            ("sweep_expired_leases", n(&SWEEPER_RECLAIMED_EXPIRED_LEASE)),
            (
                "fail_abandoned_pending",
                n(&SWEEPER_RECLAIMED_ABANDONED_PENDING),
            ),
            ("requeue_stale_pending", n(&SWEEPER_RECLAIMED_STALE_PENDING)),
            (
                "requeue_stale_pending_notifications",
                n(&SWEEPER_RECLAIMED_STALE_PENDING_NOTIFICATIONS),
            ),
            (
                "requeue_stale_pending_mail",
                n(&SWEEPER_RECLAIMED_STALE_PENDING_MAIL),
            ),
            (
                "sweep_stranded_monitor_runs",
                n(&SWEEPER_RECLAIMED_STRANDED_MONITOR_RUN),
            ),
            (
                "conveyor_sweep_expired_leases",
                n(&conveyor::SWEEPER_RECLAIMED_EXPIRED_LEASE),
            ),
            (
                "conveyor_fail_abandoned_pending",
                n(&conveyor::SWEEPER_RECLAIMED_ABANDONED_PENDING),
            ),
            (
                "conveyor_requeue_stale_pending",
                n(&conveyor::SWEEPER_RECLAIMED_STALE_PENDING),
            ),
            (
                "conveyor_fan_out_orphaned_waiters",
                n(&conveyor::SWEEPER_RELEASED_ORPHANED_WAITER),
            ),
        ],
    );
    single(
        &mut text,
        "counter",
        "arcodash_leader_acquire_failed_total",
        "Failed leader-lock acquisitions across all schedulers.",
        n(&LEADER_ACQUIRE_FAILED) + n(&conveyor::LEADER_ACQUIRE_FAILED),
    );
    single(
        &mut text,
        "counter",
        "arcodash_heartbeat_lease_lost_total",
        "HeartbeatGuard::lost() firings across all leases.",
        n(&conveyor::HEARTBEAT_LEASE_LOST),
    );
    single(
        &mut text,
        "counter",
        "arcodash_conveyor_finalize_lost_race_total",
        "Finalizes that lost the terminal CAS to another writer and discarded their outcome.",
        n(&conveyor::FINALIZE_LOST_RACE),
    );
    single(
        &mut text,
        "counter",
        "arcodash_run_task_divergence_total",
        "Conveyor tasks that went terminal without their driving run row following.",
        n(&RUN_TASK_DIVERGENCE),
    );
    single(
        &mut text,
        "counter",
        "arcodash_sweep_failed_total",
        "Sweep row-fetch queries that failed outright.",
        n(&SWEEP_FAILED),
    );
    single(
        &mut text,
        "gauge",
        "arcodash_sweep_cycle_duration_ms",
        "Duration of the most recently completed sweep cycle.",
        n(&SWEEP_CYCLE_DURATION_MS),
    );
    labeled_counter(
        &mut text,
        "arcodash_query_runs_total",
        "execute_query outcomes, by how the run ended.",
        "outcome",
        &[
            ("success", n(&QUERY_RUNS_SUCCESS)),
            ("failed", n(&QUERY_RUNS_FAILED)),
            ("timed_out", n(&QUERY_RUNS_TIMED_OUT)),
        ],
    );
    single(
        &mut text,
        "counter",
        "arcodash_monitor_evaluations_total",
        "Monitor condition evaluations attempted.",
        n(&MONITOR_EVALS_TOTAL),
    );
    labeled_counter(
        &mut text,
        "arcodash_monitor_transitions_total",
        "Monitor state transitions, by the new state.",
        "new_state",
        &[
            ("ok", n(&MONITOR_TRANSITIONS_OK)),
            ("triggered", n(&MONITOR_TRANSITIONS_TRIGGERED)),
            ("unevaluable", n(&MONITOR_TRANSITIONS_UNEVALUABLE)),
            ("unknown", n(&MONITOR_TRANSITIONS_UNKNOWN)),
        ],
    );
    labeled_counter(
        &mut text,
        "arcodash_notifications_total",
        "Notification deliveries, by terminal outcome.",
        "outcome",
        &[
            ("sent", n(&NOTIFICATIONS_SENT)),
            ("failed", n(&NOTIFICATIONS_FAILED)),
        ],
    );
    single(
        &mut text,
        "counter",
        "arcodash_escalations_total",
        "Escalation steps advanced and notified.",
        n(&ESCALATIONS_FIRED),
    );
    single(
        &mut text,
        "counter",
        "arcodash_legacy_handler_panic_total",
        "Legacy-queue handler tasks that ended in a panic rather than a plain return.",
        n(&HANDLER_PANIC),
    );
    single(
        &mut text,
        "counter",
        "arcodash_legacy_handler_timeout_total",
        "Legacy-queue handler invocations abandoned after exceeding their wall-clock timeout.",
        n(&HANDLER_TIMEOUT),
    );
    single(
        &mut text,
        "counter",
        "arcodash_conveyor_schedule_auto_paused_total",
        "Conveyor schedules auto-paused after crossing the consecutive-failure threshold.",
        n(&conveyor::SCHEDULE_AUTO_PAUSED),
    );
    push(body, &text);
}

/// Every `arcodash:queue:*` list's depth except the dead-letter list, found by key pattern so it
/// covers both per-queue and conveyor per-task-type lists.
async fn append_queue_depths(body: &Mutex<String>, valkey: &ConnectionManager) {
    let mut conn = valkey.clone();
    let prefix = "arcodash:queue:";
    let keys = arcodash_core::redis_util::scan_keys(&mut conn, &format!("{prefix}*")).await;
    let mut keys = match keys {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(error = %e, "conveyor metrics: failed to list queue keys, omitting arcodash_queue_depth");
            return;
        }
    };
    // SCAN can repeat keys across pages; a duplicate series line would break the Prometheus scrape.
    keys.sort();
    keys.dedup();

    let mut text = metric_header(
        "gauge",
        "arcodash_queue_depth",
        "Number of envelopes waiting in each queue.",
    );
    let mut any = false;
    for key in keys {
        if key == DEAD_LETTER_QUEUE_KEY {
            continue;
        }
        match conn.llen::<_, i64>(&key).await {
            Ok(len) => {
                let queue = key.strip_prefix(prefix).unwrap_or(&key);
                let _ = writeln!(text, "arcodash_queue_depth{{queue=\"{queue}\"}} {len}");
                any = true;
            }
            Err(e) => {
                tracing::warn!(error = %e, key, "conveyor metrics: LLEN failed for a queue, omitting it");
            }
        }
    }
    if any {
        push(body, &text);
    }
}

async fn append_dead_letter_depth(body: &Mutex<String>, valkey: &ConnectionManager) {
    let mut conn = valkey.clone();
    match conn.llen::<_, i64>(DEAD_LETTER_QUEUE_KEY).await {
        Ok(len) => {
            let text = format!(
                "{}arcodash_dead_letter_depth {len}\n",
                metric_header(
                    "gauge",
                    "arcodash_dead_letter_depth",
                    "Envelopes stuck in the dead-letter queue."
                )
            );
            push(body, &text);
        }
        Err(e) => {
            tracing::warn!(error = %e, "conveyor metrics: LLEN failed for the dead-letter queue, omitting arcodash_dead_letter_depth");
        }
    }
}

#[cfg(test)]
mod metrics_tests {
    use super::*;
    use tokio::net::TcpListener;

    use crate::scheduler::test_support::{exec_sql, query_row, seed_org};

    async fn test_db() -> Option<DatabaseConnection> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        Some(arcodash_core::db::connect(&url).await.expect("connect"))
    }

    async fn test_valkey() -> ConnectionManager {
        let url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL required");
        arcodash_core::valkey_client::connect(&url)
            .await
            .expect("connect to redis")
    }

    /// Binds to port 0 and returns the bound address so concurrent runs never collide.
    async fn spawn_server(
        db: DatabaseConnection,
        valkey: ConnectionManager,
    ) -> (SocketAddr, watch::Sender<bool>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = Router::new()
            .route("/health", get(health))
            .route("/metrics", get(metrics))
            .with_state(MetricsState { db, valkey });
        tokio::spawn(async move {
            let mut rx = shutdown_rx;
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.wait_for(|v| *v).await;
                })
                .await
                .unwrap();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        (addr, shutdown_tx)
    }

    /// `uq_runs_inflight` is global per (entity_type, entity_id), so seeds need ids no other test
    /// uses.
    fn unique_entity_id() -> i64 {
        (uuid::Uuid::new_v4().as_u128() % 1_000_000_000_000) as i64 + 1_000_000_000
    }

    #[tokio::test]
    async fn metrics_endpoint_returns_the_gauge_series() {
        let Some(db) = test_db().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let valkey = test_valkey().await;
        let (addr, shutdown_tx) = spawn_server(db, valkey).await;

        let resp = reqwest::get(format!("http://{addr}/metrics"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/plain; version=0.0.4"
        );
        let text = resp.text().await.unwrap();
        for expected in [
            "arcodash_runs_leaked_pending",
            "arcodash_runs_terminal_recent",
            "arcodash_conveyor_tasks_terminal_recent",
            "arcodash_notification_deliveries_by_status",
            "arcodash_mail_deliveries_by_status",
            "arcodash_notification_deliveries_terminal_recent",
            "arcodash_mail_deliveries_terminal_recent",
            "arcodash_oldest_pending_age_seconds",
            "arcodash_sweeper_reclaimed_total",
            "arcodash_leader_acquire_failed_total",
            "arcodash_heartbeat_lease_lost_total",
            "arcodash_conveyor_finalize_lost_race_total",
            "arcodash_run_task_divergence_total",
            "arcodash_sweep_failed_total",
            "arcodash_sweep_cycle_duration_ms",
            "arcodash_query_runs_total",
            "arcodash_monitor_evaluations_total",
            "arcodash_monitor_transitions_total",
            "arcodash_notifications_total",
            "arcodash_escalations_total",
            "arcodash_legacy_handler_panic_total",
            "arcodash_legacy_handler_timeout_total",
            "arcodash_conveyor_schedule_auto_paused_total",
            "arcodash_config_fingerprint_mismatch",
        ] {
            assert!(
                text.contains(expected),
                "response missing {expected}:\n{text}"
            );
        }
        // Two tables share one header: a strict Prometheus parser rejects more than one HELP/TYPE
        // pair per metric.
        assert_eq!(
            text.matches("# TYPE arcodash_oldest_pending_age_seconds gauge")
                .count(),
            1,
            "metric must carry exactly one HELP/TYPE block across all its tables:\n{text}"
        );

        let _ = shutdown_tx.send(true);
    }

    #[tokio::test]
    async fn a_genuinely_leaked_pending_row_is_reflected_in_the_gauge() {
        let Some(db) = test_db().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let valkey = test_valkey().await;

        let org = seed_org(&db, &format!("metrics-leak-test-{}", uuid::Uuid::new_v4())).await;
        let eleven_min_ago = chrono::Utc::now() - chrono::Duration::minutes(11);
        let entity_id = unique_entity_id();
        exec_sql(
            &db,
            "INSERT INTO runs (org_id, entity_type, entity_id, triggered_by, status, created_at, enqueued_at) \
             VALUES ($1, 'query', $3, 'manual', 'pending', $2, $2)",
            [org.id.into(), eleven_min_ago.into(), entity_id.into()],
        )
        .await;

        let (addr, shutdown_tx) = spawn_server(db.clone(), valkey).await;
        let resp = reqwest::get(format!("http://{addr}/metrics"))
            .await
            .unwrap();
        let text = resp.text().await.unwrap();
        let _ = shutdown_tx.send(true);
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM organizations WHERE id = $1",
            [org.id.into()],
        ))
        .await
        .ok();

        let value = text
            .lines()
            .find(|l| l.starts_with("arcodash_runs_leaked_pending "))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<i64>().ok())
            .expect("arcodash_runs_leaked_pending line must be present");
        assert!(
            value >= 1,
            "expected the seeded 11-minute-old pending row to count as leaked, got {value}"
        );
    }

    /// The positive test can't tell `created_at` from `enqueued_at` and reads a shared gauge. This
    /// org-scoped test seeds the timestamps independently through the gauge's predicate: a
    /// `created_at` predicate would count 2, not 1.
    #[tokio::test]
    async fn leaked_pending_keys_on_enqueued_at_not_created_at() {
        let Some(db) = test_db().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let org = seed_org(
            &db,
            &format!("metrics-leak-keying-{}", uuid::Uuid::new_v4()),
        )
        .await;
        let eleven_min_ago = chrono::Utc::now() - chrono::Duration::minutes(11);
        let thirty_min_ago = chrono::Utc::now() - chrono::Duration::minutes(30);
        let five_secs_ago = chrono::Utc::now() - chrono::Duration::seconds(5);

        let (id_a, id_b) = (unique_entity_id(), unique_entity_id());
        // Row A: genuinely leaked — enqueued 11 minutes ago and still unclaimed.
        exec_sql(
            &db,
            "INSERT INTO runs (org_id, entity_type, entity_id, triggered_by, status, created_at, enqueued_at) \
             VALUES ($1, 'query', $3, 'manual', 'pending', $2, $2)",
            [org.id.into(), eleven_min_ago.into(), id_a.into()],
        )
        .await;
        // Row B: old row re-enqueued moments ago; not leaked, but a `created_at` predicate would
        // count it. Distinct `entity_id` since `uq_runs_inflight` allows one pending/running row
        // per entity.
        exec_sql(
            &db,
            "INSERT INTO runs (org_id, entity_type, entity_id, triggered_by, status, created_at, enqueued_at) \
             VALUES ($1, 'query', $4, 'manual', 'pending', $2, $3)",
            [org.id.into(), thirty_min_ago.into(), five_secs_ago.into(), id_b.into()],
        )
        .await;

        let sql = format!(
            "SELECT count(*) AS c FROM runs WHERE {} AND org_id = $1",
            leaked_pending_where()
        );
        let row = query_row(&db, &sql, [org.id.into()]).await;
        let count: i64 = row.try_get("", "c").unwrap();
        assert_eq!(
            count, 1,
            "only the row enqueued 11 minutes ago is leaked; the recently-re-enqueued old row must not count"
        );

        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM organizations WHERE id = $1",
            [org.id.into()],
        ))
        .await
        .ok();
    }

    #[tokio::test]
    async fn queue_depth_gauge_reports_each_seeded_queue_exactly_once() {
        let Some(db) = test_db().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let valkey = test_valkey().await;
        let mut conn = valkey.clone();

        let suffix = uuid::Uuid::new_v4();
        let queue_a = format!("arcodash:queue:a-{suffix}");
        let queue_b = format!("arcodash:queue:b-{suffix}");
        let dead_key = DEAD_LETTER_QUEUE_KEY.to_string();
        let _: i64 = conn.rpush(&queue_a, "x").await.unwrap();
        let _: i64 = conn.rpush(&queue_b, "x").await.unwrap();
        let _: i64 = conn.rpush(&queue_b, "y").await.unwrap();
        let _: i64 = conn.rpush(&dead_key, "z").await.unwrap();

        let (addr, shutdown_tx) = spawn_server(db, valkey).await;
        let resp = reqwest::get(format!("http://{addr}/metrics"))
            .await
            .unwrap();
        let text = resp.text().await.unwrap();
        let _ = shutdown_tx.send(true);

        let label_a = format!("a-{suffix}");
        let label_b = format!("b-{suffix}");
        let lines_for = |label: &str| -> Vec<&str> {
            text.lines()
                .filter(|l| {
                    l.starts_with("arcodash_queue_depth{queue=\"")
                        && l.contains(&format!("\"{label}\""))
                })
                .collect()
        };
        let a_lines = lines_for(&label_a);
        let b_lines = lines_for(&label_b);
        assert_eq!(
            a_lines.len(),
            1,
            "expected exactly one line for queue {label_a}:\n{text}"
        );
        assert_eq!(
            b_lines.len(),
            1,
            "expected exactly one line for queue {label_b}:\n{text}"
        );
        assert!(
            a_lines[0].ends_with(" 1"),
            "expected depth 1 for {label_a}, got {}",
            a_lines[0]
        );
        assert!(
            b_lines[0].ends_with(" 2"),
            "expected depth 2 for {label_b}, got {}",
            b_lines[0]
        );
        assert!(
            !text.contains("queue=\"dead\""),
            "dead-letter queue must never appear in arcodash_queue_depth:\n{text}"
        );

        let _: i64 = conn.del(&queue_a).await.unwrap();
        let _: i64 = conn.del(&queue_b).await.unwrap();
        // LREM, not DEL: the dead-letter key is shared across tests.
        let _: i64 = conn.lrem(&dead_key, 1, "z").await.unwrap();
    }

    #[tokio::test]
    async fn a_gauge_source_being_unavailable_still_returns_200() {
        let Some(db) = test_db().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        // A lazily built client on a dead port: every valkey gauge fails, and the handler must
        // still answer.
        let dead_client = redis::Client::open("redis://127.0.0.1:1").unwrap();
        let dead_valkey = dead_client
            .get_connection_manager_lazy(redis::aio::ConnectionManagerConfig::new())
            .expect("lazy connection manager construction never touches the network");

        let (addr, shutdown_tx) = spawn_server(db, dead_valkey).await;
        let resp = reqwest::get(format!("http://{addr}/metrics"))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "an unreachable gauge source must never turn into a non-200 response"
        );
        let _ = shutdown_tx.send(true);
    }

    fn render_process_counters() -> String {
        let body = Mutex::new(String::new());
        append_process_counters(&body);
        body.into_inner().unwrap_or_else(|p| p.into_inner())
    }

    /// Reads a line's trailing numeric value; callers use a monotonic `>=` check because other
    /// concurrent tests bump the same statics.
    fn metric_value(text: &str, line_prefix: &str) -> u64 {
        text.lines()
            .find(|l| l.starts_with(line_prefix))
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("no line found for {line_prefix:?}:\n{text}"))
    }

    /// The conveyor crate's sweeper counters must render under their own `path="conveyor_…"`
    /// labels, not be dropped or summed into the worker's.
    #[test]
    fn sweeper_reclaimed_total_carries_a_label_for_every_conveyor_sweep() {
        arcodash_conveyor::SWEEPER_RECLAIMED_EXPIRED_LEASE.fetch_add(1, Ordering::Relaxed);
        arcodash_conveyor::SWEEPER_RECLAIMED_ABANDONED_PENDING.fetch_add(1, Ordering::Relaxed);
        arcodash_conveyor::SWEEPER_RECLAIMED_STALE_PENDING.fetch_add(1, Ordering::Relaxed);
        arcodash_conveyor::SWEEPER_RELEASED_ORPHANED_WAITER.fetch_add(1, Ordering::Relaxed);

        let text = render_process_counters();

        for label in [
            "conveyor_sweep_expired_leases",
            "conveyor_fail_abandoned_pending",
            "conveyor_requeue_stale_pending",
            "conveyor_fan_out_orphaned_waiters",
        ] {
            assert!(
                metric_value(
                    &text,
                    &format!("arcodash_sweeper_reclaimed_total{{path=\"{label}\"}} ")
                ) >= 1,
                "missing/zero conveyor sweep label {label}:\n{text}"
            );
        }
    }

    /// A sweep's row-fetch failure is counted and the last cycle's duration renders as a gauge.
    #[test]
    fn metrics_render_includes_the_sweep_failure_and_cycle_duration_series() {
        SWEEP_FAILED.fetch_add(1, Ordering::Relaxed);
        SWEEP_CYCLE_DURATION_MS.store(1234, Ordering::Relaxed);

        let text = render_process_counters();

        assert!(
            metric_value(&text, "arcodash_sweep_failed_total ") >= 1,
            "missing sweep-failed counter:\n{text}"
        );
        assert_eq!(
            metric_value(&text, "arcodash_sweep_cycle_duration_ms "),
            1234,
            "sweep-cycle-duration gauge must reflect the last stored value exactly, not accumulate:\n{text}"
        );
    }

    /// The job/alerting outcome counter families render under the names/labels an operator would
    /// query.
    #[test]
    fn metrics_render_includes_job_and_alerting_outcome_counters() {
        QUERY_RUNS_SUCCESS.fetch_add(1, Ordering::Relaxed);
        QUERY_RUNS_FAILED.fetch_add(1, Ordering::Relaxed);
        QUERY_RUNS_TIMED_OUT.fetch_add(1, Ordering::Relaxed);
        MONITOR_EVALS_TOTAL.fetch_add(1, Ordering::Relaxed);
        record_monitor_transition("ok");
        record_monitor_transition("triggered");
        record_monitor_transition("unevaluable");
        record_monitor_transition("something-unrecognized");
        record_notification_outcome("sent");
        record_notification_outcome("failed");
        ESCALATIONS_FIRED.fetch_add(1, Ordering::Relaxed);

        let text = render_process_counters();

        for line in [
            "arcodash_query_runs_total{outcome=\"success\"} ",
            "arcodash_query_runs_total{outcome=\"failed\"} ",
            "arcodash_query_runs_total{outcome=\"timed_out\"} ",
            "arcodash_monitor_evaluations_total ",
            "arcodash_monitor_transitions_total{new_state=\"ok\"} ",
            "arcodash_monitor_transitions_total{new_state=\"triggered\"} ",
            "arcodash_monitor_transitions_total{new_state=\"unevaluable\"} ",
            "arcodash_monitor_transitions_total{new_state=\"unknown\"} ",
            "arcodash_notifications_total{outcome=\"sent\"} ",
            "arcodash_notifications_total{outcome=\"failed\"} ",
            "arcodash_escalations_total ",
        ] {
            assert!(
                metric_value(&text, line) >= 1,
                "expected at least 1 for {line:?}:\n{text}"
            );
        }
    }

    /// The consumer's panic/timeout counters and the conveyor auto-pause counter must render, not
    /// just increment.
    #[test]
    fn metrics_render_includes_legacy_handler_faults_and_schedule_auto_pause() {
        let _guard = crate::consumer::valkey_consumer::HANDLER_COUNTERS_TEST_LOCK.blocking_lock();
        crate::consumer::valkey_consumer::HANDLER_PANIC.fetch_add(1, Ordering::Relaxed);
        crate::consumer::valkey_consumer::HANDLER_TIMEOUT.fetch_add(1, Ordering::Relaxed);
        arcodash_conveyor::SCHEDULE_AUTO_PAUSED.fetch_add(1, Ordering::Relaxed);

        let text = render_process_counters();

        assert!(metric_value(&text, "arcodash_legacy_handler_panic_total ") >= 1);
        assert!(metric_value(&text, "arcodash_legacy_handler_timeout_total ") >= 1);
        assert!(metric_value(&text, "arcodash_conveyor_schedule_auto_paused_total ") >= 1);
    }
}
