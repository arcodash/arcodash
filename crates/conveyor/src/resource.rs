//! The generic `resource_key`/`resource_cap` concurrency gate — no domain join; the caller
//! supplies the cap. `resource_key = None` means no gate (always enqueue). The cap is soft:
//! two leaders counting independently can transiently overshoot by one (accepted) — over-cap
//! tasks stay `pending` and are picked up by `requeue_stale_pending` once a slot frees.

use sea_orm::{ConnectionTrait, DbBackend, Statement};

use crate::config::default_resource_cap;

/// `true` iff enqueuing another task under `resource_key` would exceed `resource_cap` (or
/// [`default_resource_cap`] when unset) — the caller should leave the task `pending` rather than
/// push it onto the queue. `exclude_task_id` is the row being considered for enqueue, so it never
/// counts against its own admission.
pub async fn over_cap(
    db: &impl ConnectionTrait,
    resource_key: Option<&str>,
    resource_cap: Option<i64>,
    exclude_task_id: i64,
) -> anyhow::Result<bool> {
    let Some(resource_key) = resource_key else {
        return Ok(false);
    };
    let cap = resource_cap.unwrap_or_else(default_resource_cap);
    Ok(count_running(db, resource_key, exclude_task_id).await? >= cap)
}

/// Counts, toward a resource's cap, every other `running` task plus every other `pending` task
/// that is actually enqueued (`enqueued_at IS NOT NULL`) — a `pending` task still parked over the
/// cap (never enqueued) does NOT count, or it would self-deadlock the cap once saturated.
/// `exclude_task_id` is the row currently being admitted: a row never counts
/// against the cap it is being admitted through, or an already-enqueued `pending` task
/// re-checking its own cap (e.g. the sweeper's retry) would always see itself and refuse itself
/// forever. Counting enqueued-but-unclaimed `pending` tasks as well as `running` ones keeps a
/// burst of simultaneously-enqueued tasks from overshooting the cap, at the cost of slightly
/// lower burst throughput at saturation, by design.
async fn count_running(
    db: &impl ConnectionTrait,
    resource_key: &str,
    exclude_task_id: i64,
) -> anyhow::Result<i64> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT count(*) AS c FROM conveyor_tasks WHERE resource_key = $1 AND id <> $2 \
             AND (status = 'running' OR (status = 'pending' AND enqueued_at IS NOT NULL))",
            [resource_key.into(), exclude_task_id.into()],
        ))
        .await?;
    Ok(match row {
        Some(r) => r.try_get("", "c")?,
        None => 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    /// No real row ever has this id — stands in for "some other candidate row" in tests that
    /// don't care about self-exclusion.
    const UNRELATED_ID: i64 = -1;

    #[tokio::test]
    async fn no_resource_key_never_gates() {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        assert!(!over_cap(&db, None, Some(0), UNRELATED_ID).await.unwrap());
    }

    #[tokio::test]
    async fn gate_trips_at_cap_and_releases_below_it() {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let key = format!("r-{}", uuid::Uuid::new_v4());
        assert!(!over_cap(&db, Some(&key), Some(1), UNRELATED_ID)
            .await
            .unwrap());

        insert_task(&db, "toy", None, Some(&key), Some(1), "running").await;
        assert!(over_cap(&db, Some(&key), Some(1), UNRELATED_ID)
            .await
            .unwrap());

        assert!(!over_cap(&db, Some(&key), Some(2), UNRELATED_ID)
            .await
            .unwrap());
    }

    /// A `pending` task that was actually enqueued (has
    /// `enqueued_at` set, mirrors `Conveyor::push_pending`) must count toward the cap even
    /// though it hasn't started `running` yet — a burst of these could otherwise overshoot the
    /// cap by however many land `pending` before any of them is claimed. A `pending` task that
    /// was never enqueued at all (still parked over a prior cap check) must NOT count.
    #[tokio::test]
    async fn a_pending_but_enqueued_task_counts_toward_the_cap_an_unenqueued_one_does_not() {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let key = format!("r-enqueued-{}", uuid::Uuid::new_v4());
        let id = insert_task(&db, "toy", None, Some(&key), Some(1), "pending").await;
        assert!(
            !over_cap(&db, Some(&key), Some(1), UNRELATED_ID)
                .await
                .unwrap(),
            "a pending task that was never enqueued (still parked) must not count toward the cap"
        );

        db.execute_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DbBackend::Postgres,
            "UPDATE conveyor_tasks SET enqueued_at = now() WHERE id = $1",
            [id.into()],
        ))
        .await
        .unwrap();
        assert!(
            over_cap(&db, Some(&key), Some(1), UNRELATED_ID)
                .await
                .unwrap(),
            "a pending task that was actually enqueued must count toward the cap, not just a \
             running one"
        );
    }

    /// The row currently being considered for enqueue never counts against its own cap check —
    /// excluding an unrelated id leaves the same task still saturating the cap.
    #[tokio::test]
    async fn a_task_excludes_itself_from_its_own_cap_check() {
        let Some((db, _redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let key = format!("r-selfexclude-{}", uuid::Uuid::new_v4());
        let id = insert_task(&db, "toy", None, Some(&key), Some(1), "pending").await;
        db.execute_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DbBackend::Postgres,
            "UPDATE conveyor_tasks SET enqueued_at = now() WHERE id = $1",
            [id.into()],
        ))
        .await
        .unwrap();

        assert!(
            !over_cap(&db, Some(&key), Some(1), id).await.unwrap(),
            "a row must never count against the cap it is itself being admitted through"
        );
        assert!(
            over_cap(&db, Some(&key), Some(1), UNRELATED_ID)
                .await
                .unwrap(),
            "the same row must still saturate the cap for any other candidate"
        );
    }
}
