//! Lease/heartbeat/claim/finalize for the durable delivery tables (`notification_deliveries`,
//! `mail_deliveries`), parameterized by a `&'static str` table-name constant.
//!
//! Kept separate from `run_lifecycle.rs`, which is tied to `runs`' query/monitor-specific columns.

use std::time::Duration;

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};

use crate::run_lifecycle;

pub const NOTIFICATION_DELIVERIES: &str = "notification_deliveries";
pub const MAIL_DELIVERIES: &str = "mail_deliveries";

/// CAS claim: `pending`/`retrying` -> `running`, or reclaim a `running` row whose lease expired
/// (crashed worker). `Ok(false)` means 0 rows matched (missing, or held by a live lease/terminal);
/// tell them apart with [`row_exists`].
///
/// `table` is always [`NOTIFICATION_DELIVERIES`] or [`MAIL_DELIVERIES`] (never external input), so
/// interpolating it is safe (table names can't be bind params).
pub async fn claim(db: &impl ConnectionTrait, table: &str, id: i64) -> anyhow::Result<bool> {
    let sql = format!(
        "UPDATE {table} SET status = 'running', worker_id = $1, started_at = now(), \
         lease_expires_at = now() + ($2 || ' seconds')::interval \
         WHERE id = $3 \
           AND (status IN ('pending', 'retrying') \
                OR (status = 'running' AND lease_expires_at < now()))"
    );
    let res = db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            &sql,
            [
                run_lifecycle::worker_id().into(),
                run_lifecycle::run_lease_secs().to_string().into(),
                id.into(),
            ],
        ))
        .await?;
    Ok(res.rows_affected() == 1)
}

pub async fn row_exists(db: &impl ConnectionTrait, table: &str, id: i64) -> anyhow::Result<bool> {
    let sql = format!("SELECT 1 AS one FROM {table} WHERE id = $1");
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            &sql,
            [id.into()],
        ))
        .await?;
    Ok(row.is_some())
}

/// Heartbeat: extends `lease_expires_at` every `run_lease_secs()/3` while `status = 'running'`.
/// Dropping the guard stops renewal (see [`arcodash_conveyor::HeartbeatGuard`]).
pub fn spawn_heartbeat(
    db: DatabaseConnection,
    table: &'static str,
    id: i64,
) -> arcodash_conveyor::HeartbeatGuard {
    let period = Duration::from_secs((run_lifecycle::run_lease_secs() / 3).max(1).cast_unsigned());
    arcodash_conveyor::HeartbeatGuard::spawn(period, move || {
        let db = db.clone();
        async move {
            let sql = format!(
                "UPDATE {table} SET lease_expires_at = now() + ($1 || ' seconds')::interval \
                 WHERE id = $2 AND status = 'running'"
            );
            let res = db
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    &sql,
                    [
                        run_lifecycle::run_lease_secs().to_string().into(),
                        id.into(),
                    ],
                ))
                .await?;
            Ok(res.rows_affected())
        }
    })
}

/// Finalizes a delivery (terminal, or the intermediate `'retrying'` state) and clears the lease.
/// `attempt` is written to `attempt_number` when `Some`; pass `None` for `mail_deliveries`, which
/// has no such column. `terminal = false` leaves `finished_at` unset.
///
/// Fenced on `worker_id = <this process>() AND status = 'running'`: deliveries have no cross-worker
/// handoff, so if this worker's lease expired and another reclaimed the row, the stale finalize
/// must lose rather than overwrite the reclaimer's outcome. A fence miss is a silent no-op.
pub async fn finalize(
    db: &impl ConnectionTrait,
    table: &str,
    id: i64,
    status: &str,
    error: Option<String>,
    attempt: Option<i32>,
    terminal: bool,
) -> anyhow::Result<()> {
    let set_attempt = if attempt.is_some() {
        ", attempt_number = $6"
    } else {
        ""
    };
    let sql = format!(
        "UPDATE {table} SET status = $1, error = $2{set_attempt}, \
         finished_at = CASE WHEN $3 THEN now() ELSE finished_at END, \
         lease_expires_at = NULL, worker_id = NULL \
         WHERE id = $4 AND worker_id = $5 AND status = 'running'"
    );
    let mut values: Vec<sea_orm::Value> = vec![
        status.into(),
        error
            .as_deref()
            .map(arcodash_core::scrub::scrub_error_values)
            .into(),
        terminal.into(),
        id.into(),
        run_lifecycle::worker_id().into(),
    ];
    values.extend(attempt.map(Into::into));
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        &sql,
        values,
    ))
    .await?;
    Ok(())
}

/// Terminalizes a `'retrying'` row whose scheduled re-attempt could never be enqueued (e.g. queue
/// down). `finalize`'s fence never matches an unleased `'retrying'` row, so this is fenced on
/// `status = 'retrying' AND finished_at IS NULL` instead. Returns whether it won the race.
pub async fn fail_retrying(
    db: &impl ConnectionTrait,
    table: &str,
    id: i64,
    error: String,
    attempt: i32,
) -> anyhow::Result<bool> {
    let sql = format!(
        "UPDATE {table} SET status = 'failed', error = $1, attempt_number = $2, \
         finished_at = now(), lease_expires_at = NULL, worker_id = NULL \
         WHERE id = $3 AND status = 'retrying' AND finished_at IS NULL"
    );
    let res = db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            &sql,
            [
                arcodash_core::scrub::scrub_error_values(&error).into(),
                attempt.into(),
                id.into(),
            ],
        ))
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Reclaims a `running` row whose lease expired with no heartbeat back to `pending`, so the
/// stale-pending sweep sees it when no later message re-names this delivery id for [`claim`]'s
/// inline reclaim. Mirrors `scheduler::sweeper::sweep_expired_leases` for `runs`.
pub async fn reclaim_expired_leases(db: &impl ConnectionTrait, table: &str) -> anyhow::Result<u64> {
    let sql = format!(
        "UPDATE {table} SET status = 'pending', worker_id = NULL, lease_expires_at = NULL \
         WHERE status = 'running' AND lease_expires_at < now()"
    );
    let res = db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            &sql,
            [],
        ))
        .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    //! Real Postgres only (skips without `TEST_DATABASE_URL`); raw SQL state transitions, no Redis.
    use super::*;
    use crate::scheduler::test_support::{exec_sql, query_row};
    use arcodash_core::entities::{mail_delivery, notification_delivery};
    use sea_orm::EntityTrait;

    async fn setup() -> Option<DatabaseConnection> {
        let database_url = std::env::var("TEST_DATABASE_URL").ok()?;
        Some(arcodash_core::db::connect(&database_url).await.unwrap())
    }

    async fn backdate_lease(db: &DatabaseConnection, table: &str, id: i64) {
        let sql = format!(
            "UPDATE {table} SET lease_expires_at = now() - interval '1 second' WHERE id = $1"
        );
        exec_sql(db, &sql, [id.into()]).await;
    }

    async fn column<T: sea_orm::TryGetable>(
        db: &DatabaseConnection,
        table: &str,
        id: i64,
        column: &str,
    ) -> T {
        let sql = format!("SELECT {column} FROM {table} WHERE id = $1");
        query_row(db, &sql, [id.into()])
            .await
            .try_get("", column)
            .unwrap()
    }

    async fn status_of(db: &DatabaseConnection, table: &str, id: i64) -> String {
        column(db, table, id, "status").await
    }

    async fn notification_row(db: &DatabaseConnection, id: i64) -> notification_delivery::Model {
        notification_delivery::Entity::find_by_id(id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    /// Seeds a notification row and moves it through claim -> non-terminal `'retrying'` finalize.
    async fn seed_retrying_notification_row(db: &DatabaseConnection) -> i64 {
        let id = seed_notification_row(db).await;
        assert!(claim(db, NOTIFICATION_DELIVERIES, id).await.unwrap());
        finalize(
            db,
            NOTIFICATION_DELIVERIES,
            id,
            "retrying",
            None,
            Some(1),
            false,
        )
        .await
        .unwrap();
        id
    }

    /// Seeds real rows: the INSERT goes through real FKs (`org_id`, `monitor_id`), so fabricated
    /// ids fail depending on the shared test DB.
    async fn seed_notification_row(db: &DatabaseConnection) -> i64 {
        let suffix = uuid::Uuid::new_v4();
        let org =
            crate::scheduler::test_support::seed_org(db, &format!("org-delivery-lc-{suffix}"))
                .await;
        let ds = crate::scheduler::test_support::seed_data_source(db, org.id).await;
        let q = crate::scheduler::test_support::seed_query(db, org.id, ds.id).await;
        let mon = crate::scheduler::test_support::seed_monitor(
            db,
            org.id,
            q.id,
            q.user_id,
            serde_json::json!({}),
        )
        .await;
        notification_delivery::insert_pending(db, org.id, mon.id, None, -1, "triggered", None)
            .await
            .unwrap()
    }

    /// Same FK caveat: `mail_deliveries.org_id` references `organizations`.
    async fn seed_org_id(db: &DatabaseConnection) -> i64 {
        let suffix = uuid::Uuid::new_v4();
        crate::scheduler::test_support::seed_org(db, &format!("org-delivery-lc-mail-{suffix}"))
            .await
            .id
    }

    #[tokio::test]
    async fn claim_then_stale_lease_is_reclaimable() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let id = seed_notification_row(&db).await;

        assert!(claim(&db, NOTIFICATION_DELIVERIES, id).await.unwrap());
        assert_eq!(status_of(&db, NOTIFICATION_DELIVERIES, id).await, "running");

        backdate_lease(&db, NOTIFICATION_DELIVERIES, id).await;

        assert!(
            claim(&db, NOTIFICATION_DELIVERIES, id).await.unwrap(),
            "a crashed worker's expired lease must be reclaimable by a fresh claim"
        );
    }

    #[tokio::test]
    async fn claim_twice_with_live_lease_is_rejected() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let id = seed_notification_row(&db).await;

        assert!(claim(&db, NOTIFICATION_DELIVERIES, id).await.unwrap());
        assert!(
            !claim(&db, NOTIFICATION_DELIVERIES, id).await.unwrap(),
            "a second claim while the lease is still live must be rejected"
        );
    }

    #[tokio::test]
    async fn finalize_clears_the_lease_and_sets_finished_at_when_terminal() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let id = seed_notification_row(&db).await;
        assert!(claim(&db, NOTIFICATION_DELIVERIES, id).await.unwrap());

        finalize(
            &db,
            NOTIFICATION_DELIVERIES,
            id,
            "sent",
            None,
            Some(1),
            true,
        )
        .await
        .unwrap();

        let row = notification_row(&db, id).await;
        assert_eq!(row.status, "sent");
        assert!(row.lease_expires_at.is_none());
        assert!(row.worker_id.is_none());
        assert!(row.finished_at.is_some());
    }

    #[tokio::test]
    async fn delivery_failure_error_is_scrubbed() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let raw = "Key (email)=(bob@x.com) already exists.";
        let id = seed_notification_row(&db).await;
        assert!(claim(&db, NOTIFICATION_DELIVERIES, id).await.unwrap());
        finalize(
            &db,
            NOTIFICATION_DELIVERIES,
            id,
            "failed",
            Some(raw.to_string()),
            Some(1),
            true,
        )
        .await
        .unwrap();
        let row = notification_row(&db, id).await;
        assert_eq!(
            row.error.as_deref(),
            Some("Key (email)=(…) already exists.")
        );

        let retrying = seed_retrying_notification_row(&db).await;
        assert!(
            fail_retrying(&db, NOTIFICATION_DELIVERIES, retrying, raw.to_string(), 2)
                .await
                .unwrap()
        );
        let row = notification_row(&db, retrying).await;
        assert_eq!(
            row.error.as_deref(),
            Some("Key (email)=(…) already exists.")
        );
    }

    #[tokio::test]
    async fn finalize_leaves_finished_at_null_when_not_terminal() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let id = seed_notification_row(&db).await;
        assert!(claim(&db, NOTIFICATION_DELIVERIES, id).await.unwrap());

        finalize(
            &db,
            NOTIFICATION_DELIVERIES,
            id,
            "retrying",
            None,
            Some(1),
            false,
        )
        .await
        .unwrap();

        let row = notification_row(&db, id).await;
        assert_eq!(row.status, "retrying");
        assert!(row.lease_expires_at.is_none(), "lease still cleared");
        assert!(
            row.finished_at.is_none(),
            "a non-terminal finalize must leave finished_at NULL so the row stays recognizable \
             as not-yet-done"
        );
    }

    /// The table-name parameterization also works for `mail_deliveries` (no `attempt_number`,
    /// `attempt: None`).
    #[tokio::test]
    async fn claim_and_finalize_work_against_mail_deliveries_too() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let org_id = seed_org_id(&db).await;
        let id = mail_delivery::insert_pending_digest(&db, org_id, 1)
            .await
            .unwrap();

        assert!(claim(&db, MAIL_DELIVERIES, id).await.unwrap());
        assert_eq!(status_of(&db, MAIL_DELIVERIES, id).await, "running");
        assert!(!claim(&db, MAIL_DELIVERIES, id).await.unwrap());

        finalize(&db, MAIL_DELIVERIES, id, "sent", None, None, true)
            .await
            .unwrap();

        let row = mail_delivery::Entity::find_by_id(id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "sent");
        assert!(row.finished_at.is_some());
        assert!(row.lease_expires_at.is_none());
    }

    #[tokio::test]
    async fn reclaim_expired_leases_resets_a_stuck_running_row_back_to_pending() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let id = seed_notification_row(&db).await;
        assert!(claim(&db, NOTIFICATION_DELIVERIES, id).await.unwrap());
        backdate_lease(&db, NOTIFICATION_DELIVERIES, id).await;

        let n = reclaim_expired_leases(&db, NOTIFICATION_DELIVERIES)
            .await
            .unwrap();
        assert!(n >= 1, "must reclaim at least the row this test seeded");

        let row = notification_row(&db, id).await;
        assert_eq!(row.status, "pending");
        assert!(row.lease_expires_at.is_none());
        assert!(row.worker_id.is_none());
    }

    /// A finalize from a worker whose claim was reclaimed (`worker_id` mismatch) is a silent no-op.
    #[tokio::test]
    async fn finalize_from_a_stale_worker_id_is_a_no_op() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let id = seed_notification_row(&db).await;
        assert!(claim(&db, NOTIFICATION_DELIVERIES, id).await.unwrap());

        // Simulate a second worker's reclaim by overwriting worker_id.
        exec_sql(
            &db,
            "UPDATE notification_deliveries SET worker_id = 'a-different-worker' WHERE id = $1",
            [id.into()],
        )
        .await;

        finalize(
            &db,
            NOTIFICATION_DELIVERIES,
            id,
            "sent",
            None,
            Some(1),
            true,
        )
        .await
        .unwrap();

        let row = notification_row(&db, id).await;
        assert_eq!(
            row.status, "running",
            "a finalize from a worker that no longer owns the lease must not overwrite the row"
        );
        assert_eq!(
            row.worker_id.as_deref(),
            Some("a-different-worker"),
            "the reclaiming worker's ownership must be left untouched"
        );
    }

    /// `finalize`'s fence no-ops on an unleased `'retrying'` row; `fail_retrying` then succeeds on
    /// it.
    #[tokio::test]
    async fn a_retrying_delivery_can_be_failed_without_a_worker_lease() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let id = seed_retrying_notification_row(&db).await;
        assert_eq!(
            status_of(&db, NOTIFICATION_DELIVERIES, id).await,
            "retrying"
        );

        // `finalize` on an unleased retrying row is a silent no-op.
        finalize(
            &db,
            NOTIFICATION_DELIVERIES,
            id,
            "failed",
            Some("failed to schedule retry".to_string()),
            Some(2),
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            status_of(&db, NOTIFICATION_DELIVERIES, id).await,
            "retrying",
            "finalize's worker_id fence must silently no-op on an unleased retrying row"
        );

        assert!(fail_retrying(
            &db,
            NOTIFICATION_DELIVERIES,
            id,
            "failed to schedule retry".to_string(),
            2,
        )
        .await
        .unwrap());

        let row = notification_row(&db, id).await;
        assert_eq!(row.status, "failed");
        assert!(row.finished_at.is_some());
        assert!(row.lease_expires_at.is_none());
        assert_eq!(row.error.as_deref(), Some("failed to schedule retry"));
        assert_eq!(row.attempt_number, 2);
    }

    #[tokio::test]
    async fn fail_retrying_does_not_touch_a_row_another_attempt_already_reclaimed() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let id = seed_retrying_notification_row(&db).await;
        // A fresh claim, as the next attempt would perform, moves the row back to 'running'.
        assert!(claim(&db, NOTIFICATION_DELIVERIES, id).await.unwrap());

        let won = fail_retrying(
            &db,
            NOTIFICATION_DELIVERIES,
            id,
            "failed to schedule retry".to_string(),
            2,
        )
        .await
        .unwrap();
        assert!(
            !won,
            "a row already reclaimed by the next attempt must not be touched"
        );
        assert_eq!(status_of(&db, NOTIFICATION_DELIVERIES, id).await, "running");
    }

    #[tokio::test]
    async fn fail_retrying_is_idempotent() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let id = seed_retrying_notification_row(&db).await;

        assert!(
            fail_retrying(&db, NOTIFICATION_DELIVERIES, id, "first".to_string(), 2)
                .await
                .unwrap()
        );
        let first = notification_row(&db, id).await;

        let second = fail_retrying(&db, NOTIFICATION_DELIVERIES, id, "second".to_string(), 3)
            .await
            .unwrap();
        assert!(
            !second,
            "a second call must not rewrite an already-failed row"
        );

        let row = notification_row(&db, id).await;
        assert_eq!(row.finished_at, first.finished_at);
        assert_eq!(
            row.error.as_deref(),
            Some("first"),
            "the second call's error must not overwrite the first"
        );
        assert_eq!(row.attempt_number, 2);
    }

    /// A plain `drop(heartbeat)` (what a handler panic's unwind does) must stop the renewal for
    /// good.
    #[tokio::test]
    async fn heartbeat_guard_drop_stops_the_renewal_without_an_explicit_abort() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let id = seed_notification_row(&db).await;
        assert!(claim(&db, NOTIFICATION_DELIVERIES, id).await.unwrap());

        // Shrinks the heartbeat period from the 90s default; process-global env var, so not
        // parallel-safe.
        std::env::set_var("RUN_LEASE_SECS", "3");
        let heartbeat = spawn_heartbeat(db.clone(), NOTIFICATION_DELIVERIES, id);

        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        drop(heartbeat);

        let lease_at_drop: Option<sea_orm::prelude::DateTimeWithTimeZone> =
            column(&db, NOTIFICATION_DELIVERIES, id, "lease_expires_at").await;
        tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
        let lease_well_after_drop: Option<sea_orm::prelude::DateTimeWithTimeZone> =
            column(&db, NOTIFICATION_DELIVERIES, id, "lease_expires_at").await;
        std::env::remove_var("RUN_LEASE_SECS");

        assert_eq!(
            lease_at_drop, lease_well_after_drop,
            "lease_expires_at must not advance after the guard is dropped — a still-advancing \
             lease means the heartbeat task kept running instead of being aborted"
        );
    }
}
