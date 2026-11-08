//! Worker liveness heartbeat: this process periodically writes a `worker:<id>` key into Redis so
//! operators can list live worker processes and `job_service::worker_status` (backend crate) can
//! report an aggregate online count on the org-scoped `/api/status`. That endpoint only counts live
//! keys, never the hostname/PID encoded in the key names (global infrastructure detail).
//!
//! Key: `worker:<hostname>-<pid>` -> JSON [`WorkerHeartbeat`], `SET`+`EX` ([`HEARTBEAT_TTL_SECS`]).
//! The id is fixed for the process's life, so each tick renews the same key.
//!
//! The TTL is what guarantees a dead worker disappears: `kill -9`, an OOM kill or power loss never
//! reach the shutdown branch. The `DEL` on clean shutdown is only an optimization so a graceful
//! restart leaves no ghost entry, and is best-effort (log + continue): the process is already
//! tearing down in parallel (see `crate::run`), and a transient Redis error is not worth blocking
//! exit over.

use std::time::Duration;

use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use tokio::sync::watch;

use arcodash_core::redis_util::WorkerHeartbeat;

const HEARTBEAT_TICK_SECS: u64 = 10;
const HEARTBEAT_TTL_SECS: u64 = 30;

/// Tick every [`HEARTBEAT_TICK_SECS`] writing this process's `worker:<id>` key (see module docs
/// for the key format/TTL), until `shutdown` fires, then best-effort `DEL` the key once.
pub async fn run(
    mut redis: ConnectionManager,
    concurrency: usize,
    mut shutdown: watch::Receiver<bool>,
) {
    let hostname = arcodash_core::worker_identity::hostname().to_string();
    let pid = std::process::id();
    let id = arcodash_core::worker_identity::worker_id().to_string();
    let key = format!("worker:{id}");
    let started_at = chrono::Utc::now().to_rfc3339();

    let mut interval = tokio::time::interval(Duration::from_secs(HEARTBEAT_TICK_SECS));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let hb = WorkerHeartbeat {
                    id: id.clone(),
                    hostname: hostname.clone(),
                    pid,
                    started_at: started_at.clone(),
                    concurrency,
                    last_heartbeat: chrono::Utc::now().to_rfc3339(),
                };
                match serde_json::to_string(&hb) {
                    Ok(json) => {
                        if let Err(e) = redis.set_ex::<_, _, ()>(&key, json, HEARTBEAT_TTL_SECS).await {
                            tracing::warn!("heartbeat SET failed: {e}");
                        }
                    }
                    Err(e) => tracing::warn!("heartbeat serialize failed: {e}"),
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
        }
    }
    if let Err(e) = redis.del::<_, ()>(&key).await {
        tracing::warn!("heartbeat DEL on shutdown failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_serializes_with_expected_snake_case_keys() {
        let hb = WorkerHeartbeat {
            id: "host-123".to_string(),
            hostname: "host".to_string(),
            pid: 123,
            started_at: "2026-01-01T00:00:00+00:00".to_string(),
            concurrency: 4,
            last_heartbeat: "2026-01-01T00:00:10+00:00".to_string(),
        };

        let json = serde_json::to_string(&hb).expect("serialize failed");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse failed");
        let obj = value.as_object().expect("must serialize to a JSON object");

        for key in [
            "id",
            "hostname",
            "pid",
            "started_at",
            "concurrency",
            "last_heartbeat",
        ] {
            assert!(obj.contains_key(key), "missing expected key: {key}");
        }
        assert_eq!(obj.len(), 6, "expected exactly 6 keys, got {obj:?}");

        chrono::DateTime::parse_from_rfc3339(&hb.started_at)
            .expect("started_at must round-trip via RFC3339");
        chrono::DateTime::parse_from_rfc3339(&hb.last_heartbeat)
            .expect("last_heartbeat must round-trip via RFC3339");
    }
}
