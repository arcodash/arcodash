//! Integration test: `job_service::org_job_counts` — the org-scoped/global job-status aggregation,
//! which reads per-key with `GET`s (a multi-key `MGET` is rejected cross-slot on a clustered
//! Redis). Confirms the per-key read buckets correctly by org and status; it can't reproduce
//! CROSSSLOT itself, since the local test Redis isn't clustered.

mod common;
use common::spawn_app;

use arcodash_backend::services::job_service::org_job_counts;
use arcodash_core::queue::{self, JobStatus};

/// Two i64 org ids derived from the current time, distinct from any other test's fixed org ids
/// (e.g. the seeded default org, or `999_999`) and from each other — so a `Some(org_id)` count
/// can be asserted exactly without a leftover key from another test file landing in the bucket.
fn unique_org_ids() -> (i64, i64) {
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    (base, base + 1)
}

#[tokio::test]
async fn org_job_counts_buckets_by_status_and_scopes_by_org() {
    let app = skip_if_none!(spawn_app().await);

    let (org_a, org_b) = unique_org_ids();

    // Snapshot the global count before writing fixtures — the shared test Redis carries other
    // tests' `job:*` keys with their own (unrelated) TTLs, so the "all orgs" assertion below
    // checks the delta this test caused, not an absolute count.
    let before_all = org_job_counts(&app.redis, None).await.unwrap();

    // org_a: one of each of three statuses.
    let job_a_pending = format!("counts-test-{}", uuid::Uuid::new_v4());
    queue::set_job_status(
        &app.redis,
        &job_a_pending,
        org_a,
        None,
        JobStatus::Pending,
        None,
        None,
    )
    .await
    .unwrap();

    let job_a_started = format!("counts-test-{}", uuid::Uuid::new_v4());
    queue::set_job_status(
        &app.redis,
        &job_a_started,
        org_a,
        None,
        JobStatus::Started,
        None,
        None,
    )
    .await
    .unwrap();

    let job_a_success = format!("counts-test-{}", uuid::Uuid::new_v4());
    queue::set_job_status(
        &app.redis,
        &job_a_success,
        org_a,
        None,
        JobStatus::Success,
        Some(7),
        None,
    )
    .await
    .unwrap();

    // org_b: a failure and a cancelled job — must never show up in org_a's counts.
    let job_b_failure = format!("counts-test-{}", uuid::Uuid::new_v4());
    queue::set_job_status(
        &app.redis,
        &job_b_failure,
        org_b,
        None,
        JobStatus::Failure,
        None,
        Some("boom".to_string()),
    )
    .await
    .unwrap();

    let job_b_cancelled = format!("counts-test-{}", uuid::Uuid::new_v4());
    queue::set_job_status(
        &app.redis,
        &job_b_cancelled,
        org_b,
        None,
        JobStatus::Cancelled,
        None,
        None,
    )
    .await
    .unwrap();

    // Scoped to org_a: only its own pending/started/success, nothing from org_b.
    let counts_a = org_job_counts(&app.redis, Some(org_a)).await.unwrap();
    assert_eq!(counts_a.queued, 1, "org_a: one pending job");
    assert_eq!(counts_a.in_flight, 1, "org_a: one started job");
    assert_eq!(counts_a.recent_success, 1, "org_a: one successful job");
    assert_eq!(
        counts_a.recent_failure, 0,
        "org_a must not see org_b's failure"
    );
    assert_eq!(
        counts_a.recent_cancelled, 0,
        "org_a must not see org_b's cancellation"
    );

    // Scoped to org_b: only its own failure/cancelled, nothing from org_a.
    let counts_b = org_job_counts(&app.redis, Some(org_b)).await.unwrap();
    assert_eq!(counts_b.queued, 0);
    assert_eq!(counts_b.in_flight, 0);
    assert_eq!(counts_b.recent_success, 0);
    assert_eq!(counts_b.recent_failure, 1, "org_b: one failed job");
    assert_eq!(counts_b.recent_cancelled, 1, "org_b: one cancelled job");

    // Unscoped: counts every org's jobs, so the delta over the pre-fixture snapshot must equal
    // the five records just written, split the same way across all five buckets.
    let after_all = org_job_counts(&app.redis, None).await.unwrap();
    assert_eq!(after_all.queued - before_all.queued, 1);
    assert_eq!(after_all.in_flight - before_all.in_flight, 1);
    assert_eq!(after_all.recent_success - before_all.recent_success, 1);
    assert_eq!(after_all.recent_failure - before_all.recent_failure, 1);
    assert_eq!(after_all.recent_cancelled - before_all.recent_cancelled, 1);
}
