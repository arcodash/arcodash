use super::{parse_bool_env, Config};

/// The `CONVEYOR_*_ENABLED` flags `apps/backend` and `apps/worker` each read independently — see
/// [`cross_binary_fingerprint`]. `CONVEYOR_SCHEDULER_ENABLED` is deliberately not here: it has no
/// reader in the API backend (only `apps/worker/src/conveyor_schedules.rs` consults it), so the
/// two binaries disagreeing on it is not drift worth warning about.
const CROSS_BINARY_CONVEYOR_FLAGS: [&str; 5] = [
    "CONVEYOR_SEND_NOTIFICATION_ENABLED",
    "CONVEYOR_EVALUATE_MONITOR_ENABLED",
    "CONVEYOR_EXECUTE_QUERY_ENABLED",
    "CONVEYOR_SEND_DIGEST_ENABLED",
    "CONVEYOR_SEND_MAIL_ENABLED",
];

/// Pure hash core behind [`cross_binary_fingerprint`], split out so it's testable without
/// touching process env (the flag names are read by every other test file in this crate too,
/// under default parallel test threads — a test that also read live env here would race them).
fn fingerprint_from_values(flags: [bool; 5], max_query_rows: u32) -> String {
    use sha2::{Digest, Sha256};
    // `DefaultHasher`'s output isn't guaranteed stable across Rust releases (std says so
    // explicitly) — two binaries built with different toolchains would then mismatch forever,
    // making the drift warning below fire on every boot with nothing actually wrong.
    let mut hasher = Sha256::new();
    for flag in flags {
        hasher.update([u8::from(flag)]);
    }
    hasher.update(max_query_rows.to_le_bytes());
    crate::secrets::hex_encode(&hasher.finalize())
}

/// Hash of every setting the API backend (`apps/backend` or an embedding binary) and `apps/worker`
/// — two independently-deployed processes — must agree on: [`CROSS_BINARY_CONVEYOR_FLAGS`] plus
/// `max_query_rows`.
///
/// Not a security digest, just a cheap way for each binary to notice the other disagrees (see
/// `publish_and_check_cross_binary_fingerprint`); a parse error on one of the flags is folded in as
/// `false`, the same fallback its own reader applies.
#[must_use]
pub fn cross_binary_fingerprint(config: &Config) -> String {
    let flags =
        CROSS_BINARY_CONVEYOR_FLAGS.map(|name| parse_bool_env(name, false).unwrap_or(false));
    fingerprint_from_values(flags, config.max_query_rows)
}

/// TTL on the fingerprint each binary publishes at startup — long enough to catch a stale but
/// still-running replica on the other side, short enough that a decommissioned binary's key ages
/// out on its own rather than triggering a false mismatch warning forever.
const CONFIG_FINGERPRINT_TTL_SECS: u64 = 24 * 60 * 60;

fn config_fingerprint_key(binary: &str) -> String {
    format!("deploy:config_fingerprint:{binary}")
}

/// Publish this process's [`cross_binary_fingerprint`] and warn if the other binary's
/// last-published one disagrees.
///
/// Never a hard failure: a rolling deploy legitimately has one side ahead of the other for the few
/// minutes a rollout takes, and refusing to boot mid-rollout would be worse than the drift this is
/// meant to catch.
pub async fn publish_and_check_cross_binary_fingerprint(
    redis: &mut redis::aio::ConnectionManager,
    this_binary: &str,
    other_binary: &str,
    fingerprint: &str,
) {
    use redis::AsyncCommands;
    let own_key = config_fingerprint_key(this_binary);
    if let Err(e) = redis
        .set_ex::<_, _, ()>(&own_key, fingerprint, CONFIG_FINGERPRINT_TTL_SECS)
        .await
    {
        tracing::warn!(key = %own_key, error = %e, "failed to publish cross-binary config fingerprint");
        return;
    }
    let other_key = config_fingerprint_key(other_binary);
    match redis.get::<_, Option<String>>(&other_key).await {
        Ok(Some(other_fp)) if other_fp != fingerprint => {
            crate::metrics::set_config_fingerprint_check(true, true);
            tracing::warn!(
                own_key = %own_key,
                other_key = %other_key,
                own_fingerprint = %fingerprint,
                other_fingerprint = %other_fp,
                "cross-binary config fingerprint mismatch — the CONVEYOR_*_ENABLED flags and \
                 MAX_QUERY_ROWS must agree between the API backend (apps/backend or \
                 an embedding binary) and apps/worker; expected briefly during a rolling deploy, but \
                 persistent drift silently breaks routing between the two processes"
            );
        }
        Ok(Some(_)) => crate::metrics::set_config_fingerprint_check(false, true),
        Ok(None) => crate::metrics::set_config_fingerprint_check(false, false),
        Err(e) => {
            crate::metrics::set_config_fingerprint_check(false, false);
            tracing::warn!(key = %other_key, error = %e, "failed to read the other binary's cross-binary config fingerprint");
        }
    }
}

/// Interval between cross-binary fingerprint re-checks. Startup alone can't see a config change
/// made to a running service (an env-var edit with no redeploy), and it refreshes each
/// side's key well inside [`CONFIG_FINGERPRINT_TTL_SECS`].
const CONFIG_FINGERPRINT_RECHECK_SECS: u64 = 300;

/// Runs [`publish_and_check_cross_binary_fingerprint`] once immediately (matching the existing
/// startup check), then on a fixed interval for the life of the process.
///
/// Fire-and-forget: the handle is dropped, same lifetime as `middleware::rate_limit`'s sweep loop.
pub fn spawn_cross_binary_fingerprint_check(
    mut redis: redis::aio::ConnectionManager,
    this_binary: &'static str,
    other_binary: &'static str,
    fingerprint: String,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            CONFIG_FINGERPRINT_RECHECK_SECS,
        ));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            publish_and_check_cross_binary_fingerprint(
                &mut redis,
                this_binary,
                other_binary,
                &fingerprint,
            )
            .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_from_values_is_stable_for_identical_inputs() {
        assert_eq!(
            fingerprint_from_values([true, false, true, false, true], 10_000),
            fingerprint_from_values([true, false, true, false, true], 10_000)
        );
    }

    #[test]
    fn fingerprint_from_values_changes_when_any_one_flag_differs() {
        let base = fingerprint_from_values([false; 5], 10_000);
        for i in 0..5 {
            let mut flipped = [false; 5];
            flipped[i] = true;
            assert_ne!(
                base,
                fingerprint_from_values(flipped, 10_000),
                "flag {i} must change the fingerprint"
            );
        }
    }

    #[test]
    fn fingerprint_from_values_changes_when_max_query_rows_differs() {
        let base = fingerprint_from_values([false; 5], 10_000);
        assert_ne!(base, fingerprint_from_values([false; 5], 10_001));
    }

    /// Pinned to a literal digest, not compared against another `fingerprint_from_values` call —
    /// the three tests above only ever compare fingerprints to each other, so a hash that drifts
    /// across Rust toolchains would pass all three while still breaking the real cross-binary
    /// check this function backs. Only a fixed expected value catches that.
    #[test]
    fn fingerprint_from_values_matches_a_pinned_digest() {
        assert_eq!(
            fingerprint_from_values([true, false, true, false, true], 10_000),
            "88668539a3ce86ac4a3e17854b1db3c3c12967b94a1811098a2f28024737d2d7"
        );
    }

    fn rendered_metrics() -> String {
        let mut rendered = String::new();
        crate::metrics::render(&mut rendered);
        rendered
    }

    #[derive(Clone, Default)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Real Redis only (skips cleanly without `TEST_REDIS_URL`). Drives the actual round-trip: a
    /// simulated backend publishes its fingerprint, then `publish_and_check_cross_binary_
    /// fingerprint` runs as the worker side with a deliberately different one, and this asserts the
    /// mismatch warning actually fires (a captured `tracing` subscriber) — not just that the two
    /// hashes differ, which the pure `fingerprint_from_values` tests above already cover. Also
    /// covers the agreeing and peer-absent cases in the same test function: the mismatch/peer-seen
    /// gauges are process-global statics, so asserting on them needs to happen without another
    /// fingerprint-checking test running concurrently in the same process.
    #[tokio::test]
    async fn publish_and_check_cross_binary_fingerprint_warns_on_disagreement() {
        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let mut redis = crate::valkey_client::connect(&redis_url).await.unwrap();

        // Unique per run so concurrent test runs (or a real backend/worker on the same Redis)
        // never collide on these keys.
        let suffix = uuid::Uuid::new_v4();
        let backend_name = format!("test-backend-{suffix}");
        let worker_name = format!("test-worker-{suffix}");

        let backend_fp = fingerprint_from_values([true, false, false, false, false], 10_000);
        let worker_fp = fingerprint_from_values([false; 5], 10_000);
        assert_ne!(
            backend_fp, worker_fp,
            "test setup must pick disagreeing fingerprints"
        );

        // Simulate the backend having already published its (different) fingerprint.
        use redis::AsyncCommands;
        redis
            .set_ex::<_, _, ()>(
                format!("deploy:config_fingerprint:{backend_name}"),
                &backend_fp,
                60,
            )
            .await
            .unwrap();

        let buf = SharedBuf::default();
        let writer_buf = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer_buf.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        publish_and_check_cross_binary_fingerprint(
            &mut redis,
            &worker_name,
            &backend_name,
            &worker_fp,
        )
        .await;

        let text = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
        assert!(
            text.contains("cross-binary config fingerprint mismatch"),
            "expected a mismatch warning, got: {text}"
        );

        let rendered = rendered_metrics();
        assert!(
            rendered.contains("arcodash_config_fingerprint_mismatch 1"),
            "expected the mismatch gauge to read 1: {rendered}"
        );
        assert!(
            rendered.contains("arcodash_config_fingerprint_peer_seen 1"),
            "expected the peer-seen gauge to read 1: {rendered}"
        );

        // Agreeing case: publish the same fingerprint under a fresh pair of keys.
        let agree_backend_name = format!("test-backend-agree-{suffix}");
        let agree_worker_name = format!("test-worker-agree-{suffix}");
        redis
            .set_ex::<_, _, ()>(
                format!("deploy:config_fingerprint:{agree_backend_name}"),
                &worker_fp,
                60,
            )
            .await
            .unwrap();
        publish_and_check_cross_binary_fingerprint(
            &mut redis,
            &agree_worker_name,
            &agree_backend_name,
            &worker_fp,
        )
        .await;
        let rendered = rendered_metrics();
        assert!(
            rendered.contains("arcodash_config_fingerprint_mismatch 0"),
            "expected the mismatch gauge to read 0 on agreement: {rendered}"
        );
        assert!(
            rendered.contains("arcodash_config_fingerprint_peer_seen 1"),
            "expected the peer-seen gauge to read 1 on agreement: {rendered}"
        );

        // Peer-absent case: the other binary's key was never published.
        let absent_worker_name = format!("test-worker-absent-{suffix}");
        let absent_backend_name = format!("test-backend-absent-{suffix}");
        publish_and_check_cross_binary_fingerprint(
            &mut redis,
            &absent_worker_name,
            &absent_backend_name,
            &worker_fp,
        )
        .await;
        let rendered = rendered_metrics();
        assert!(
            rendered.contains("arcodash_config_fingerprint_peer_seen 0"),
            "expected the peer-seen gauge to read 0 when the peer never published: {rendered}"
        );
    }
}
