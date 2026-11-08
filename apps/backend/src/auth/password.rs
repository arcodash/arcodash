//! Argon2id password hashing/verification for `users.password_hash`.

use anyhow::{anyhow, Result};
use argon2::password_hash::PasswordVerifier;
use argon2::{Argon2, PasswordHash, PasswordHasher};

/// Hash `plain` with Argon2id, returning the full PHC-formatted hash string.
///
/// Algorithm, params, salt, and hash are all encoded together, suitable for storing directly in
/// `users.password_hash` and later verifying with [`verify_password`].
///
/// Runs on a blocking-pool thread — Argon2id is deliberately slow (that's what makes it a good
/// password hash), and every call site here is reachable by an unauthenticated request
/// (login, invite-accept, forgot/reset password), so running it inline would let a modest
/// concurrent burst stall the whole async runtime.
pub async fn hash_password(plain: &str) -> Result<String> {
    let plain = plain.to_owned();
    tokio::task::spawn_blocking(move || {
        let argon2 = Argon2::default(); // Argon2id with sane default params.
        let hash = argon2
            .hash_password(plain.as_bytes())
            .map_err(|e| anyhow!("failed to hash password: {e}"))?;
        Ok(hash.to_string())
    })
    .await
    .map_err(|e| anyhow!("password hashing task panicked: {e}"))?
}

/// Verify `plain` against a PHC-formatted Argon2 `hash` (as produced by [`hash_password`]).
///
/// Returns `Ok(false)` for a non-matching password, `Err` only if `hash` itself is malformed.
/// Runs on a blocking-pool thread for the same reason as [`hash_password`].
pub async fn verify_password(plain: &str, hash: &str) -> Result<bool> {
    let plain = plain.to_owned();
    let hash = hash.to_owned();
    tokio::task::spawn_blocking(move || {
        let parsed_hash = PasswordHash::new(&hash)
            .map_err(|e| anyhow!("stored password hash is invalid: {e}"))?;
        Ok(Argon2::default()
            .verify_password(plain.as_bytes(), &parsed_hash)
            .is_ok())
    })
    .await
    .map_err(|e| anyhow!("password verification task panicked: {e}"))?
}

/// Argon2 verification is what makes a failed login take as long as a successful one; a request
/// for an account that doesn't exist (or has no password) is run against this fixed hash so
/// response latency doesn't disclose which case it was. The plaintext that produced it was never
/// recorded anywhere and is irrelevant — only the hash's cost parameters matter, and
/// `Argon2::default()` guarantees they match what a real stored hash uses.
const DUMMY_PASSWORD_HASH: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$O19IN3RiGRZFvsyOe2ce8g$C68sTfGpJMJDn01ioWOlHIEfPttjtDVit2407zLH21U";

/// Runs a real Argon2 verification against a fixed hash to equalize login timing.
///
/// A login attempt against an unknown email (or an account with no password set) costs the same
/// wall time as one that reaches [`verify_password`] for real.
///
/// The result is always discarded — this exists only for its timing, never its answer.
pub async fn verify_dummy_password(plain: &str) {
    let _ = verify_password(plain, DUMMY_PASSWORD_HASH).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hash_then_verify_round_trips() {
        let hash = hash_password("correct horse battery staple").await.unwrap();
        assert!(verify_password("correct horse battery staple", &hash)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn verify_rejects_wrong_password() {
        let hash = hash_password("correct horse battery staple").await.unwrap();
        assert!(!verify_password("wrong password", &hash).await.unwrap());
    }

    #[tokio::test]
    async fn different_calls_produce_different_hashes_due_to_random_salt() {
        let h1 = hash_password("same password").await.unwrap();
        let h2 = hash_password("same password").await.unwrap();
        assert_ne!(h1, h2);
    }

    /// PHC string produced by the reference C implementation (Python argon2-cffi), m=19456 t=2 p=1,
    /// so this pins cross-implementation compatibility.
    #[tokio::test]
    async fn verifies_a_hash_from_the_reference_implementation() {
        let hash = "$argon2id$v=19$m=19456,t=2,p=1$waC7prabqLhI5yDW+kbURg$IzcKGPLPJjTaMeD6oyc9tFvVINUBCsK61IvbmCjIVfw";
        assert!(verify_password("correct horse battery staple", hash)
            .await
            .unwrap());
        assert!(!verify_password("wrong", hash).await.unwrap());
    }

    #[tokio::test]
    async fn verify_errors_on_malformed_hash() {
        assert!(verify_password("anything", "not-a-valid-phc-hash")
            .await
            .is_err());
    }

    /// Guards against a typo'd `DUMMY_PASSWORD_HASH` literal — a malformed PHC string would turn
    /// every miss into a silent `Err` (swallowed by `verify_dummy_password`'s `let _ =`) instead
    /// of the `Ok(false)` a real stored hash produces, which would still run Argon2 but for less
    /// time on the parse-failure path, reopening the timing gap this exists to close.
    #[tokio::test]
    async fn dummy_password_hash_is_well_formed_and_never_matches() {
        assert!(!verify_password("anything", DUMMY_PASSWORD_HASH)
            .await
            .unwrap());
    }

    /// Proves the point of `spawn_blocking`: with only 2 async worker threads, 8 concurrent
    /// Argon2 hashes must not starve a latency-sensitive async task also running on those same
    /// 2 threads. Measures wall-clock time rather than a `tokio::time::timeout` around the sleep:
    /// with only 2 worker threads, `Timeout::poll` polls the inner sleep before the deadline, so
    /// once the hashing tasks finish, the sleep and the timeout become due on the same driver
    /// advance and `sleep.await.is_ok()` stays `true` regardless of how long the sleep's task was
    /// actually starved — the timeout wrapper can't fail for the reason this test claims.
    /// `Instant::now()` doesn't depend on the driver's scheduling and has no such blind spot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_hashing_does_not_starve_the_async_runtime() {
        let hash_tasks: Vec<_> = (0..8)
            .map(|_| tokio::spawn(hash_password("some password")))
            .collect();

        let started = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let elapsed = started.elapsed();
        // If the hashing ran inline instead of on the blocking pool, it would occupy both
        // workers: 8 Argon2 hashes over 2 threads is ~4 serialized rounds of ~100ms, so the
        // sleep's task would go unpolled for ~400ms — well past this floor.
        assert!(
            elapsed < std::time::Duration::from_millis(250),
            "an unrelated async sleep must complete promptly even while 8 Argon2 hashes are in \
             flight, took {elapsed:?}"
        );

        for task in hash_tasks {
            assert!(task.await.unwrap().is_ok());
        }
    }
}
