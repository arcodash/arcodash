//! The digest fan-out loop: enqueues one `send_digest` task per digest-enabled user who is in their
//! chosen local send hour (`user_is_due`). Shared by the conveyor-scheduled handler and the
//! in-process hourly cron; the Redis per-user-per-day guard (`claim_digest_send_for_user`) stops
//! the two entry points racing each other.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use arcodash_conveyor::{TaskContext, TaskHandler, TaskOutcome, TaskResult};
use arcodash_core::entities::user;
use arcodash_core::entitlements::WorkGate;

use crate::handlers::execute_query::ExecuteQueryContext;

pub struct DigestFanoutHandler {
    pub ctx: Arc<ExecuteQueryContext>,
}

#[async_trait]
impl TaskHandler for DigestFanoutHandler {
    async fn run(&self, _task: TaskContext, _payload: serde_json::Value) -> TaskResult {
        let summary = fan_out_digests(&self.ctx).await?;
        if summary.failed > 0 {
            // Any per-user enqueue failure fails the tick; the failure streak, not one failure, is
            // what auto-pauses.
            return Err(anyhow::anyhow!(
                "{} of {} due digests failed to enqueue",
                summary.failed,
                summary.due
            ));
        }
        Ok(TaskOutcome::Success { result_ref: None })
    }
}

/// Outcome of one fan-out pass; decides whether the tick succeeded.
#[derive(Default)]
pub struct FanOutSummary {
    pub due: usize,
    pub enqueued: usize,
    pub failed: usize,
}

/// Users are paged in fixed-size keyset batches (on `id`, not `OFFSET`) rather than loaded at once.
const USER_PAGE_SIZE: u64 = 500;

pub async fn fan_out_digests(ctx: &ExecuteQueryContext) -> anyhow::Result<FanOutSummary> {
    let now = chrono::Utc::now();
    // Memoized per org for this fan-out so N due users in one org cost one gate check.
    let mut gate_cache: HashMap<i64, WorkGate> = HashMap::new();
    let mut summary = FanOutSummary::default();

    let mut after_id: i64 = 0;
    loop {
        let page = user::Entity::find()
            .filter(user::Column::DigestEnabled.eq(true))
            // An offboarded or never-accepted account keeps its digest flag; the flag is not
            // consent.
            .filter(user::Column::DisabledAt.is_null())
            .filter(user::Column::IsInvitationPending.eq(false))
            .filter(user::Column::Id.gt(after_id))
            .order_by_asc(user::Column::Id)
            .limit(USER_PAGE_SIZE)
            .all(&ctx.db)
            .await
            .inspect_err(|e| tracing::error!(error = %e, "digest_fanout: failed to load a page of digest-enabled users"))?;
        let Some(last) = page.last() else {
            break;
        };
        after_id = last.id;
        let page_len = page.len() as u64;

        for u in page {
            if !arcodash_core::digest_schedule::user_is_due(
                u.digest_hour,
                u.digest_timezone.as_deref(),
                now,
            ) {
                continue;
            }
            summary.due += 1;
            // Checked before claiming the per-day guard, so a blocked org never burns it. Memoized
            // per org (a plain get-or-insert, since `HashMap::entry` can't hold an `await`).
            let gate = if let Some(g) = gate_cache.get(&u.org_id) {
                *g
            } else {
                let g = ctx.entitlements.scheduled_work(&ctx.db, u.org_id).await;
                gate_cache.insert(u.org_id, g);
                g
            };
            if let WorkGate::Blocked { reason } = gate {
                tracing::info!(org_id = u.org_id, reason, "skipping scheduled work");
                continue;
            }
            let local_date =
                arcodash_core::digest_schedule::user_local_date(u.digest_timezone.as_deref(), now);
            if !crate::scheduler::claim_digest_send_for_user(&ctx.redis, u.id, local_date).await {
                tracing::debug!(
                    user_id = u.id,
                    "digest_fanout: already sent today, skipping"
                );
                continue;
            }
            if let Err(e) = arcodash_core::mail_notify::enqueue_send_digest(
                &ctx.db,
                ctx.queue.as_ref(),
                ctx.conveyor.get().map(std::convert::AsRef::as_ref),
                u.org_id,
                u.id,
            )
            .await
            {
                // The guard means "this digest went out"; an enqueue failure must release it.
                crate::scheduler::release_digest_send_guard(&ctx.redis, u.id, local_date).await;
                tracing::warn!(user_id = u.id, error = %e, "digest_fanout: failed to enqueue send_digest");
                summary.failed += 1;
            } else {
                summary.enqueued += 1;
            }
        }

        if page_len < USER_PAGE_SIZE {
            break;
        }
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    //! Real Postgres + Redis only (skips without `TEST_DATABASE_URL`/`TEST_REDIS_URL`).
    use std::sync::Arc;

    use arcodash_conveyor::schedule_spec::ScheduleSpec;
    use arcodash_conveyor::{OverlapPolicy, ScheduleRequest};
    use arcodash_core::entities::mail_delivery;
    use sea_orm::sea_query::IntoCondition;
    use sea_orm::ActiveModelTrait;
    use sea_orm::ActiveValue::Set;
    use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};

    use super::*;
    use crate::scheduler::test_support::{exec_sql, seed_org, test_ctx, BlockedByTest};

    /// A digest-enabled user with an unsubscribe token; `digest_hour` is pinned to the current UTC
    /// hour so the user is always due.
    async fn seed_user(db: &sea_orm::DatabaseConnection, org_id: i64) -> user::Model {
        use chrono::Timelike;
        user::ActiveModel {
            digest_enabled: Set(true),
            digest_unsubscribe_token: Set(Some(uuid::Uuid::new_v4().to_string())),
            digest_hour: Set(Some(chrono::Utc::now().hour() as i16)),
            digest_timezone: Set(Some("UTC".to_string())),
            ..crate::scheduler::test_support::new_user(org_id, vec![])
        }
        .insert(db)
        .await
        .unwrap()
    }

    async fn send_digest_count(
        db: &sea_orm::DatabaseConnection,
        user_ids: &[i64],
        scope: impl IntoCondition,
    ) -> usize {
        mail_delivery::Entity::find()
            .filter(mail_delivery::Column::TaskType.eq("send_digest"))
            .filter(mail_delivery::Column::EntityId.is_in(user_ids.iter().copied()))
            .filter(scope)
            .all(db)
            .await
            .unwrap()
            .len()
    }

    /// A conveyor running `DigestFanoutHandler` under `task_type`, with a schedule already due.
    struct RunningFanout {
        shutdown_tx: tokio::sync::watch::Sender<bool>,
        run_handle: tokio::task::JoinHandle<()>,
    }

    impl RunningFanout {
        async fn stop(self) {
            let _ = self.shutdown_tx.send(true);
            tokio::time::timeout(std::time::Duration::from_secs(10), self.run_handle)
                .await
                .unwrap()
                .unwrap();
        }
    }

    async fn start_due_fanout(
        db: &sea_orm::DatabaseConnection,
        redis: redis::aio::ConnectionManager,
        redis_url: String,
        task_type: &str,
    ) -> RunningFanout {
        let ctx = Arc::new(test_ctx(db.clone(), redis.clone()));
        let conveyor = arcodash_conveyor::ConveyorBuilder::new(db.clone(), redis, redis_url)
            .register(task_type, DigestFanoutHandler { ctx: ctx.clone() })
            .tier("aux", &[task_type], 2)
            .build();
        let _ = ctx.conveyor.set(conveyor.clone());

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let run_handle = {
            let conveyor = conveyor.clone();
            tokio::spawn(async move { conveyor.run(shutdown_rx).await })
        };

        let schedule_id = conveyor
            .schedule(ScheduleRequest {
                task_type: task_type.into(),
                entity_id: Some(1),
                payload: serde_json::json!({}),
                spec: ScheduleSpec::Interval { every_secs: 300 },
                timezone: "UTC".to_string(),
                overlap_policy: OverlapPolicy::Skip,
                tenant_id: None,
                resource_key: None,
                resource_cap: None,
            })
            .await
            .unwrap();

        exec_sql(
            db,
            "UPDATE conveyor_schedules SET next_run_at = now() - interval '5 seconds' WHERE id = $1",
            [schedule_id.0.into()],
        )
        .await;

        RunningFanout {
            shutdown_tx,
            run_handle,
        }
    }

    fn task_ctx() -> arcodash_conveyor::TaskContext {
        arcodash_conveyor::TaskContext {
            task_id: arcodash_conveyor::TaskId(0),
            task_type: "digest_fanout".into(),
            attempt: 1,
            tenant_id: None,
            entity_id: None,
        }
    }

    async fn setup() -> Option<(
        sea_orm::DatabaseConnection,
        redis::aio::ConnectionManager,
        String,
    )> {
        let database_url = std::env::var("TEST_DATABASE_URL").ok()?;
        let redis_url = std::env::var("TEST_REDIS_URL").ok()?;
        let db = arcodash_core::db::connect(&database_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();
        Some((db, redis, redis_url))
    }

    #[tokio::test]
    async fn digest_fanout_fires_through_conveyor_and_enqueues_a_send_digest_per_user() {
        let Some((db, redis, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL/TEST_REDIS_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-digest-fanout-{suffix}")).await;
        let u1 = seed_user(&db, org.id).await;
        let u2 = seed_user(&db, org.id).await;
        // The guard is keyed by a `BIGSERIAL` user id that a suite's `TRUNCATE ... RESTART
        // IDENTITY` can recycle, so clear it defensively.
        let today =
            arcodash_core::digest_schedule::user_local_date(Some("UTC"), chrono::Utc::now());
        crate::scheduler::release_digest_send_guard(&redis, u1.id, today).await;
        crate::scheduler::release_digest_send_guard(&redis, u2.id, today).await;

        let task_type = format!("digest_fanout_test_{}", uuid::Uuid::new_v4());
        let running = start_due_fanout(&db, redis, redis_url, &task_type).await;

        // Well past 5s: the deadline loop only claims a due schedule while holding the conveyor
        // leader lock (35s TTL, no release on shutdown), which another process on a shared Redis
        // may hold.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        loop {
            // Scoped by org_id: a recycled user id on a shared test DB could match an unrelated
            // test.
            let count = send_digest_count(
                &db,
                &[u1.id, u2.id],
                mail_delivery::Column::OrgId.eq(org.id),
            )
            .await;
            if count == 2 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "digest_fanout never enqueued a mail_deliveries row for both users within 90s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        running.stop().await;
    }

    /// A user whose chosen send hour isn't the current one gets no `send_digest` this tick.
    #[tokio::test]
    async fn digest_fanout_skips_a_user_whose_send_hour_has_not_arrived() {
        use chrono::Timelike;
        let Some((db, redis, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL/TEST_REDIS_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-digest-fanout-nothour-{suffix}")).await;
        let due_user = seed_user(&db, org.id).await;
        let not_due_user = seed_user(&db, org.id).await;
        let today =
            arcodash_core::digest_schedule::user_local_date(Some("UTC"), chrono::Utc::now());
        crate::scheduler::release_digest_send_guard(&redis, due_user.id, today).await;
        // `user_is_due` catches up on missed ticks (`>=`), so pin the hour strictly ahead of now;
        // skip when the current hour is the last of the UTC day.
        let now_hour = chrono::Utc::now().hour() as i16;
        if now_hour >= 23 {
            eprintln!(
                "skipping: too close to the UTC day rollover to construct a not-yet-due hour deterministically"
            );
            return;
        }
        let off_hour = now_hour + 1;
        let mut active: user::ActiveModel = not_due_user.clone().into();
        active.digest_hour = Set(Some(off_hour));
        active.update(&db).await.unwrap();

        let task_type = format!("digest_fanout_hour_test_{}", uuid::Uuid::new_v4());
        let running = start_due_fanout(&db, redis, redis_url, &task_type).await;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        loop {
            // Scoped by org_id: a recycled user id on a shared test DB could match an unrelated
            // test.
            let count =
                send_digest_count(&db, &[due_user.id], mail_delivery::Column::OrgId.eq(org.id))
                    .await;
            if count == 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "digest_fanout never enqueued the due user's send_digest within 90s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let not_due_count = send_digest_count(
            &db,
            &[not_due_user.id],
            mail_delivery::Column::OrgId.eq(org.id),
        )
        .await;
        assert_eq!(
            not_due_count, 0,
            "a user whose send hour has not arrived must not get a digest enqueued"
        );

        running.stop().await;
    }

    /// A prior claim of a user's per-day guard makes `fan_out_digests` skip that user.
    #[tokio::test]
    async fn digest_fanout_skips_when_todays_guard_already_claimed() {
        let Some((db, redis, redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL/TEST_REDIS_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-digest-fanout-guard-{suffix}")).await;
        let u1 = seed_user(&db, org.id).await;
        let u2 = seed_user(&db, org.id).await;

        // Claim both users' guards directly, standing in for an earlier send today.
        let today =
            arcodash_core::digest_schedule::user_local_date(Some("UTC"), chrono::Utc::now());
        let _ = crate::scheduler::claim_digest_send_for_user(&redis, u1.id, today).await;
        let _ = crate::scheduler::claim_digest_send_for_user(&redis, u2.id, today).await;

        let task_type = format!("digest_fanout_guard_test_{}", uuid::Uuid::new_v4());
        let running = start_due_fanout(&db, redis, redis_url, &task_type).await;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        loop {
            let ran = !db
                .query_all_raw(sea_orm::Statement::from_sql_and_values(
                    sea_orm::DbBackend::Postgres,
                    "SELECT 1 AS one FROM conveyor_tasks WHERE task_type = $1 AND status = 'success'",
                    [task_type.clone().into()],
                ))
                .await
                .unwrap()
                .is_empty();
            if ran {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "digest_fanout task never completed within 90s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let count = send_digest_count(
            &db,
            &[u1.id, u2.id],
            mail_delivery::Column::OrgId.eq(org.id),
        )
        .await;
        assert_eq!(
            count, 0,
            "digest_fanout must not enqueue sends when today's guard is already claimed"
        );

        running.stop().await;
    }

    /// A blocked org's due user must not get a `send_digest`; calls `fan_out_digests` directly
    /// since only the org gate is under test.
    #[tokio::test]
    async fn digest_fanout_skips_a_due_user_in_a_blocked_org() {
        let Some((db, redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL/TEST_REDIS_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-digest-fanout-gated-{suffix}")).await;
        let u = seed_user(&db, org.id).await;

        // Scope to rows created from here on; `entity_id` alone can match stale rows on a
        // long-lived shared DB.
        let before = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());

        let mut ctx = test_ctx(db.clone(), redis);
        ctx.entitlements = Arc::new(BlockedByTest);
        fan_out_digests(&ctx).await.unwrap();

        let count =
            send_digest_count(&db, &[u.id], mail_delivery::Column::CreatedAt.gte(before)).await;
        assert_eq!(
            count, 0,
            "a blocked org's due user must not get a digest enqueued"
        );
    }

    /// A disabled user keeps `digest_enabled = true`; the page query must exclude them before the
    /// guard is ever claimed.
    #[tokio::test]
    async fn fan_out_skips_a_disabled_digest_enabled_user() {
        let Some((db, redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL/TEST_REDIS_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-digest-fanout-disableduser-{suffix}")).await;
        let u = seed_user(&db, org.id).await;
        let mut active: user::ActiveModel = u.clone().into();
        active.disabled_at = Set(Some(sea_orm::prelude::DateTimeWithTimeZone::from(
            chrono::Utc::now(),
        )));
        active.update(&db).await.unwrap();

        let today =
            arcodash_core::digest_schedule::user_local_date(Some("UTC"), chrono::Utc::now());
        // A recycled id can inherit a live guard key; start clean.
        crate::scheduler::release_digest_send_guard(&redis, u.id, today).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        // `summary.due` counts the whole shared table, so check this specific user's enqueue
        // instead.
        let before = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
        fan_out_digests(&ctx).await.unwrap();
        let sent =
            send_digest_count(&db, &[u.id], mail_delivery::Column::CreatedAt.gte(before)).await;
        assert_eq!(sent, 0, "a disabled user must not even be counted as due");

        // Release after asserting: the assertion is itself a `SET NX` claim and must not leave the
        // key behind.
        let claimed = crate::scheduler::claim_digest_send_for_user(&redis, u.id, today).await;
        crate::scheduler::release_digest_send_guard(&redis, u.id, today).await;
        assert!(
            claimed,
            "a disabled user's per-day guard must never be claimed"
        );
    }

    /// Same rule for a still-pending invitation, whose `digest_enabled` flag carries no consent.
    #[tokio::test]
    async fn fan_out_skips_a_pending_invitation() {
        let Some((db, redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL/TEST_REDIS_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-digest-fanout-pending-{suffix}")).await;
        let u = seed_user(&db, org.id).await;
        let mut active: user::ActiveModel = u.clone().into();
        active.is_invitation_pending = Set(true);
        active.update(&db).await.unwrap();

        let today =
            arcodash_core::digest_schedule::user_local_date(Some("UTC"), chrono::Utc::now());
        crate::scheduler::release_digest_send_guard(&redis, u.id, today).await;

        let ctx = test_ctx(db.clone(), redis.clone());
        // `summary.due` is a whole-table count; check this user's enqueue instead.
        let before = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
        fan_out_digests(&ctx).await.unwrap();
        let sent =
            send_digest_count(&db, &[u.id], mail_delivery::Column::CreatedAt.gte(before)).await;
        assert_eq!(
            sent, 0,
            "a never-accepted invitation must not be counted as due"
        );

        let claimed = crate::scheduler::claim_digest_send_for_user(&redis, u.id, today).await;
        crate::scheduler::release_digest_send_guard(&redis, u.id, today).await;
        assert!(
            claimed,
            "a pending invitation's per-day guard must never be claimed"
        );
    }

    /// Stands in for a broken send path; a real `ValkeyQueue` on a dead address would fail at
    /// construction instead.
    struct FailingQueue;

    #[async_trait::async_trait]
    impl arcodash_core::queue::Queue for FailingQueue {
        async fn enqueue(
            &self,
            _queue: arcodash_core::queue::QueueName,
            _job: arcodash_core::queue::JobMessage,
        ) -> anyhow::Result<()> {
            anyhow::bail!("enqueue always fails")
        }
    }

    /// A failed `enqueue_send_digest` must release the per-day guard it just claimed.
    #[tokio::test]
    async fn a_failed_digest_enqueue_releases_the_per_day_guard() {
        let Some((db, redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL/TEST_REDIS_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-digest-fanout-enqfail-{suffix}")).await;
        let u = seed_user(&db, org.id).await;

        let today =
            arcodash_core::digest_schedule::user_local_date(Some("UTC"), chrono::Utc::now());
        // A recycled id can inherit a live guard key; start clean.
        crate::scheduler::release_digest_send_guard(&redis, u.id, today).await;

        let mut ctx = test_ctx(db.clone(), redis.clone());
        ctx.queue = Arc::new(FailingQueue);

        fan_out_digests(&ctx).await.unwrap();

        // This assertion is itself a `SET NX` claim, so release it afterward.
        let claimed = crate::scheduler::claim_digest_send_for_user(&redis, u.id, today).await;
        crate::scheduler::release_digest_send_guard(&redis, u.id, today).await;
        assert!(
            claimed,
            "a fresh claim must succeed, proving the failed enqueue released the guard rather \
             than leaving it claimed for the rest of its TTL"
        );
    }

    /// The entitlement gate must run before the per-day guard claim, which must never be claimed
    /// for a blocked org.
    #[tokio::test]
    async fn a_blocked_org_never_claims_the_guard() {
        let Some((db, redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL/TEST_REDIS_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-digest-fanout-blocked-{suffix}")).await;
        let u = seed_user(&db, org.id).await;

        let today =
            arcodash_core::digest_schedule::user_local_date(Some("UTC"), chrono::Utc::now());
        // A recycled id can inherit a live guard key; start clean.
        crate::scheduler::release_digest_send_guard(&redis, u.id, today).await;

        let mut ctx = test_ctx(db.clone(), redis.clone());
        ctx.entitlements = Arc::new(BlockedByTest);
        fan_out_digests(&ctx).await.unwrap();

        // Release after asserting: the assertion is itself a `SET NX` claim.
        let claimed = crate::scheduler::claim_digest_send_for_user(&redis, u.id, today).await;
        crate::scheduler::release_digest_send_guard(&redis, u.id, today).await;
        assert!(
            claimed,
            "a blocked org's user must never have its per-day guard claimed"
        );

        // Scoped by org_id: a recycled user id on a shared test DB could match an unrelated test.
        let count = send_digest_count(&db, &[u.id], mail_delivery::Column::OrgId.eq(org.id)).await;
        assert_eq!(
            count, 0,
            "a blocked org's due user must not get a digest enqueued"
        );
    }

    #[tokio::test]
    async fn handler_reports_success_when_nothing_was_due() {
        let Some((db, redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL/TEST_REDIS_URL not set");
            return;
        };
        // A user whose send hour can never be "now": nothing is due, but the handler must still
        // walk the table without failing.
        let ctx = Arc::new(test_ctx(db, redis));
        let handler = DigestFanoutHandler { ctx };
        let outcome = handler.run(task_ctx(), serde_json::json!({})).await;
        match outcome {
            Ok(TaskOutcome::Success { .. }) => {}
            Ok(_) => panic!("a tick with nothing due must report Success"),
            Err(e) => panic!("a tick with nothing due must not fail: {e}"),
        }
    }

    /// Forcing every enqueue to fail (`FailingQueue`) must surface as a failed `TaskResult`, or
    /// `auto_pause_threshold` could never fire.
    #[tokio::test]
    async fn handler_reports_failure_when_the_body_fails() {
        let Some((db, redis, _redis_url)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL/TEST_REDIS_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-digest-fanout-handler-fail-{suffix}")).await;
        let u = seed_user(&db, org.id).await;
        let today =
            arcodash_core::digest_schedule::user_local_date(Some("UTC"), chrono::Utc::now());
        crate::scheduler::release_digest_send_guard(&redis, u.id, today).await;

        let mut ctx = test_ctx(db, redis.clone());
        ctx.queue = Arc::new(FailingQueue);
        let handler = DigestFanoutHandler { ctx: Arc::new(ctx) };
        let outcome = handler.run(task_ctx(), serde_json::json!({})).await;
        assert!(
            outcome.is_err(),
            "a tick where every enqueue fails must report failure"
        );

        crate::scheduler::release_digest_send_guard(&redis, u.id, today).await;
    }
}
