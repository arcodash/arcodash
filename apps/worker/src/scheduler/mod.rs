//! The scheduling engine: the deadline loop (`deadline`/`sweeper`) plus the digest cron and
//! escalation tick (`run_aux_ticks`).
//!
//! Leader election: each job has its own Redis lock key (acquire-or-renew via Lua) with a TTL
//! slightly longer than its interval; a replica that can't acquire skips that tick. Correctness
//! never depends on which replica holds a lock.

pub mod deadline;
pub mod escalations;
pub mod sweeper;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use redis::AsyncCommands;
use tokio::sync::watch;
use tokio_cron_scheduler::{Job, JobScheduler};

use crate::handlers::execute_query::ExecuteQueryContext;

const LEADER_LOCK_KEY_MONITORS: &str = "arcodash:scheduler:leader:monitors";
const LEADER_LOCK_KEY_DIGEST: &str = "arcodash:scheduler:leader:digest";
const LEADER_LOCK_KEY_UPLOAD_SWEEP: &str = "arcodash:scheduler:leader:upload_sweep";
const LEADER_LOCK_KEY_SCHEMA_DAILY: &str = "arcodash:scheduler:leader:schema_daily";
/// TTL for the once-per-user-per-day digest idempotence guard; two days so the key outlives the day
/// it names.
const DIGEST_SENT_GUARD_TTL_SECS: u64 = 172_800;
/// How long `run` waits for `run_aux_ticks` to finish its shutdown (scheduler stop plus lock
/// releases).
const AUX_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Test-only: when set, `run_aux_ticks` sleeps briefly after observing shutdown, before releasing
/// leader locks.
#[cfg(test)]
static TEST_DELAY_AUX_SHUTDOWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Default monitor tick interval (5 minutes), used when `MONITOR_TICK_SECS` is unset.
const MONITOR_TICK_SECS_DEFAULT: u64 = 300;

/// `MONITOR_TICK_SECS`: how often the escalation tick runs (monitor evaluation itself is
/// deadline-driven). Floored at `arcodash_core::config::MONITOR_TICK_SECS_MIN` so a bad value can't
/// cause a hot loop. `conveyor_schedules::escalation_interval_secs` has a separate 60s floor
/// because Conveyor rejects shorter intervals.
fn monitor_tick_secs() -> u64 {
    let configured: u64 =
        arcodash_core::config::env_parsed("MONITOR_TICK_SECS", MONITOR_TICK_SECS_DEFAULT);
    let floored = configured.max(arcodash_core::config::MONITOR_TICK_SECS_MIN);
    if floored != configured {
        tracing::warn!(
            configured_secs = configured,
            floored_secs = floored,
            "MONITOR_TICK_SECS is below the hot-loop floor, clamping"
        );
    }
    floored
}

/// Default interval for the orphaned-upload sweep (1 hour); floored at 60s so a
/// zero/negative-parsed `UPLOAD_SWEEP_SECS` can't turn it into a hot loop.
const UPLOAD_SWEEP_SECS_DEFAULT: u64 = 3600;
const UPLOAD_SWEEP_SECS_MIN: u64 = 60;

fn upload_sweep_secs() -> u64 {
    let configured: u64 =
        arcodash_core::config::env_parsed("UPLOAD_SWEEP_SECS", UPLOAD_SWEEP_SECS_DEFAULT);
    configured.max(UPLOAD_SWEEP_SECS_MIN)
}

/// Acquire or renew this replica's lock for `domain_key`. Contention and a Redis error both count
/// as "not leader" and are both counted.
pub async fn acquire_leader_lock(
    redis: &redis::aio::ConnectionManager,
    domain_key: &str,
    replica_id: &str,
) -> bool {
    let held = arcodash_conveyor::acquire_leader_lock(redis, domain_key, replica_id).await
        == arcodash_conveyor::LeaderLockOutcome::Acquired;
    if !held {
        crate::health::LEADER_ACQUIRE_FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    held
}

pub use arcodash_conveyor::release_leader_lock;

/// Entry point: run the deadline engine plus the escalation/digest periodic jobs, for the life
/// of the process.
pub async fn run(
    ctx: Arc<ExecuteQueryContext>,
    redis_url: String,
    shutdown: watch::Receiver<bool>,
) {
    let aux = tokio::spawn(run_aux_ticks(ctx.clone(), shutdown.clone()));
    deadline::run(ctx, redis_url, shutdown).await;
    // `run_aux_ticks` releases its leader locks on shutdown; aborting it could strand a lock for
    // its TTL, so give it a bounded grace and then detach.
    if tokio::time::timeout(AUX_SHUTDOWN_GRACE, aux).await.is_err() {
        tracing::warn!("scheduler: aux ticks did not finish shutting down in time");
    }
}

/// The digest cron + escalation tick, which aren't `schedules`-table entities. Both are skipped
/// when the conveyor scheduler owns aux scheduling, to avoid two ticks doing the same scan
/// (concurrent fires are safe no-ops anyway).
async fn run_aux_ticks(ctx: Arc<ExecuteQueryContext>, mut shutdown: watch::Receiver<bool>) {
    let lock_id = uuid::Uuid::new_v4().to_string();
    let aux_owned_by_conveyor = crate::conveyor_schedules::scheduler_enabled();

    let mut sched = match JobScheduler::new().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "scheduler: failed to create aux JobScheduler, digest/escalation disabled");
            return;
        }
    };

    let mut jobs = Vec::new();
    if !aux_owned_by_conveyor {
        jobs.push((
            "escalation",
            Job::new_repeated_async(
                Duration::from_secs(monitor_tick_secs()),
                leader_tick(&ctx, &lock_id, LEADER_LOCK_KEY_MONITORS, escalation_tick),
            ),
        ));
        // Hourly: each user has their own send hour; `fan_out_digests` filters to who is due.
        jobs.push((
            "digest",
            Job::new_async(
                "0 0 * * * *",
                leader_tick(&ctx, &lock_id, LEADER_LOCK_KEY_DIGEST, digest_tick),
            ),
        ));
    }
    jobs.push((
        "upload sweep",
        Job::new_repeated_async(
            Duration::from_secs(upload_sweep_secs()),
            leader_tick(
                &ctx,
                &lock_id,
                LEADER_LOCK_KEY_UPLOAD_SWEEP,
                |ctx| async move { sweeper::sweep_orphan_uploads(&ctx).await },
            ),
        ),
    ));
    jobs.push((
        "daily schema",
        Job::new_async(
            "0 17 * * * *",
            leader_tick(
                &ctx,
                &lock_id,
                LEADER_LOCK_KEY_SCHEMA_DAILY,
                schema_daily_tick,
            ),
        ),
    ));

    for (name, job) in jobs {
        let job = match job {
            Ok(job) => job,
            Err(e) => {
                tracing::error!(error = %e, "scheduler: failed to create {name} job");
                return;
            }
        };
        if let Err(e) = sched.add(job).await {
            tracing::error!(error = %e, "scheduler: failed to add {name} job");
            return;
        }
    }

    if let Err(e) = sched.start().await {
        tracing::error!(error = %e, "scheduler: failed to start aux JobScheduler");
        return;
    }

    tracing::info!(
        aux_owned_by_conveyor,
        "scheduler: aux ticks started (escalation + digest skipped when the conveyor scheduler owns them)"
    );
    let _ = shutdown.changed().await;
    #[cfg(test)]
    if TEST_DELAY_AUX_SHUTDOWN.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    if let Err(e) = sched.shutdown().await {
        tracing::warn!(error = %e, "scheduler: error during aux ticks shutdown");
    }

    // Each lock is only held transiently inside a tick, so releasing all of them is a harmless
    // no-op for any not held.
    for key in [
        LEADER_LOCK_KEY_MONITORS,
        LEADER_LOCK_KEY_DIGEST,
        LEADER_LOCK_KEY_UPLOAD_SWEEP,
        LEADER_LOCK_KEY_SCHEMA_DAILY,
    ] {
        release_leader_lock(&ctx.redis, key, &lock_id).await;
    }
}

/// A `tokio_cron_scheduler` job body that runs `tick` only on the replica holding `lock_key`.
fn leader_tick<F, Fut>(
    ctx: &Arc<ExecuteQueryContext>,
    lock_id: &str,
    lock_key: &'static str,
    tick: F,
) -> impl FnMut(uuid::Uuid, JobScheduler) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync
where
    F: Fn(Arc<ExecuteQueryContext>) -> Fut + Copy + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let ctx = ctx.clone();
    let lock_id = lock_id.to_string();
    move |_, _| {
        let ctx = ctx.clone();
        let lock_id = lock_id.clone();
        Box::pin(async move {
            if acquire_leader_lock(&ctx.redis, lock_key, &lock_id).await {
                tick(ctx).await;
            }
        })
    }
}

// No schedule row backs the aux ticks, so their failures are only surfaced.

async fn escalation_tick(ctx: Arc<ExecuteQueryContext>) {
    match escalations::run_due_escalations(
        &ctx.db,
        ctx.entitlements.as_ref(),
        ctx.queue.as_ref(),
        ctx.conveyor.get().map(std::convert::AsRef::as_ref),
    )
    .await
    {
        Ok(s) if s.failed > 0 => tracing::warn!(
            due = s.due,
            escalated = s.escalated,
            failed = s.failed,
            "scheduler: escalation tick had failures"
        ),
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "scheduler: escalation tick failed"),
    }
}

async fn digest_tick(ctx: Arc<ExecuteQueryContext>) {
    match crate::handlers::digest_fanout::fan_out_digests(&ctx).await {
        Ok(s) if s.failed > 0 => tracing::warn!(
            due = s.due,
            enqueued = s.enqueued,
            failed = s.failed,
            "scheduler: digest fan-out had failures"
        ),
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "scheduler: digest fan-out failed"),
    }
}

async fn schema_daily_tick(ctx: Arc<ExecuteQueryContext>) {
    let hour = chrono::Timelike::hour(&chrono::Utc::now());
    match crate::handlers::schema_daily::enqueue_daily_schema_passes(&ctx, hour).await {
        Ok(n) => tracing::info!(hour, enqueued = n, "scheduler: daily schema pass"),
        Err(e) => tracing::error!(error = %e, "scheduler: daily schema pass failed"),
    }
}

/// `arcodash:digest:sent:<user_id>:<local-date>`: a `SET NX EX` guard, one per user per local day,
/// so only one tick enqueues a given user's send (and the cron and conveyor fan-out can't
/// double-send).
fn digest_send_guard_key(user_id: i64, local_date: chrono::NaiveDate) -> String {
    format!("arcodash:digest:sent:{user_id}:{local_date}")
}

pub async fn claim_digest_send_for_user(
    redis: &redis::aio::ConnectionManager,
    user_id: i64,
    local_date: chrono::NaiveDate,
) -> bool {
    let mut conn = redis.clone();
    let result: Option<String> = conn
        .set_options(
            digest_send_guard_key(user_id, local_date),
            "1",
            redis::SetOptions::default()
                .conditional_set(redis::ExistenceCheck::NX)
                .with_expiration(redis::SetExpiry::EX(DIGEST_SENT_GUARD_TTL_SECS)),
        )
        .await
        .unwrap_or(None);
    result.is_some()
}

/// Releases a claimed per-user-per-day digest-send guard so a terminal send failure is retried next
/// tick instead of after the full TTL.
///
/// Deletes both `local_date` and the day before: the guard may have been claimed just before local
/// midnight, and the recomputed date would otherwise miss it.
pub async fn release_digest_send_guard(
    redis: &redis::aio::ConnectionManager,
    user_id: i64,
    local_date: chrono::NaiveDate,
) {
    let keys = [
        digest_send_guard_key(user_id, local_date),
        digest_send_guard_key(user_id, local_date - chrono::Duration::days(1)),
    ];
    let _: Result<i64, _> = redis.clone().del(&keys).await;
}

/// Shared seeding helpers for `deadline`'s and `sweeper`'s integration tests; real Postgres + Redis
/// only, guarded by `TEST_DATABASE_URL`/`TEST_REDIS_URL`.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;

    /// Entitlements that block scheduled work for a single org.
    pub(crate) struct BlockOrg(pub i64);

    #[async_trait::async_trait]
    impl arcodash_core::entitlements::OrgEntitlements for BlockOrg {
        async fn scheduled_work(
            &self,
            _db: &DatabaseConnection,
            org_id: i64,
        ) -> arcodash_core::entitlements::WorkGate {
            if org_id == self.0 {
                arcodash_core::entitlements::WorkGate::Blocked {
                    reason: "blocked by test",
                }
            } else {
                arcodash_core::entitlements::WorkGate::Allowed
            }
        }

        fn ai_features(&self, _org: &organization::Model) -> bool {
            true
        }
    }

    /// Entitlements that block all scheduled work and AI features.
    pub(crate) struct BlockedByTest;

    #[async_trait::async_trait]
    impl arcodash_core::entitlements::OrgEntitlements for BlockedByTest {
        async fn scheduled_work(
            &self,
            _db: &DatabaseConnection,
            _org_id: i64,
        ) -> arcodash_core::entitlements::WorkGate {
            arcodash_core::entitlements::WorkGate::Blocked {
                reason: "blocked by test",
            }
        }

        fn ai_features(&self, _org: &organization::Model) -> bool {
            false
        }
    }

    use arcodash_core::connectors::registry::ConnectorRegistry;
    use arcodash_core::entities::{
        data_source, monitor, organization, query, query_result, run, schedule, user,
    };
    use arcodash_core::queue::{JobMessage, ValkeyQueue};
    use sea_orm::prelude::DateTimeWithTimeZone;
    use sea_orm::{
        ActiveModelTrait, ActiveValue::Set, ConnectionTrait, DatabaseConnection, DbBackend,
        EntityTrait, Statement,
    };

    use crate::handlers::execute_query::ExecuteQueryContext;

    pub(crate) fn test_ctx(
        db: DatabaseConnection,
        redis: redis::aio::ConnectionManager,
    ) -> ExecuteQueryContext {
        let store = test_store();
        ExecuteQueryContext {
            db,
            redis: redis.clone(),
            connectors: Arc::new(ConnectorRegistry::default()),
            store: store.clone(),
            encryption_key: [0u8; 32],
            queue: Arc::new(ValkeyQueue::new(redis)),
            mailer: None,
            mail_from: "test@localhost".to_string(),
            base_url: "http://localhost:3000".to_string(),
            query_timeout_secs: 300,
            max_query_rows: 10_000,
            conveyor: std::sync::OnceLock::new(),
            ai: None,
            entitlements: std::sync::Arc::new(arcodash_core::entitlements::Unrestricted),
            storage: std::sync::Arc::new(
                arcodash_core::test_support::blob::StaticResolver::with_store(
                    arcodash_core::org_storage::StorageMode::ArcoDash,
                    Arc::new(arcodash_core::test_support::blob::MemoryBlobStore::default()),
                    "orgs/test/",
                ),
            ),
        }
    }

    pub(crate) async fn setup(
    ) -> Option<(sea_orm::DatabaseConnection, redis::aio::ConnectionManager)> {
        let database_url = std::env::var("TEST_DATABASE_URL").ok()?;
        let redis_url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL required");
        let db = arcodash_core::db::connect(&database_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();
        Some((db, redis))
    }

    pub(crate) async fn acquire_shared_test_db_lock(
        database_url: &str,
    ) -> sea_orm::DatabaseConnection {
        use sea_orm::ConnectionTrait;
        let mut opts = sea_orm::ConnectOptions::new(database_url.to_string());
        opts.max_connections(1).min_connections(1);
        let lock_conn = sea_orm::Database::connect(opts)
            .await
            .expect("connect test serialization lock");
        lock_conn
            .execute_unprepared("SELECT pg_advisory_lock(4021957)")
            .await
            .expect("acquire test serialization lock");
        lock_conn
    }

    /// Removes and returns the first job on the shared queue `key` matching `predicate`, leaving
    /// unrelated jobs alone.
    pub(crate) async fn find_and_remove_job(
        redis: &redis::aio::ConnectionManager,
        key: &str,
        predicate: impl Fn(&JobMessage) -> bool,
    ) -> Option<JobMessage> {
        use redis::AsyncCommands;
        let mut conn = redis.clone();
        let items: Vec<String> = conn.lrange(key, 0, -1).await.unwrap();
        for item in items {
            let Ok(job) = serde_json::from_str::<JobMessage>(&item) else {
                continue;
            };
            if predicate(&job) {
                let _: i64 = conn.lrem(key, 1, item.as_str()).await.unwrap();
                return Some(job);
            }
        }
        None
    }

    pub(crate) async fn find_and_remove_notification_for_monitor(
        redis: &redis::aio::ConnectionManager,
        key: &str,
        monitor_id: i64,
    ) -> Option<JobMessage> {
        find_and_remove_job(redis, key, |job| {
            matches!(job, JobMessage::SendNotification { monitor_id: mid, .. } if *mid == monitor_id)
        })
        .await
    }

    /// Runs a raw Postgres statement, panicking on error.
    pub(crate) async fn exec_sql(
        db: &DatabaseConnection,
        sql: &str,
        values: impl IntoIterator<Item = sea_orm::Value>,
    ) {
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            values,
        ))
        .await
        .unwrap();
    }

    /// The one row a raw Postgres query must return.
    pub(crate) async fn query_row(
        db: &DatabaseConnection,
        sql: &str,
        values: impl IntoIterator<Item = sea_orm::Value>,
    ) -> sea_orm::QueryResult {
        db.query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            values,
        ))
        .await
        .unwrap()
        .unwrap()
    }

    pub(crate) async fn load_run(db: &DatabaseConnection, run_id: i64) -> run::Model {
        run::Entity::find_by_id(run_id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    pub(crate) async fn run_status(db: &DatabaseConnection, run_id: i64) -> String {
        query_row(db, "SELECT status FROM runs WHERE id = $1", [run_id.into()])
            .await
            .try_get("", "status")
            .unwrap()
    }

    pub(crate) fn ctx_with_registry(
        db: sea_orm::DatabaseConnection,
        redis: redis::aio::ConnectionManager,
        registry: ConnectorRegistry,
    ) -> ExecuteQueryContext {
        ExecuteQueryContext {
            connectors: Arc::new(registry),
            query_timeout_secs: 30,
            ..test_ctx(db, redis)
        }
    }

    /// `ObjectStore::from_env()` against the emulator, defaulting `UPLOAD_BUCKET` when unset.
    pub(crate) fn test_store() -> Arc<arcodash_core::storage::ObjectStore> {
        if std::env::var("UPLOAD_BUCKET").is_err() {
            std::env::set_var("UPLOAD_BUCKET", "arcodash-uploads-test");
        }
        arcodash_core::storage::ObjectStore::from_env().expect("test object store")
    }

    pub(crate) async fn seed_org(db: &DatabaseConnection, slug: &str) -> organization::Model {
        let now = DateTimeWithTimeZone::from(chrono::Utc::now());
        organization::ActiveModel {
            name: Set(slug.to_string()),
            slug: Set(slug.to_string()),
            settings: Set(serde_json::json!({})),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    pub(crate) async fn seed_data_source(
        db: &DatabaseConnection,
        org_id: i64,
    ) -> data_source::Model {
        insert_data_source(db, org_id, "pg", "sched-test-ds", None).await
    }

    /// A data source of connector type `type_name` with no options.
    pub(crate) async fn seed_data_source_of_type(
        db: &DatabaseConnection,
        org_id: i64,
        type_name: &str,
    ) -> data_source::Model {
        insert_data_source(db, org_id, type_name, type_name, None).await
    }

    async fn insert_data_source(
        db: &DatabaseConnection,
        org_id: i64,
        type_name: &str,
        name_prefix: &str,
        encrypted_options: Option<Vec<u8>>,
    ) -> data_source::Model {
        let now = DateTimeWithTimeZone::from(chrono::Utc::now());
        let suffix = uuid::Uuid::new_v4();
        data_source::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("{name_prefix}-{suffix}")),
            r#type: Set(type_name.to_string()),
            encrypted_options: Set(encrypted_options),
            scheduled_queue_name: Set(None),
            schema: Set(None),
            schema_refreshed_at: Set(None),
            paused_at: Set(None),
            pause_reason: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    /// A real `pg` data source pointing at the testing stack's target Postgres
    /// (`TEST_TARGET_DATABASE_URL`), so tests can run real `ExecuteQuery` jobs (e.g.
    /// `pg_sleep(n)`). `None` when the env var is unset.
    pub(crate) async fn seed_target_pg_data_source(
        db: &DatabaseConnection,
        org_id: i64,
        encryption_key: &[u8; 32],
    ) -> Option<data_source::Model> {
        let url = std::env::var("TEST_TARGET_DATABASE_URL").ok()?;
        // Manual postgres://user:pass@host:port/dbname parse, kept local to avoid a cross-crate
        // test-only dependency.
        let rest = url
            .strip_prefix("postgres://")
            .or_else(|| url.strip_prefix("postgresql://"))
            .expect("TEST_TARGET_DATABASE_URL must start with postgres(ql)://");
        let (userinfo, hostpath) = rest.split_once('@').expect("missing userinfo");
        let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        let (hostport, dbname) = hostpath.split_once('/').expect("missing dbname");
        let (host, port_str) = hostport.split_once(':').unwrap_or((hostport, "5432"));
        let port: u16 = port_str.parse().unwrap_or(5432);
        let opts = serde_json::json!({
            "host": host, "port": port, "user": user, "password": password,
            "dbname": dbname, "sslmode": "disable",
        });
        let encrypted = arcodash_core::secrets::encrypt(
            encryption_key,
            serde_json::to_vec(&opts).unwrap().as_slice(),
        )
        .unwrap();
        Some(insert_data_source(db, org_id, "pg", "sched-test-target-ds", Some(encrypted)).await)
    }

    /// Not mapped on `data_source::Model` (unused by any entity field) — set directly via SQL.
    pub(crate) async fn set_max_concurrent_runs(db: &DatabaseConnection, ds_id: i64, cap: i64) {
        exec_sql(
            db,
            "UPDATE data_sources SET max_concurrent_runs = $1 WHERE id = $2",
            [cap.into(), ds_id.into()],
        )
        .await;
    }

    /// An unsaved user in `groups`, with a unique email and API key.
    pub(crate) fn new_user(org_id: i64, groups: Vec<i64>) -> user::ActiveModel {
        let now = DateTimeWithTimeZone::from(chrono::Utc::now());
        let suffix = uuid::Uuid::new_v4();
        user::ActiveModel {
            org_id: Set(org_id),
            name: Set("sched-test-user".to_string()),
            email: Set(format!("sched-test-{suffix}@test.local")),
            password_hash: Set(None),
            groups: Set(groups),
            api_key_hash: Set(arcodash_core::hash::hash_api_key(&suffix.to_string())),
            disabled_at: Set(None),
            is_invitation_pending: Set(false),
            profile_image_url: Set(None),
            details: Set(serde_json::json!({})),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
    }

    pub(crate) async fn seed_user(
        db: &DatabaseConnection,
        org_id: i64,
        groups: Vec<i64>,
    ) -> user::Model {
        new_user(org_id, groups).insert(db).await.unwrap()
    }

    pub(crate) async fn seed_query(
        db: &DatabaseConnection,
        org_id: i64,
        ds_id: i64,
    ) -> query::Model {
        let now = DateTimeWithTimeZone::from(chrono::Utc::now());
        let suffix = uuid::Uuid::new_v4();
        let u = seed_user(db, org_id, vec![]).await;

        query::ActiveModel {
            org_id: Set(org_id),
            data_source_id: Set(ds_id),
            name: Set(format!("sched-test-query-{suffix}")),
            description: Set(None),
            query: Set("SELECT 1".to_string()),
            query_hash: Set(format!("hash-{suffix}")),
            api_key: Set(suffix.to_string()),
            user_id: Set(u.id),
            last_modified_by_id: Set(None),
            latest_query_data_id: Set(None),
            is_archived: Set(false),
            options: Set(serde_json::json!({})),
            tags: Set(None),
            last_execution_status: Set(String::new()),
            last_execution_at: Set(None),
            row_count: Set(None),
            last_execution_error: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    /// Seed a draft query, then mark it published (copied into `published_*` with a throwaway
    /// `changes` row for the FK).
    pub(crate) async fn seed_published_query(
        db: &DatabaseConnection,
        org_id: i64,
        ds_id: i64,
    ) -> query::Model {
        let q = seed_query(db, org_id, ds_id).await;
        let change_id: i64 = query_row(
            db,
            "INSERT INTO changes (object_type, object_id, user_id, change, created_at, updated_at) \
             VALUES ('query', $1, $2, $3, now(), now()) RETURNING id",
            [q.id.into(), q.user_id.into(), serde_json::json!({}).into()],
        )
        .await
        .try_get("", "id")
        .unwrap();

        exec_sql(
            db,
            "UPDATE queries SET published_name = name, published_description = description, \
             published_query = query, published_query_hash = query_hash, \
             published_data_source_id = data_source_id, published_options = options, \
             published_change_id = $1, published_at = now() WHERE id = $2",
            [change_id.into(), q.id.into()],
        )
        .await;

        query::Entity::find_by_id(q.id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn seed_schedule(
        db: &DatabaseConnection,
        org_id: i64,
        entity_type: &str,
        entity_id: i64,
        spec: serde_json::Value,
        overlap_policy: &str,
        next_run_at: chrono::DateTime<chrono::Utc>,
        consecutive_failures: i32,
    ) -> schedule::Model {
        schedule::ActiveModel {
            org_id: Set(org_id),
            entity_type: Set(entity_type.to_string()),
            entity_id: Set(entity_id),
            spec: Set(spec),
            timezone: Set("UTC".to_string()),
            enabled: Set(true),
            paused_until: Set(None),
            auto_paused_at: Set(None),
            overlap_policy: Set(overlap_policy.to_string()),
            next_run_at: Set(Some(next_run_at.into())),
            consecutive_failures: Set(consecutive_failures),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    /// Seed a `query_results` row with a single-row `{"rows": [<row>]}` payload at a caller-chosen
    /// `retrieved_at`.
    pub(crate) async fn seed_query_result(
        db: &DatabaseConnection,
        org_id: i64,
        ds_id: i64,
        retrieved_at: chrono::DateTime<chrono::Utc>,
        row: serde_json::Value,
    ) -> query_result::Model {
        let now = DateTimeWithTimeZone::from(chrono::Utc::now());
        let suffix = uuid::Uuid::new_v4();
        query_result::ActiveModel {
            org_id: Set(org_id),
            data_source_id: Set(ds_id),
            query_hash: Set(format!("hash-{suffix}")),
            query: Set("SELECT 1".to_string()),
            data: Set(Some(serde_json::json!({ "rows": [row] }))),
            data_location: Set(None),
            data_size_bytes: Set(None),
            runtime: Set(Some(0.01)),
            retrieved_at: Set(retrieved_at.into()),
            last_used_at: Set(retrieved_at.into()),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    /// Point a query's draft result pointer at `qr_id`.
    pub(crate) async fn set_latest_query_data(db: &DatabaseConnection, query_id: i64, qr_id: i64) {
        exec_sql(
            db,
            "UPDATE queries SET latest_query_data_id = $1 WHERE id = $2",
            [qr_id.into(), query_id.into()],
        )
        .await;
    }

    /// Point a query's PUBLISHED result pointer at `qr_id`.
    pub(crate) async fn set_published_latest_query_data(
        db: &DatabaseConnection,
        query_id: i64,
        qr_id: i64,
    ) {
        exec_sql(
            db,
            "UPDATE queries SET published_latest_query_data_id = $1 WHERE id = $2",
            [qr_id.into(), query_id.into()],
        )
        .await;
    }

    pub(crate) async fn seed_monitor(
        db: &DatabaseConnection,
        org_id: i64,
        query_id: i64,
        user_id: i64,
        options: serde_json::Value,
    ) -> monitor::Model {
        let now = DateTimeWithTimeZone::from(chrono::Utc::now());
        let suffix = uuid::Uuid::new_v4();
        monitor::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("sched-test-monitor-{suffix}")),
            query_id: Set(query_id),
            user_id: Set(user_id),
            options: Set(options),
            state: Set("unknown".to_string()),
            visibility: Set("workspace".to_string()),
            last_triggered_at: Set(None),
            rearm: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            acked_at: Set(None),
            acked_by: Set(None),
            escalation_step: Set(0),
            last_escalated_at: Set(None),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    /// Insert a `runs` row directly with any status/lease/schedule_id, bypassing `plan_run`'s
    /// conflict handling.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn seed_run(
        db: &DatabaseConnection,
        org_id: i64,
        entity_type: &str,
        entity_id: i64,
        schedule_id: Option<i64>,
        data_source_id: Option<i64>,
        status: &str,
        lease_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> i64 {
        query_row(
            db,
            "INSERT INTO runs (org_id, entity_type, entity_id, schedule_id, data_source_id, \
                               triggered_by, status, lease_expires_at, created_at) \
             VALUES ($1, $2, $3, $4, $5, 'manual', $6, $7, now()) RETURNING id",
            [
                org_id.into(),
                entity_type.into(),
                entity_id.into(),
                schedule_id.into(),
                data_source_id.into(),
                status.into(),
                lease_expires_at.into(),
            ],
        )
        .await
        .try_get("", "id")
        .unwrap()
    }

    /// An unscheduled `seed_run` for query `q`, on its own org and data source.
    pub(crate) async fn seed_query_run(
        db: &DatabaseConnection,
        q: &query::Model,
        status: &str,
        lease_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> i64 {
        seed_run(
            db,
            q.org_id,
            "query",
            q.id,
            None,
            Some(q.data_source_id),
            status,
            lease_expires_at,
        )
        .await
    }
}

/// Real Postgres + Redis only (skips without `TEST_DATABASE_URL`/`TEST_REDIS_URL`). Run with
/// `--test-threads=1`: this module mutates process-wide env vars.
#[cfg(test)]
pub(crate) mod aux_tick_tests {
    use super::*;

    /// Restores a process env var on drop, including on panic, so a failed assertion can't leak it
    /// into the next test.
    pub(crate) struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        pub(crate) fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// Drives the real `run_aux_ticks` with `CONVEYOR_SCHEDULER_ENABLED=1`: neither the in-process
    /// escalation nor digest job may register when the conveyor owns aux scheduling.
    /// `MONITOR_TICK_SECS=1` so a wrongly registered job would fire in the wait window.
    #[tokio::test]
    async fn neither_in_process_job_runs_when_conveyor_owns_aux_scheduling() {
        let Some((db, redis)) = test_support::setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let mut conn = redis.clone();
        let _: () = conn.del(LEADER_LOCK_KEY_MONITORS).await.unwrap_or(());
        let _: () = conn.del(LEADER_LOCK_KEY_DIGEST).await.unwrap_or(());

        let _flag_guard = EnvVarGuard::set("CONVEYOR_SCHEDULER_ENABLED", "1");
        let _tick_guard = EnvVarGuard::set("MONITOR_TICK_SECS", "1");

        let ctx = Arc::new(test_support::test_ctx(db, redis));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(run_aux_ticks(ctx, shutdown_rx));

        // Several 1s cycles pass: ample time for a wrongly registered job to attempt its lock.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let monitors_touched: bool = conn.exists(LEADER_LOCK_KEY_MONITORS).await.unwrap_or(false);
        let digest_touched: bool = conn.exists(LEADER_LOCK_KEY_DIGEST).await.unwrap_or(false);
        assert!(
            !monitors_touched,
            "in-process escalation job must not run when the conveyor scheduler owns aux scheduling"
        );
        assert!(
            !digest_touched,
            "in-process digest job must not run when the conveyor scheduler owns aux scheduling"
        );

        let _ = shutdown_tx.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();
    }

    /// Resets `TEST_DELAY_AUX_SHUTDOWN` on drop, including on panic.
    struct AuxShutdownDelayGuard;

    impl AuxShutdownDelayGuard {
        fn enable() -> Self {
            TEST_DELAY_AUX_SHUTDOWN.store(true, std::sync::atomic::Ordering::SeqCst);
            Self
        }
    }

    impl Drop for AuxShutdownDelayGuard {
        fn drop(&mut self) {
            TEST_DELAY_AUX_SHUTDOWN.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Covers the shutdown race between `scheduler::run`'s aux-task teardown and `run_aux_ticks`'s
    /// reaction to the same signal. `AuxShutdownDelayGuard` holds `run_aux_ticks` in its shutdown
    /// path so `run` must actually wait for the lock releases; replacing the bounded join with
    /// `aux.abort()` makes this fail. Asserts every aux leader lock key is gone after `run`
    /// returns.
    #[tokio::test]
    async fn a_clean_shutdown_releases_every_aux_leader_lock() {
        let Some((db, redis)) = test_support::setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let redis_url = std::env::var("TEST_REDIS_URL").unwrap();

        let mut conn = redis.clone();
        let _: () = conn.del(LEADER_LOCK_KEY_MONITORS).await.unwrap_or(());
        let _: () = conn.del(LEADER_LOCK_KEY_DIGEST).await.unwrap_or(());
        let _: () = conn.del(LEADER_LOCK_KEY_UPLOAD_SWEEP).await.unwrap_or(());

        let _tick_guard = EnvVarGuard::set("MONITOR_TICK_SECS", "1");
        let _delay_guard = AuxShutdownDelayGuard::enable();

        let ctx = Arc::new(test_support::test_ctx(db, redis));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let run_handle = {
            let ctx = ctx.clone();
            tokio::spawn(async move { run(ctx, redis_url, shutdown_rx).await })
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let acquired: bool = conn.exists(LEADER_LOCK_KEY_MONITORS).await.unwrap_or(false);
            if acquired {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "escalation leader lock was never acquired — the scheduler isn't ticking"
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        let _ = shutdown_tx.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(10), run_handle)
            .await
            .expect("scheduler::run must return once shutdown fires")
            .unwrap();

        for key in [
            LEADER_LOCK_KEY_MONITORS,
            LEADER_LOCK_KEY_DIGEST,
            LEADER_LOCK_KEY_UPLOAD_SWEEP,
        ] {
            assert!(
                !conn.exists::<_, bool>(key).await.unwrap_or(true),
                "{key} must be released by the time scheduler::run returns from a clean shutdown"
            );
        }
    }
}

/// Real Redis only (skips cleanly without `TEST_REDIS_URL`), except the unreachable-Redis case.
#[cfg(test)]
mod leader_lock_tests {
    use super::*;

    /// Serializes the tests that assert an exact delta on the global
    /// `crate::health::LEADER_ACQUIRE_FAILED` counter.
    static LEADER_ACQUIRE_FAILED_TEST_LOCK: tokio::sync::Mutex<()> =
        tokio::sync::Mutex::const_new(());

    /// A Redis command failure (a dead port) must report "not leader" rather than panic or hang.
    /// Built via `new_lazy_with_config` because the shared connect helper connects eagerly and
    /// would fail at construction.
    #[tokio::test]
    async fn acquire_leader_lock_reports_not_leader_on_a_redis_error() {
        let _guard = LEADER_ACQUIRE_FAILED_TEST_LOCK.lock().await;
        let client = redis::Client::open("redis://127.0.0.1:1/").unwrap();
        let config = redis::aio::ConnectionManagerConfig::new()
            .set_number_of_retries(0)
            .set_connection_timeout(Some(std::time::Duration::from_millis(200)))
            .set_response_timeout(Some(std::time::Duration::from_millis(200)));
        let bad_redis = redis::aio::ConnectionManager::new_lazy_with_config(client, config)
            .expect("lazy construction never touches the network");
        let held =
            acquire_leader_lock(&bad_redis, "test:leader-lock:unreachable", "replica-1").await;
        assert!(
            !held,
            "an unreachable Redis must report false (not leader), never panic or hang"
        );
    }

    /// The holder must re-acquire (renew) its own lock on the next call without waiting out the
    /// TTL, while a different replica is still rejected.
    #[tokio::test]
    async fn same_replica_renews_immediately_foreign_replica_is_rejected() {
        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        // Its rejected acquire also bumps LEADER_ACQUIRE_FAILED, so it shares the lock guarding
        // that counter.
        let _guard = LEADER_ACQUIRE_FAILED_TEST_LOCK.lock().await;
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();
        let key = format!("test:leader-lock:{}", uuid::Uuid::new_v4());
        let holder = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();

        assert!(
            acquire_leader_lock(&redis, &key, &holder).await,
            "first acquire on a free key must succeed"
        );
        assert!(
            !acquire_leader_lock(&redis, &key, &other).await,
            "a foreign replica must not acquire a key the holder still holds"
        );
        assert!(
            acquire_leader_lock(&redis, &key, &holder).await,
            "the holder must be able to renew its own lock on the very next call, \
             not have to wait out the TTL"
        );

        let mut conn = redis.clone();
        let _: () = conn.del(&key).await.unwrap_or(());
    }

    /// A foreign replica losing to a held lock must count toward `LEADER_ACQUIRE_FAILED` exactly
    /// once per losing call.
    #[tokio::test]
    async fn acquire_leader_lock_counts_contention_as_not_leader() {
        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let _guard = LEADER_ACQUIRE_FAILED_TEST_LOCK.lock().await;
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();
        let key = format!("test:leader-lock-contended:{}", uuid::Uuid::new_v4());
        let holder = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();

        let before =
            crate::health::LEADER_ACQUIRE_FAILED.load(std::sync::atomic::Ordering::Relaxed);

        assert!(
            acquire_leader_lock(&redis, &key, &holder).await,
            "first acquire on a free key must succeed"
        );
        assert!(
            !acquire_leader_lock(&redis, &key, &other).await,
            "a foreign replica must not acquire a key the holder still holds"
        );

        let after = crate::health::LEADER_ACQUIRE_FAILED.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            after - before,
            1,
            "the one losing acquire must increment LEADER_ACQUIRE_FAILED by exactly 1"
        );

        let mut conn = redis.clone();
        let _: () = conn.del(&key).await.unwrap_or(());
    }

    /// `release_leader_lock` must delete the key when held by the releasing replica, and not when a
    /// different replica holds it.
    #[tokio::test]
    async fn release_deletes_only_when_still_held_by_this_replica() {
        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();
        let key = format!("test:leader-lock-release:{}", uuid::Uuid::new_v4());
        let holder = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();

        assert!(acquire_leader_lock(&redis, &key, &holder).await);
        release_leader_lock(&redis, &key, &holder).await;
        let mut conn = redis.clone();
        assert!(
            !conn.exists::<_, bool>(&key).await.unwrap(),
            "release must delete the key when still held by the releasing replica"
        );

        assert!(acquire_leader_lock(&redis, &key, &other).await);
        release_leader_lock(&redis, &key, &holder).await;
        assert!(
            conn.exists::<_, bool>(&key).await.unwrap(),
            "release must NOT delete a key a different replica now legitimately holds"
        );
        let _: () = conn.del(&key).await.unwrap_or(());
    }
}
