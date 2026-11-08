//! Shared heartbeat-lease guard: aborts the spawned renewal task on drop, so an early return, a
//! propagated error, or a handler panic can never leak a task that keeps renewing a lease on a
//! row nobody is actively working on anymore. Also tracks whether the renewal loop itself
//! detected the lease is gone, so a caller can refuse to persist work it may no longer own.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Consecutive renewal-write failures (DB errors, not 0-row updates) before the lease is treated
/// as lost. A single transient error shouldn't abandon a healthy lease; three in a row means the
/// renewal path itself is broken, not just unlucky.
const MAX_CONSECUTIVE_RENEW_ERRORS: u32 = 3;

/// Total heartbeat leases lost across every guard, process-wide — exposed via `/metrics` as
/// `arcodash_heartbeat_lease_lost_total`.
pub static HEARTBEAT_LEASE_LOST: AtomicU64 = AtomicU64::new(0);

/// Wraps a periodic lease-renewal task. There is no `disarm()` — with abort-on-drop, a caller
/// done with the lease (about to finalize, or already finalized) just drops the guard.
pub struct HeartbeatGuard {
    handle: tokio::task::JoinHandle<()>,
    lost: Arc<AtomicBool>,
    lost_notify: Arc<tokio::sync::Notify>,
}

impl HeartbeatGuard {
    /// Spawns a task that calls `renew` every `period` until the guard is dropped. `renew` must
    /// perform the domain's own `UPDATE ... WHERE id = $1 AND status = 'running'` and return its
    /// `rows_affected`: `0` means the row is no longer `running` under this lease (finalized or
    /// reclaimed elsewhere) — the lease is lost. `MAX_CONSECUTIVE_RENEW_ERRORS` consecutive `Err`s
    /// from `renew` (e.g. a DB partition) are treated the same way. Either case stops the loop;
    /// [`HeartbeatGuard::lost`] reports it to the caller.
    pub fn spawn<F, Fut>(period: Duration, renew: F) -> Self
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<u64>> + Send,
    {
        let lost = Arc::new(AtomicBool::new(false));
        let lost_task = lost.clone();
        let lost_notify = Arc::new(tokio::sync::Notify::new());
        let notify_task = lost_notify.clone();
        let handle = tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            let mut consecutive_errors = 0u32;
            loop {
                tick.tick().await;
                match renew().await {
                    Ok(0) => {
                        tracing::warn!(
                            "heartbeat lease lost: the row is no longer running under this lease"
                        );
                        break;
                    }
                    Ok(_) => consecutive_errors = 0,
                    Err(e) => {
                        consecutive_errors += 1;
                        tracing::warn!(error = %e, consecutive_errors, "heartbeat renewal failed");
                        if consecutive_errors >= MAX_CONSECUTIVE_RENEW_ERRORS {
                            break;
                        }
                    }
                }
            }
            lost_task.store(true, Ordering::SeqCst);
            HEARTBEAT_LEASE_LOST.fetch_add(1, Ordering::Relaxed);
            notify_task.notify_waiters();
        });
        Self {
            handle,
            lost,
            lost_notify,
        }
    }

    /// Whether the renewal loop detected the lease is gone (a 0-row renew) or unrenewable
    /// (`MAX_CONSECUTIVE_RENEW_ERRORS` write failures in a row). Check before persisting anything
    /// the lease was meant to protect ownership of.
    #[must_use]
    pub fn lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    /// Resolves once the renewal loop has declared the lease lost; otherwise pending forever, so it
    /// can be raced against the work the lease protects.
    pub async fn lease_lost(&self) {
        let notified = self.lost_notify.notified();
        tokio::pin!(notified);
        // Register before re-reading the flag, or a loss signalled in between the two would be
        // missed: `notify_waiters()` only wakes futures already registered at the time it fires.
        notified.as_mut().enable();
        if self.lost() {
            return;
        }
        notified.await;
    }
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[cfg(test)]
mod tests {
    //! Pure logic, no DB — exercises the guard's own drop/error-counting behavior against a
    //! synthetic `renew` closure. Real-row heartbeat tests (lease actually stops advancing in
    //! Postgres) live alongside each domain's own `spawn_heartbeat` wrapper.
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[tokio::test]
    async fn drop_aborts_the_renewal_task() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_task = calls.clone();
        let guard = HeartbeatGuard::spawn(Duration::from_millis(20), move || {
            let calls = calls_task.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(1)
            }
        });
        tokio::time::sleep(Duration::from_millis(70)).await;
        let count_before_drop = calls.load(Ordering::SeqCst);
        assert!(
            count_before_drop >= 1,
            "renew must have ticked at least once"
        );
        drop(guard);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            count_before_drop,
            "renew must stop ticking once the guard is dropped"
        );
    }

    #[tokio::test]
    async fn zero_rows_affected_marks_the_lease_lost_and_stops_the_loop() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_task = calls.clone();
        let guard = HeartbeatGuard::spawn(Duration::from_millis(20), move || {
            let calls = calls_task.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(0)
            }
        });
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(guard.lost(), "a 0-row renew must mark the lease lost");
        let count_at_lost = calls.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            count_at_lost,
            "the loop must stop renewing once the lease is lost"
        );
    }

    #[tokio::test]
    async fn three_consecutive_errors_mark_the_lease_lost() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_task = calls.clone();
        let guard = HeartbeatGuard::spawn(Duration::from_millis(10), move || {
            let calls = calls_task.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("simulated db error")
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            guard.lost(),
            "three consecutive renew errors must mark the lease lost"
        );
        assert!(calls.load(Ordering::SeqCst) >= 3);
    }

    #[tokio::test]
    async fn lease_lost_resolves_when_the_renewal_loop_declares_the_lease_gone() {
        let guard = HeartbeatGuard::spawn(Duration::from_millis(10), || async { Ok(0) });
        tokio::time::timeout(Duration::from_millis(500), guard.lease_lost())
            .await
            .expect("lease_lost() must resolve once the renewal loop sees a 0-row renew");
    }

    #[tokio::test]
    async fn lease_lost_is_pending_while_the_lease_is_healthy() {
        let guard = HeartbeatGuard::spawn(Duration::from_millis(10), || async { Ok(1) });
        assert!(
            tokio::time::timeout(Duration::from_millis(300), guard.lease_lost())
                .await
                .is_err(),
            "lease_lost() must stay pending while the lease keeps renewing successfully"
        );
    }

    /// The ordering hazard `lease_lost`'s doc comment calls out: `notified.enable()` must register
    /// before re-checking `lost()`, or a loss signalled in the gap between the two would be missed
    /// forever by a caller that starts waiting only after it already happened.
    #[tokio::test]
    async fn lease_lost_resolves_even_when_awaited_after_the_loss() {
        let guard = HeartbeatGuard::spawn(Duration::from_millis(10), || async { Ok(0) });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(guard.lost(), "the lease must already be lost by now");

        tokio::time::timeout(Duration::from_millis(500), guard.lease_lost())
            .await
            .expect("lease_lost() awaited after the loss already happened must still resolve");
    }

    #[tokio::test]
    async fn a_transient_error_followed_by_success_does_not_mark_the_lease_lost() {
        let call_n = Arc::new(AtomicU32::new(0));
        let call_n_task = call_n.clone();
        let guard = HeartbeatGuard::spawn(Duration::from_millis(10), move || {
            let n = call_n_task.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    anyhow::bail!("transient")
                } else {
                    Ok(1)
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            !guard.lost(),
            "an error streak that recovers before 3 consecutive failures must not mark the lease lost"
        );
    }
}
