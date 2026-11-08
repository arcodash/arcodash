//! Process-local Prometheus-format counters for `crates/shared`.
//!
//! Static atomics/maps rendered to text on demand — no metrics crate, matching the pattern
//! `apps/worker/src/health.rs` already uses for the worker's own counters. Counters reset on
//! restart, which is correct for a Prometheus counter and not something to try to persist.
//!
//! Nothing in this crate serves `/metrics` itself — a caller with an HTTP endpoint (the worker's
//! `collect_metrics`) calls [`render`] and appends the result to its own response body.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::connectors::ConnectorErrorKind;

type OperationsKey = (&'static str, &'static str, &'static str);
type ErrorsKey = (&'static str, &'static str);
type ExecuteEnqueueKey = (&'static str, &'static str);

type Counters<K> = OnceLock<Mutex<HashMap<K, u64>>>;

static CONNECTOR_OPERATIONS: Counters<OperationsKey> = OnceLock::new();
static CONNECTOR_ERRORS: Counters<ErrorsKey> = OnceLock::new();
static EXECUTE_ENQUEUE: Counters<ExecuteEnqueueKey> = OnceLock::new();
static SSRF_GUARD_REJECTIONS: AtomicU64 = AtomicU64::new(0);
static SECRET_DECRYPT_FAILURES: AtomicU64 = AtomicU64::new(0);
static MONITOR_OPTIONS_CORRUPT: AtomicU64 = AtomicU64::new(0);
/// Guards tests that read exact before/after values of `MONITOR_OPTIONS_CORRUPT` — it's bumped
/// from both this module's and `models.rs`'s test suites, and cargo runs tests in parallel by
/// default, so an unguarded read races with unrelated tests' increments.
#[cfg(test)]
pub(crate) static MONITOR_OPTIONS_CORRUPT_TEST_LOCK: Mutex<()> = Mutex::new(());
/// Test-only read of the counter's raw value — callers outside this module (e.g. `models.rs`'s
/// tests) can't see the private `MONITOR_OPTIONS_CORRUPT` static directly.
#[cfg(test)]
pub(crate) fn monitor_options_corrupt_count() -> u64 {
    MONITOR_OPTIONS_CORRUPT.load(Ordering::Relaxed)
}
/// `1` when the most recently completed cross-binary fingerprint check found the peer's
/// published value and it disagreed, `0` otherwise (including "no check has run yet").
static CONFIG_FINGERPRINT_MISMATCH: AtomicU64 = AtomicU64::new(0);
/// `1` when the most recently completed cross-binary fingerprint check found the peer's key
/// published at all, `0` when it was absent or unreadable — distinct from the mismatch gauge
/// because "the peer never published" and "the peer disagrees" call for different operator
/// responses.
static CONFIG_FINGERPRINT_PEER_SEEN: AtomicU64 = AtomicU64::new(0);

fn lock<K>(counters: &'static Counters<K>) -> MutexGuard<'static, HashMap<K, u64>> {
    counters
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const fn connector_error_kind_label(kind: ConnectorErrorKind) -> &'static str {
    match kind {
        ConnectorErrorKind::Connect => "connect",
        ConnectorErrorKind::Statement => "statement",
    }
}

/// One `QueryRunner` I/O method finishing, from `registry::LoggingConnector` — the one place all
/// three methods, across every connector, already pass through.
pub fn record_connector_operation(
    connector: &'static str,
    op: &'static str,
    outcome: &'static str,
) {
    *lock(&CONNECTOR_OPERATIONS)
        .entry((connector, op, outcome))
        .or_insert(0) += 1;
}

/// A connector operation's failure, broken out by [`ConnectorErrorKind`] — reuses the
/// classification `registry::LoggingConnector` already computes for logging rather than
/// re-deriving it here.
pub fn record_connector_error(connector: &'static str, kind: ConnectorErrorKind) {
    *lock(&CONNECTOR_ERRORS)
        .entry((connector, connector_error_kind_label(kind)))
        .or_insert(0) += 1;
}

/// One `enqueue_execute_query` call resolving.
///
/// `backend`: `"conveyor"` | `"legacy"`; `outcome`:
/// `"queued"` | `"parked"` | `"already_inflight"` | `"error"` (legacy only ever reports
/// `"queued"`/`"error"` — it has no parking or in-flight-dedup concept).
pub fn record_execute_enqueue(backend: &'static str, outcome: &'static str) {
    *lock(&EXECUTE_ENQUEUE)
        .entry((backend, outcome))
        .or_insert(0) += 1;
}

/// `net_guard::guard_host` rejecting a private/internal address.
pub fn record_ssrf_guard_rejection() {
    SSRF_GUARD_REJECTIONS.fetch_add(1, Ordering::Relaxed);
}

/// `secrets::decrypt` failing — wrong key, corrupt/tampered ciphertext, or truncated input.
pub fn record_secret_decrypt_failure() {
    SECRET_DECRYPT_FAILURES.fetch_add(1, Ordering::Relaxed);
}

/// A monitor's stored `options` failing to parse — its condition and silence state are then
/// only partly recoverable, so the monitor is not behaving as configured.
pub fn record_monitor_options_corrupt() {
    MONITOR_OPTIONS_CORRUPT.fetch_add(1, Ordering::Relaxed);
}

/// Records the outcome of one cross-binary config fingerprint check (see
/// `config::publish_and_check_cross_binary_fingerprint`).
pub fn set_config_fingerprint_check(mismatch: bool, peer_seen: bool) {
    CONFIG_FINGERPRINT_MISMATCH.store(u64::from(mismatch), Ordering::Relaxed);
    CONFIG_FINGERPRINT_PEER_SEEN.store(u64::from(peer_seen), Ordering::Relaxed);
}

/// Append every counter this module tracks to `out` in Prometheus text exposition format.
pub fn render(out: &mut String) {
    let _ = writeln!(
        out,
        "# HELP arcodash_connector_operations_total Connector I/O method calls by outcome.\n\
         # TYPE arcodash_connector_operations_total counter"
    );
    for ((connector, op, outcome), count) in lock(&CONNECTOR_OPERATIONS).iter() {
        let _ = writeln!(
            out,
            "arcodash_connector_operations_total{{connector=\"{connector}\",op=\"{op}\",outcome=\"{outcome}\"}} {count}"
        );
    }

    let _ = writeln!(
        out,
        "# HELP arcodash_connector_errors_total Connector operation failures by error kind.\n\
         # TYPE arcodash_connector_errors_total counter"
    );
    for ((connector, kind), count) in lock(&CONNECTOR_ERRORS).iter() {
        let _ = writeln!(
            out,
            "arcodash_connector_errors_total{{connector=\"{connector}\",kind=\"{kind}\"}} {count}"
        );
    }

    let _ = writeln!(
        out,
        "# HELP arcodash_execute_enqueue_total execute_query enqueue attempts by backend and outcome.\n\
         # TYPE arcodash_execute_enqueue_total counter"
    );
    for ((backend, outcome), count) in lock(&EXECUTE_ENQUEUE).iter() {
        let _ = writeln!(
            out,
            "arcodash_execute_enqueue_total{{backend=\"{backend}\",outcome=\"{outcome}\"}} {count}"
        );
    }

    for (name, kind, help, value) in [
        (
            "arcodash_ssrf_guard_rejections_total",
            "counter",
            "Connections blocked by the SSRF egress guard.",
            &SSRF_GUARD_REJECTIONS,
        ),
        (
            "arcodash_secret_decrypt_failures_total",
            "counter",
            "Failed secrets::decrypt calls.",
            &SECRET_DECRYPT_FAILURES,
        ),
        (
            "arcodash_monitor_options_corrupt_total",
            "counter",
            "Monitors whose stored options JSON could not be parsed.",
            &MONITOR_OPTIONS_CORRUPT,
        ),
        (
            "arcodash_config_fingerprint_mismatch",
            "gauge",
            "Whether the last cross-binary config fingerprint check disagreed with the peer.",
            &CONFIG_FINGERPRINT_MISMATCH,
        ),
        (
            "arcodash_config_fingerprint_peer_seen",
            "gauge",
            "Whether the last cross-binary config fingerprint check found the peer's published value.",
            &CONFIG_FINGERPRINT_PEER_SEEN,
        ),
    ] {
        let _ = writeln!(
            out,
            "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {}",
            value.load(Ordering::Relaxed)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_emits_the_metric_name_type_line_and_value_for_each_counter() {
        let _counter = crate::test_support::hold_decrypt_counter();
        let mut out = String::new();
        record_connector_operation("pg_test_render", "run_query", "ok");
        record_connector_error("pg_test_render", ConnectorErrorKind::Connect);
        record_ssrf_guard_rejection();
        record_secret_decrypt_failure();
        render(&mut out);

        assert!(out.contains("# TYPE arcodash_connector_operations_total counter"));
        assert!(out.contains(
            "arcodash_connector_operations_total{connector=\"pg_test_render\",op=\"run_query\",outcome=\"ok\"}"
        ));
        assert!(out.contains("# TYPE arcodash_connector_errors_total counter"));
        assert!(out.contains(
            "arcodash_connector_errors_total{connector=\"pg_test_render\",kind=\"connect\"}"
        ));
        assert!(out.contains("# TYPE arcodash_ssrf_guard_rejections_total counter"));
        assert!(out.contains("# TYPE arcodash_secret_decrypt_failures_total counter"));
    }

    #[test]
    fn execute_enqueue_family_renders_backend_and_outcome_labels() {
        let mut out = String::new();
        record_execute_enqueue("conveyor", "queued");
        record_execute_enqueue("legacy", "error");
        render(&mut out);
        assert!(out.contains("# TYPE arcodash_execute_enqueue_total counter"));
        assert!(
            out.contains("arcodash_execute_enqueue_total{backend=\"conveyor\",outcome=\"queued\"}")
        );
        assert!(
            out.contains("arcodash_execute_enqueue_total{backend=\"legacy\",outcome=\"error\"}")
        );
    }

    #[test]
    fn two_distinct_label_sets_render_as_two_series() {
        let mut out = String::new();
        record_connector_operation("distinct_a", "run_query", "ok");
        record_connector_operation("distinct_b", "run_query", "ok");
        render(&mut out);
        assert!(out.contains("connector=\"distinct_a\""));
        assert!(out.contains("connector=\"distinct_b\""));
    }

    #[test]
    fn counters_are_deltas_not_absolutes_across_shared_process_state() {
        let mut before = String::new();
        render(&mut before);
        let before_count = SSRF_GUARD_REJECTIONS.load(Ordering::Relaxed);

        record_ssrf_guard_rejection();

        let mut after = String::new();
        render(&mut after);
        let after_count = SSRF_GUARD_REJECTIONS.load(Ordering::Relaxed);

        assert_eq!(after_count, before_count + 1);
    }

    #[test]
    fn monitor_options_corrupt_renders_type_and_value() {
        // This test's own assertions don't read the counter's value, but the increment below
        // still writes the same process-global `MONITOR_OPTIONS_CORRUPT` a sibling test asserts
        // an exact delta on — hold the lock so that write can't land inside the sibling's window.
        let _guard = MONITOR_OPTIONS_CORRUPT_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        record_monitor_options_corrupt();
        let mut out = String::new();
        render(&mut out);
        assert!(out.contains("# TYPE arcodash_monitor_options_corrupt_total counter"));
        assert!(out.contains("arcodash_monitor_options_corrupt_total "));
    }

    #[test]
    fn monitor_options_corrupt_counter_is_a_delta() {
        let _guard = MONITOR_OPTIONS_CORRUPT_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let before = MONITOR_OPTIONS_CORRUPT.load(Ordering::Relaxed);
        record_monitor_options_corrupt();
        let after_corrupt = MONITOR_OPTIONS_CORRUPT.load(Ordering::Relaxed);
        assert_eq!(after_corrupt, before + 1);

        // A clean `parse_monitor_options` call must not touch this counter.
        let _ = crate::models::parse_monitor_options(
            1,
            &serde_json::json!({"column": "x", "op": "greater than", "value": 1}),
        );
        let after_clean_parse = MONITOR_OPTIONS_CORRUPT.load(Ordering::Relaxed);
        assert_eq!(after_clean_parse, after_corrupt);
    }
}
