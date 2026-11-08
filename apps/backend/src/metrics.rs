//! `GET /metrics` — Prometheus text exposition for this process. This listener serves the public
//! API, so it is gated by `METRICS_TOKEN`; unset means the route doesn't exist (404, not 401).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sea_orm::{DbBackend, FromQueryResult, Statement};

use crate::services::job_service;
use crate::state::AppState;

/// A labeled counter family: label tuple → count.
type CounterMap<K> = OnceLock<Mutex<HashMap<K, u64>>>;

/// Requests rejected by [`crate::middleware::rate_limit::limit_by_ip`].
static RATE_LIMIT_REJECTIONS_IP: AtomicU64 = AtomicU64::new(0);
/// Requests rejected by [`crate::middleware::rate_limit::limit_by_user`].
static RATE_LIMIT_REJECTIONS_USER: AtomicU64 = AtomicU64::new(0);

/// `(method, route, status)` for every HTTP response. `route` is the matched path template
/// (`"<unmatched>"` if none), never the raw path, to bound cardinality; `status` is the class
/// (`"2xx"`, ...).
static HTTP_REQUESTS: CounterMap<(&'static str, String, &'static str)> = OnceLock::new();

/// `(method, outcome)` for every authentication attempt across all entry points
/// (password/LDAP/SAML login, session cookie, API key, remote user).
static AUTH_OUTCOMES: CounterMap<(&'static str, &'static str)> = OnceLock::new();

/// `schedule_service::update` clearing a failure auto-pause; the pause side is set and metered by
/// the worker.
static SCHEDULE_AUTO_PAUSE_CLEARED: AtomicU64 = AtomicU64::new(0);

static QUERY_EXECUTIONS: CounterMap<(&'static str, &'static str)> = OnceLock::new();
static CACHE_LOOKUP_HITS: AtomicU64 = AtomicU64::new(0);
static CACHE_LOOKUP_MISSES: AtomicU64 = AtomicU64::new(0);

/// `(capability, outcome)` for every `authz::authorize::authorize` decision.
static AUTHZ_DECISIONS: CounterMap<(&'static str, &'static str)> = OnceLock::new();

/// `(provider, call, outcome)` for each AI provider HTTP call. Owned `String` keys because `call`
/// comes from a runtime parameter.
static AI_PROVIDER_CALLS: CounterMap<(String, String, String)> = OnceLock::new();

/// `embed_service::create_embed`/`revoke_embed` taking effect: `"created"` | `"revoked"`.
static EMBED_EVENTS: CounterMap<&'static str> = OnceLock::new();
/// Public embed resolution (`/api/embed/{token}` and the deprecated public route): `"served"` |
/// `"not_found"` | `"error"` | `"unauthenticated"`; one shared counter for both routes.
static EMBED_SERVES: CounterMap<&'static str> = OnceLock::new();
/// `monitor_service::notify_subscription` ("Send now"): `"sent"` | `"denied"` | `"error"`.
static MONITOR_NOTIFICATIONS: CounterMap<&'static str> = OnceLock::new();

/// Poison-tolerant: a panic mid-increment must not take `/metrics` down with it.
fn lock<K>(map: &CounterMap<K>) -> MutexGuard<'_, HashMap<K, u64>> {
    map.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn bump<K: Eq + Hash>(map: &CounterMap<K>, key: K) {
    *lock(map).entry(key).or_insert(0) += 1;
}

pub fn incr_ip_rejection() {
    RATE_LIMIT_REJECTIONS_IP.fetch_add(1, Ordering::Relaxed);
}

pub fn incr_user_rejection() {
    RATE_LIMIT_REJECTIONS_USER.fetch_add(1, Ordering::Relaxed);
}

/// Collapse a status code to its class (`"2xx"`, `"4xx"`, ...).
#[must_use]
pub const fn status_class(status: StatusCode) -> &'static str {
    match status.as_u16() / 100 {
        1 => "1xx",
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        5 => "5xx",
        _ => "unknown",
    }
}

pub fn record_http_request(method: &'static str, route: String, status_class: &'static str) {
    bump(&HTTP_REQUESTS, (method, route, status_class));
}

pub fn record_auth_outcome(method: &'static str, outcome: &'static str) {
    bump(&AUTH_OUTCOMES, (method, outcome));
}

pub fn incr_schedule_auto_pause_cleared() {
    SCHEDULE_AUTO_PAUSE_CLEARED.fetch_add(1, Ordering::Relaxed);
}

/// One query execution resolving. `path`: `"interactive"` | `"run"`. `outcome`: `"cached"` |
/// `"enqueued"` | `"parked"`.
pub fn incr_query_execution(path: &'static str, outcome: &'static str) {
    bump(&QUERY_EXECUTIONS, (path, outcome));
}

/// One `find_fresh_cached_result` lookup, excluding the `max_age == Some(0)` forced refresh.
pub fn incr_query_result_cache_lookup(hit: bool) {
    let counter = if hit {
        &CACHE_LOOKUP_HITS
    } else {
        &CACHE_LOOKUP_MISSES
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// One `authz::authorize::authorize` decision; `capability` is the action's capability key.
pub fn record_authz_decision(capability: &'static str, allowed: bool) {
    bump(
        &AUTHZ_DECISIONS,
        (capability, if allowed { "allowed" } else { "denied" }),
    );
}

pub fn record_ai_provider_call(provider: &str, call: &str, outcome: &str) {
    bump(
        &AI_PROVIDER_CALLS,
        (provider.to_string(), call.to_string(), outcome.to_string()),
    );
}

/// `event`: `"created"` | `"revoked"`.
pub fn record_embed_event(event: &'static str) {
    bump(&EMBED_EVENTS, event);
}

pub fn record_embed_serve(outcome: &'static str) {
    bump(&EMBED_SERVES, outcome);
}

/// `outcome`: `"sent"` | `"denied"` | `"error"`.
pub fn record_monitor_notification(outcome: &'static str) {
    bump(&MONITOR_NOTIFICATIONS, outcome);
}

/// Overall budget for building the response; a gauge query still in flight is dropped rather than
/// holding a scraper open.
const METRICS_TIMEOUT: Duration = Duration::from_secs(2);

fn counter_header(name: &str, help: &str) -> String {
    format!("# HELP {name} {help}\n# TYPE {name} counter\n")
}

fn gauge_header(name: &str, help: &str) -> String {
    format!("# HELP {name} {help}\n# TYPE {name} gauge\n")
}

/// Renders a whole counter family: header, then one series per key, `labels` formatting the
/// `{...}` contents.
fn push_counter_family<K>(
    text: &mut String,
    name: &str,
    help: &str,
    map: &CounterMap<K>,
    labels: impl Fn(&K) -> String,
) {
    text.push_str(&counter_header(name, help));
    for (key, count) in lock(map).iter() {
        let _ = writeln!(text, "{name}{{{}}} {count}", labels(key));
    }
}

/// Constant-time comparison so a `METRICS_TOKEN` bearer can't be brute-forced by timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

async fn body(state: &AppState) -> String {
    let mut text = counter_header(
        "arcodash_rate_limit_rejections_total",
        "Requests rejected by a rate limiter, by limiter kind.",
    );
    for (limiter, counter) in [
        ("ip", &RATE_LIMIT_REJECTIONS_IP),
        ("user", &RATE_LIMIT_REJECTIONS_USER),
    ] {
        let _ = writeln!(
            text,
            "arcodash_rate_limit_rejections_total{{limiter=\"{limiter}\"}} {}",
            counter.load(Ordering::Relaxed)
        );
    }

    match job_service::org_job_counts(&state.redis, None).await {
        Ok(counts) => {
            text.push_str(&gauge_header(
                "arcodash_jobs_by_status",
                "In-flight/recent jobs across every org, by status.",
            ));
            for (status, value) in [
                ("queued", counts.queued),
                ("in_flight", counts.in_flight),
                ("recent_success", counts.recent_success),
                ("recent_failure", counts.recent_failure),
                ("recent_cancelled", counts.recent_cancelled),
            ] {
                let _ = writeln!(
                    text,
                    "arcodash_jobs_by_status{{status=\"{status}\"}} {value}"
                );
            }
        }
        Err(e) => {
            tracing::warn!(error = ?e, "metrics: job counts query failed, omitting arcodash_jobs_by_status");
        }
    }

    push_query_execution_families(&mut text);
    push_counter_family(
        &mut text,
        "arcodash_authz_decisions_total",
        "authz::authorize::authorize decisions by capability and outcome.",
        &AUTHZ_DECISIONS,
        |(capability, outcome)| format!("capability=\"{capability}\",outcome=\"{outcome}\""),
    );
    push_counter_family(
        &mut text,
        "arcodash_ai_provider_calls_total",
        "AI provider HTTP calls (generation and test_key) by provider, call, and outcome.",
        &AI_PROVIDER_CALLS,
        |(provider, call, outcome)| {
            format!("provider=\"{provider}\",call=\"{call}\",outcome=\"{outcome}\"")
        },
    );
    push_counter_family(
        &mut text,
        "arcodash_embed_events_total",
        "embed_service::create_embed/revoke_embed actually taking effect, by event.",
        &EMBED_EVENTS,
        |event| format!("event=\"{event}\""),
    );
    push_counter_family(
        &mut text,
        "arcodash_embed_serves_total",
        "GET /api/embed/{token} and GET /api/dashboards/public/{token} resolutions, by outcome.",
        &EMBED_SERVES,
        |outcome| format!("outcome=\"{outcome}\""),
    );
    push_counter_family(
        &mut text,
        "arcodash_monitor_notifications_total",
        "monitor_service::notify_subscription (\"Send now\") resolutions, by outcome.",
        &MONITOR_NOTIFICATIONS,
        |outcome| format!("outcome=\"{outcome}\""),
    );
    push_counter_family(
        &mut text,
        "arcodash_http_requests_total",
        "HTTP responses by method, matched route template, and status class.",
        &HTTP_REQUESTS,
        |(method, route, status)| {
            format!("method=\"{method}\",route=\"{route}\",status=\"{status}\"")
        },
    );
    push_counter_family(
        &mut text,
        "arcodash_auth_outcomes_total",
        "Authentication attempts resolving, by method and outcome.",
        &AUTH_OUTCOMES,
        |(method, outcome)| format!("method=\"{method}\",outcome=\"{outcome}\""),
    );

    text.push_str(&counter_header(
        "arcodash_schedule_auto_pause_cleared_total",
        "schedule_service::update clearing a failure auto-pause.",
    ));
    let _ = writeln!(
        text,
        "arcodash_schedule_auto_pause_cleared_total {}",
        SCHEDULE_AUTO_PAUSE_CLEARED.load(Ordering::Relaxed)
    );

    append_run_gauges(&mut text, &state.db).await;

    arcodash_core::metrics::render(&mut text);

    text
}

fn push_query_execution_families(text: &mut String) {
    push_counter_family(
        text,
        "arcodash_query_executions_total",
        "query_service::execute/run_service::run_now outcomes by path and outcome.",
        &QUERY_EXECUTIONS,
        |(path, outcome)| format!("path=\"{path}\",outcome=\"{outcome}\""),
    );
    text.push_str(&counter_header(
        "arcodash_query_result_cache_lookups_total",
        "find_fresh_cached_result lookups inside query_service::execute, by hit/miss.",
    ));
    for (outcome, counter) in [("hit", &CACHE_LOOKUP_HITS), ("miss", &CACHE_LOOKUP_MISSES)] {
        let _ = writeln!(
            text,
            "arcodash_query_result_cache_lookups_total{{outcome=\"{outcome}\"}} {}",
            counter.load(Ordering::Relaxed)
        );
    }
}

#[derive(FromQueryResult)]
struct CountRow {
    n: i64,
}

/// Single-value gauge from a `SELECT count(*) AS n ...`; a failed query is logged and omits the
/// family.
async fn push_count_gauge(
    text: &mut String,
    db: &sea_orm::DatabaseConnection,
    sql: &str,
    name: &str,
    help: &str,
    what: &str,
) {
    match CountRow::find_by_statement(Statement::from_string(DbBackend::Postgres, sql))
        .one(db)
        .await
    {
        Ok(Some(row)) => {
            text.push_str(&gauge_header(name, help));
            let _ = writeln!(text, "{name} {}", row.n);
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = ?e, "metrics: {what} query failed, omitting {name}");
        }
    }
}

/// Scrape-time gauges derived from `runs` over the last hour. Each query is independent; a failure
/// is logged and omits that family.
async fn append_run_gauges(text: &mut String, db: &sea_orm::DatabaseConnection) {
    #[derive(FromQueryResult)]
    struct LatencyRow {
        queue_p50: Option<f64>,
        queue_p95: Option<f64>,
        dur_p50: Option<f64>,
        dur_p95: Option<f64>,
    }
    #[derive(FromQueryResult)]
    struct StatusRow {
        status: String,
        n: i64,
    }
    match StatusRow::find_by_statement(Statement::from_string(
        DbBackend::Postgres,
        "SELECT status, count(*) AS n FROM runs \
          WHERE created_at > now() - interval '1 hour' GROUP BY status",
    ))
    .all(db)
    .await
    {
        Ok(rows) => {
            text.push_str(&gauge_header(
                "arcodash_runs_by_status",
                "runs created in the last hour, by status.",
            ));
            for row in rows {
                let _ = writeln!(
                    text,
                    "arcodash_runs_by_status{{status=\"{}\"}} {}",
                    row.status, row.n
                );
            }
        }
        Err(e) => {
            tracing::warn!(error = ?e, "metrics: runs-by-status query failed, omitting arcodash_runs_by_status");
        }
    }

    match LatencyRow::find_by_statement(Statement::from_string(
        DbBackend::Postgres,
        "SELECT \
           percentile_cont(0.5) WITHIN GROUP (ORDER BY EXTRACT(EPOCH FROM (started_at - enqueued_at))) AS queue_p50, \
           percentile_cont(0.95) WITHIN GROUP (ORDER BY EXTRACT(EPOCH FROM (started_at - enqueued_at))) AS queue_p95, \
           percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_ms) AS dur_p50, \
           percentile_cont(0.95) WITHIN GROUP (ORDER BY duration_ms) AS dur_p95 \
         FROM runs WHERE created_at > now() - interval '1 hour'",
    ))
    .one(db)
    .await
    {
        Ok(Some(row)) => {
            text.push_str(&gauge_header(
                "arcodash_run_queue_latency_seconds",
                "Time between a run's enqueue and its actual start, last hour, by quantile.",
            ));
            for (q, v) in [("0.5", row.queue_p50), ("0.95", row.queue_p95)] {
                if let Some(v) = v {
                    let _ = writeln!(text, "arcodash_run_queue_latency_seconds{{quantile=\"{q}\"}} {v}");
                }
            }
            text.push_str(&gauge_header(
                "arcodash_run_duration_seconds",
                "Run execution duration, last hour, by quantile.",
            ));
            for (q, v) in [("0.5", row.dur_p50), ("0.95", row.dur_p95)] {
                if let Some(v) = v {
                    let secs = v / 1000.0;
                    let _ = writeln!(text, "arcodash_run_duration_seconds{{quantile=\"{q}\"}} {secs}");
                }
            }
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = ?e, "metrics: run latency query failed, omitting arcodash_run_queue_latency_seconds/arcodash_run_duration_seconds");
        }
    }

    // Matches `DEFAULT_DS_CONCURRENCY`, the fallback wherever `max_concurrent_runs` is unset.
    push_count_gauge(
        text,
        db,
        "SELECT count(*) AS n FROM ( \
           SELECT r.data_source_id \
             FROM runs r JOIN data_sources d ON d.id = r.data_source_id \
            WHERE r.status = 'running' AND r.data_source_id IS NOT NULL \
            GROUP BY r.data_source_id, d.max_concurrent_runs \
           HAVING count(*) >= COALESCE(d.max_concurrent_runs, 4)) s",
        "arcodash_data_sources_at_concurrency_cap",
        "Data sources currently at or over their max_concurrent_runs cap.",
        "data-source saturation",
    )
    .await;

    // The worker meters the auto-pause transition; this gauge shows how many schedules are stuck
    // right now.
    push_count_gauge(
        text,
        db,
        "SELECT count(*) AS n FROM schedules WHERE auto_paused_at IS NOT NULL",
        "arcodash_schedules_auto_paused",
        "Schedules currently auto-paused after repeated failures.",
        "auto-paused schedule count",
    )
    .await;
}

pub async fn metrics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let expected = std::env::var("METRICS_TOKEN").unwrap_or_default();
    if expected.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let authorized = bearer_token(&headers)
        .is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()));
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let text = tokio::time::timeout(METRICS_TIMEOUT, body(&state))
        .await
        .unwrap_or_else(|_| {
            tracing::warn!("metrics: handler exceeded its time budget, returning counters only");
            String::new()
        });
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], text).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_and_mismatches() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrey"));
        assert!(!constant_time_eq(b"secret", b"shorter"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn incr_and_read_counters() {
        let ip = || RATE_LIMIT_REJECTIONS_IP.load(Ordering::Relaxed);
        let user = || RATE_LIMIT_REJECTIONS_USER.load(Ordering::Relaxed);
        let (before_ip, before_user) = (ip(), user());
        incr_ip_rejection();
        incr_user_rejection();
        incr_user_rejection();
        assert_eq!(ip(), before_ip + 1);
        assert_eq!(user(), before_user + 2);
    }

    #[test]
    fn query_execution_and_cache_lookup_counters_render_as_valid_exposition() {
        incr_query_execution("interactive", "cached");
        incr_query_execution("interactive", "enqueued");
        incr_query_execution("run", "enqueued");
        incr_query_result_cache_lookup(true);
        incr_query_result_cache_lookup(false);

        let mut out = String::new();
        push_query_execution_families(&mut out);

        assert!(out.contains("path=\"interactive\",outcome=\"cached\""));
        assert!(out.contains("path=\"interactive\",outcome=\"enqueued\""));
        assert!(out.contains("path=\"run\",outcome=\"enqueued\""));
        assert!(out.contains("arcodash_query_result_cache_lookups_total{outcome=\"hit\"}"));
        assert!(out.contains("arcodash_query_result_cache_lookups_total{outcome=\"miss\"}"));

        let help_count = out
            .matches("# HELP arcodash_query_executions_total")
            .count();
        assert_eq!(help_count, 1, "family must be declared exactly once");
        let type_idx = out.find("# TYPE arcodash_query_executions_total").unwrap();
        let first_series_idx = out.find("arcodash_query_executions_total{").unwrap();
        assert!(
            type_idx < first_series_idx,
            "TYPE line must precede the first series"
        );
    }

    #[test]
    fn bearer_token_extracts_only_the_bearer_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer abc123".parse().unwrap());
        assert_eq!(bearer_token(&headers), Some("abc123"));

        let mut wrong_scheme = HeaderMap::new();
        wrong_scheme.insert(header::AUTHORIZATION, "Basic abc123".parse().unwrap());
        assert_eq!(bearer_token(&wrong_scheme), None);

        assert_eq!(bearer_token(&HeaderMap::new()), None);
    }

    /// Each test uses a nonce-suffixed label value, since these counters are process-global and
    /// tests run in parallel.
    #[test]
    fn authz_decision_counter_tracks_capability_and_outcome_separately() {
        record_authz_decision("test_only_capability_authz_1", true);
        record_authz_decision("test_only_capability_authz_1", true);
        record_authz_decision("test_only_capability_authz_1", false);

        let guard = lock(&AUTHZ_DECISIONS);
        assert_eq!(guard[&("test_only_capability_authz_1", "allowed")], 2);
        assert_eq!(guard[&("test_only_capability_authz_1", "denied")], 1);
    }

    #[test]
    fn ai_provider_call_counter_tracks_provider_call_and_outcome() {
        record_ai_provider_call("test_only_provider_ai_1", "test_key", "ok");
        record_ai_provider_call("test_only_provider_ai_1", "test_key", "invalid_key");

        let guard = lock(&AI_PROVIDER_CALLS);
        assert_eq!(
            guard[&(
                "test_only_provider_ai_1".to_string(),
                "test_key".to_string(),
                "ok".to_string()
            )],
            1
        );
        assert_eq!(
            guard[&(
                "test_only_provider_ai_1".to_string(),
                "test_key".to_string(),
                "invalid_key".to_string()
            )],
            1
        );
    }

    #[test]
    fn embed_event_and_serve_counters_track_their_own_outcome() {
        record_embed_event("created");
        record_embed_event("revoked");
        record_embed_serve("served");
        record_embed_serve("not_found");

        let guard = lock(&EMBED_EVENTS);
        assert!(guard["created"] >= 1);
        assert!(guard["revoked"] >= 1);
        drop(guard);

        let guard = lock(&EMBED_SERVES);
        assert!(guard["served"] >= 1);
        assert!(guard["not_found"] >= 1);
    }

    #[test]
    fn monitor_notification_counter_tracks_outcome() {
        record_monitor_notification("sent");
        record_monitor_notification("denied");

        let guard = lock(&MONITOR_NOTIFICATIONS);
        assert!(guard["sent"] >= 1);
        assert!(guard["denied"] >= 1);
    }

    /// These families render as valid Prometheus exposition.
    #[test]
    fn new_counter_families_declare_help_and_type_before_their_series() {
        record_authz_decision("test_only_capability_authz_2", true);
        record_ai_provider_call("test_only_provider_ai_2", "test_key", "ok");
        record_embed_event("created");
        record_embed_serve("served");
        record_monitor_notification("sent");
        record_http_request("GET", "/api/test_only_route".to_string(), "2xx");
        record_auth_outcome("password", "success");

        const FAMILIES: [&str; 7] = [
            "arcodash_authz_decisions_total",
            "arcodash_ai_provider_calls_total",
            "arcodash_embed_events_total",
            "arcodash_embed_serves_total",
            "arcodash_monitor_notifications_total",
            "arcodash_http_requests_total",
            "arcodash_auth_outcomes_total",
        ];
        let mut out = String::new();
        for name in FAMILIES {
            out.push_str(&counter_header(name, "test"));
            out.push_str(&format!("{name}{{x=\"y\"}} 1\n"));
        }
        for name in FAMILIES {
            let type_idx = out.find(&format!("# TYPE {name}")).unwrap();
            let first_series_idx = out.find(&format!("{name}{{")).unwrap();
            assert!(
                type_idx < first_series_idx,
                "TYPE line must precede {name}'s first series"
            );
        }
    }

    /// Two requests differing only by id collapse onto one series keyed on the route template.
    #[test]
    fn http_requests_counter_labels_by_template_not_raw_path() {
        let route = "/api/test_only_template_route/{id}".to_string();
        record_http_request("GET", route.clone(), "2xx");
        record_http_request("GET", route.clone(), "2xx");

        let guard = lock(&HTTP_REQUESTS);
        assert_eq!(guard[&("GET", route, "2xx")], 2);
    }

    #[test]
    fn status_class_collapses_codes_to_their_hundreds_digit() {
        assert_eq!(status_class(StatusCode::OK), "2xx");
        assert_eq!(status_class(StatusCode::NOT_FOUND), "4xx");
        assert_eq!(status_class(StatusCode::INTERNAL_SERVER_ERROR), "5xx");
    }

    #[test]
    fn auth_outcome_counter_tracks_method_and_outcome_separately() {
        record_auth_outcome("test_only_method_auth_1", "success");
        record_auth_outcome("test_only_method_auth_1", "success");
        record_auth_outcome("test_only_method_auth_1", "bad_credentials");

        let guard = lock(&AUTH_OUTCOMES);
        assert_eq!(guard[&("test_only_method_auth_1", "success")], 2);
        assert_eq!(guard[&("test_only_method_auth_1", "bad_credentials")], 1);
    }

    #[test]
    fn schedule_auto_pause_cleared_counter_increments() {
        let before = SCHEDULE_AUTO_PAUSE_CLEARED.load(Ordering::Relaxed);
        incr_schedule_auto_pause_cleared();
        assert_eq!(
            SCHEDULE_AUTO_PAUSE_CLEARED.load(Ordering::Relaxed),
            before + 1
        );
    }
}
