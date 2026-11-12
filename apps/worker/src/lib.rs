mod ai_sink;
mod consumer;
mod conveyor_schedules;
mod delivery_lifecycle;
mod handlers;
mod health;
mod heartbeat;
mod run_lifecycle;
mod scheduler;

use std::sync::Arc;

use anyhow::Context;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

use arcodash_core::connectors::registry::ConnectorRegistry;
use arcodash_core::entitlements::{OrgEntitlements, Unrestricted};
use arcodash_core::queue::ValkeyQueue;
use arcodash_core::secrets;
use arcodash_core::storage::ObjectStore;
use arcodash_core::{config::Config, db, valkey_client};

use handlers::execute_query::{build_mailer, ExecuteQueryContext};

/// If Postgres RLS is enabled on `table` (`organizations` in production), the worker's role must
/// own the table or be a member of its owner; any other role silently gets zero rows from every
/// org-scoped query, stopping scheduled work for all tenants. Checked once at startup so a
/// misconfigured `DATABASE_URL` fails loudly.
///
/// `table` is a fixed identifier compared as a bound string value (`relname = $1`), never spliced
/// into SQL; parameterized so tests can use a disposable table.
async fn assert_table_rls_bypass_role(
    db: &sea_orm::DatabaseConnection,
    table: &str,
) -> anyhow::Result<()> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};

    let Some(row) = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT relrowsecurity, \
                    pg_has_role(current_user, relowner, 'USAGE') AS has_owner_usage \
             FROM pg_class \
             WHERE relname = $1 AND relnamespace = 'public'::regnamespace",
            [table.into()],
        ))
        .await
        .context("failed to check organizations table RLS/ownership")?
    else {
        // No `organizations` table is a different startup problem, not this check's job.
        return Ok(());
    };

    let rls_enabled: bool = row.try_get("", "relrowsecurity")?;
    if rls_enabled && !row.try_get::<bool>("", "has_owner_usage")? {
        anyhow::bail!(
            "RLS is enabled on organizations but this worker's DATABASE_URL role is not the \
             table's owner (or a member of it) — every org-scoped query would silently return 0 \
             rows and scheduled work would stop for every tenant with no error. Point \
             DATABASE_URL at the owner role, or grant the current role membership in it."
        );
    }
    Ok(())
}

/// Deployment-supplied behavior the worker consults while processing work.
pub struct WorkerExtensions {
    pub entitlements: Arc<dyn OrgEntitlements>,
    /// Resolves each organization's object storage; `None` keeps the shared-bucket default.
    pub storage_resolver: Option<Arc<dyn arcodash_core::org_storage::OrgStorageResolver>>,
}

impl Default for WorkerExtensions {
    fn default() -> Self {
        Self {
            entitlements: Arc::new(Unrestricted),
            storage_resolver: None,
        }
    }
}

/// Runs the worker until a shutdown signal arrives.
pub async fn run(ext: WorkerExtensions) -> anyhow::Result<()> {
    // Fine if absent (e.g. containers where env is injected directly).
    let _ = dotenvy::dotenv();
    let config = Config::from_env()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_new(&config.rust_log).unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    tracing::info!("connecting to database and valkey");
    let db = db::connect(&config.database_url).await?;
    assert_table_rls_bypass_role(&db, "organizations").await?;
    let redis = valkey_client::connect(&config.valkey_url).await?;

    // Cross-binary config agreement: the CONVEYOR_*_ENABLED flags and MAX_QUERY_ROWS must match
    // `apps/backend`. Warning only: a rolling deploy legitimately has one side ahead for a few
    // minutes.
    let config_fingerprint = arcodash_core::config::cross_binary_fingerprint(&config);
    tracing::info!(config_fingerprint, "cross-binary config fingerprint");
    arcodash_core::config::publish_and_check_cross_binary_fingerprint(
        &mut redis.clone(),
        "worker",
        "backend",
        &config_fingerprint,
    )
    .await;
    arcodash_core::config::spawn_cross_binary_fingerprint_check(
        redis.clone(),
        "worker",
        "backend",
        config_fingerprint.clone(),
    );

    let encryption_key =
        secrets::parse_key_hex(&config.encryption_key).context("invalid ENCRYPTION_KEY")?;
    let store = ObjectStore::from_env().context("failed to initialize object storage")?;
    if std::env::var("STORAGE_EMULATOR_HOST").is_ok_and(|v| !v.trim().is_empty()) {
        store
            .ensure_bucket()
            .await
            .context("failed to ensure upload bucket exists against the storage emulator")?;
    }
    let storage = ext.storage_resolver.unwrap_or_else(|| {
        Arc::new(arcodash_core::org_storage::DefaultStorageResolver::new(
            store.clone(),
        ))
    });
    let ctx = Arc::new(ExecuteQueryContext {
        connectors: Arc::new(ConnectorRegistry::with_db(
            db.clone(),
            store.clone(),
            storage.clone(),
        )),
        queue: Arc::new(ValkeyQueue::new(redis.clone())),
        db,
        redis,
        store,
        encryption_key,
        mailer: build_mailer(&config),
        mail_from: config.mail_from.clone(),
        entitlements: ext.entitlements,
        storage,
        base_url: config.base_url.clone(),
        query_timeout_secs: config.query_timeout_secs,
        max_query_rows: config.max_query_rows,
        conveyor: std::sync::OnceLock::new(),
        ai: Some(Arc::new(ai_sink::AiRuntime {
            config: Arc::new(config.clone()),
            http: reqwest::Client::new(),
            provider_lookup: arcodash_core::ai::provider_for,
        })),
    });

    tracing::info!(
        queues = ?config.worker_queues.iter().map(|q| q.as_str()).collect::<Vec<_>>(),
        fast_concurrency = config.fast_concurrency,
        long_concurrency = config.long_concurrency,
        "worker started"
    );

    // Every job holds a DB connection while it works, so concurrency above the pool size just
    // queues jobs on `acquire` until its 30s timeout.
    let pool_max: u32 = arcodash_core::config::env_parsed("DB_MAX_CONNECTIONS", 10);
    let job_concurrency = config.fast_concurrency + config.long_concurrency;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "configured worker concurrency is a small count"
    )]
    let exceeds_pool = job_concurrency as u32 > pool_max;
    if exceeds_pool {
        tracing::warn!(
            pool_max,
            job_concurrency,
            "job concurrency exceeds the database pool size; jobs will queue on connection \
             acquisition — raise DB_MAX_CONNECTIONS or lower FAST_CONCURRENCY/LONG_CONCURRENCY"
        );
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let health_addr = std::net::SocketAddr::new(config.app_host, config.app_port);
    let mut health_handle = tokio::spawn(health::run(
        health_addr,
        shutdown_rx.clone(),
        ctx.db.clone(),
        ctx.redis.clone(),
    ));

    // Set before any consumer/scheduler task starts so every `ctx.conveyor.get()` call site sees a
    // consistent state. One conveyor is built and only task types whose own flag is on are
    // registered; producers also self-gate on their flag.
    let notif_on = arcodash_core::notify::conveyor_send_notification_enabled();
    let digest_on = arcodash_core::mail_notify::conveyor_send_digest_enabled();
    let mail_on = arcodash_core::mail_notify::conveyor_send_mail_enabled();
    let evaluate_monitor_on = arcodash_core::monitor_eval::conveyor_evaluate_monitor_enabled();
    let execute_query_on = arcodash_core::query_exec::conveyor_execute_query_enabled();
    let scheduler_on = conveyor_schedules::scheduler_enabled();

    let conveyor = if notif_on
        || digest_on
        || mail_on
        || evaluate_monitor_on
        || execute_query_on
        || scheduler_on
    {
        let mut builder = arcodash_conveyor::ConveyorBuilder::new(
            ctx.db.clone(),
            ctx.redis.clone(),
            config.valkey_url.clone(),
        );
        let mut task_types: Vec<&str> = Vec::new();

        if notif_on {
            builder = builder.register(
                "send_notification",
                handlers::send_notification::SendNotificationHandler { ctx: ctx.clone() },
            );
            task_types.push("send_notification");
        }
        if digest_on {
            builder = builder.register(
                "send_digest",
                handlers::send_digest::SendDigestHandler { ctx: ctx.clone() },
            );
            task_types.push("send_digest");
        }
        if mail_on {
            builder = builder.register(
                "send_mail",
                handlers::send_mail::SendMailHandler { ctx: ctx.clone() },
            );
            task_types.push("send_mail");
        }
        if evaluate_monitor_on {
            builder = builder.register(
                "evaluate_monitor",
                handlers::evaluate_monitor::EvaluateMonitorHandler { ctx: ctx.clone() },
            );
            task_types.push("evaluate_monitor");
        }
        if execute_query_on {
            builder = builder
                .register(
                    "execute_query",
                    handlers::execute_query::ExecuteQueryHandler { ctx: ctx.clone() },
                )
                .register(
                    "execute_query_scheduled",
                    handlers::execute_query::ExecuteQueryHandler { ctx: ctx.clone() },
                );
        }

        if scheduler_on {
            builder = builder
                .register(
                    "escalation_tick",
                    handlers::escalation_tick::EscalationTickHandler { ctx: ctx.clone() },
                )
                .register(
                    "digest_fanout",
                    handlers::digest_fanout::DigestFanoutHandler { ctx: ctx.clone() },
                )
                .tier("aux", &["escalation_tick", "digest_fanout"], 2);
        }

        if !task_types.is_empty() {
            builder = builder.tier("email", &task_types, config.fast_concurrency);
        }
        if execute_query_on {
            // Same fast/long tier split and env-driven concurrency knobs as the Redis-queue
            // consumer (`consumer::valkey_consumer`), so a long query gets the same shutdown grace
            // through conveyor.
            builder = builder
                .tier("query-fast", &["execute_query"], config.fast_concurrency)
                .tier_with_drain(
                    "query-long",
                    &["execute_query_scheduled"],
                    config.long_concurrency,
                    config.long_drain_secs,
                );
        }

        let conveyor = builder.build();

        let _ = ctx.conveyor.set(conveyor.clone());

        if scheduler_on {
            conveyor_schedules::register_aux_schedules(&ctx.db, &conveyor).await;
        }

        Some(conveyor)
    } else {
        None
    };

    // A disabled conveyor still gets a supervised handle; the placeholder returns only on shutdown.
    let mut conveyor_handle = {
        let mut shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            match conveyor {
                Some(c) => c.run(shutdown).await,
                None => {
                    let _ = shutdown.wait_for(|v| *v).await;
                }
            }
        })
    };

    // The BLPOP loops open dedicated connections from `config.valkey_url` instead of sharing
    // `ctx.redis`, which would stall job-status reads/writes (see `consumer::valkey_consumer`).
    let mut consumer_handle = tokio::spawn(consumer::valkey_consumer::run(
        config.valkey_url.clone(),
        ctx.clone(),
        config.worker_queues.clone(),
        config.fast_concurrency,
        config.long_concurrency,
        config.long_drain_secs,
        job_handler_timeout(config.query_timeout_secs),
        shutdown_rx.clone(),
    ));

    let mut heartbeat_handle = tokio::spawn(heartbeat::run(
        ctx.redis.clone(),
        job_concurrency,
        shutdown_rx.clone(),
    ));

    let mut scheduler_handle = tokio::spawn(scheduler::run(
        ctx,
        config.valkey_url.clone(),
        shutdown_rx.clone(),
    ));

    // Race the always-on tasks against shutdown: any task returning on its own (panic or
    // fall-through) leaves the worker missing a core function, so exit and let the supervisor
    // restart it.
    tokio::select! {
        () = shutdown_signal() => {
            tracing::info!("shutdown signal received, stopping consumers");
            let _ = shutdown_tx.send(true);
            let _ = consumer_handle.await;
            let _ = scheduler_handle.await;
            let _ = heartbeat_handle.await;
            let _ = health_handle.await;
            let _ = conveyor_handle.await;
            tracing::info!("worker shutting down");
        }
        result = &mut consumer_handle => exit_on_task_end("consumer", result),
        result = &mut scheduler_handle => exit_on_task_end("scheduler", result),
        result = &mut heartbeat_handle => exit_on_task_end("heartbeat", result),
        result = &mut health_handle => exit_on_task_end("health", result),
        result = &mut conveyor_handle => exit_on_task_end("conveyor", result),
    }

    Ok(())
}

fn exit_on_task_end(task: &str, result: impl std::fmt::Debug) -> ! {
    tracing::error!(
        ?result,
        "worker: {task} task exited unexpectedly, exiting process"
    );
    std::process::exit(1)
}

/// `JOB_HANDLER_TIMEOUT_SECS`: wall-clock ceiling on one Redis-queue handler, so a wedged handler
/// can't hold its permit forever. The default tracks `QUERY_TIMEOUT_SECS` so it exceeds the longest
/// legitimate handler.
fn job_handler_timeout(query_timeout_secs: u64) -> std::time::Duration {
    let default_secs = query_timeout_secs.saturating_mul(2).max(3600);
    std::time::Duration::from_secs(arcodash_core::config::env_parsed(
        "JOB_HANDLER_TIMEOUT_SECS",
        default_secs,
    ))
}

/// Resolves on Ctrl+C (SIGINT) or SIGTERM, whichever comes first.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

#[cfg(test)]
mod rls_bypass_role_tests {
    //! Exercises `assert_table_rls_bypass_role` against real Postgres (skips without
    //! `TEST_DATABASE_URL`), using a disposable table per test.

    use sea_orm::ConnectionTrait;

    use super::assert_table_rls_bypass_role;

    async fn owner_db() -> Option<sea_orm::DatabaseConnection> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        Some(arcodash_core::db::connect(&url).await.expect("connect"))
    }

    fn unique_table(label: &str) -> String {
        format!("worker_rls_test_{label}_{}", uuid::Uuid::new_v4().simple())
    }

    #[tokio::test]
    async fn passes_when_table_has_no_rls() {
        let Some(db) = owner_db().await else {
            eprintln!("[SKIP] TEST_DATABASE_URL not set");
            return;
        };
        let table = unique_table("norls");
        db.execute_unprepared(&format!("CREATE TABLE {table} (id int)"))
            .await
            .unwrap();

        let result = assert_table_rls_bypass_role(&db, &table).await;

        db.execute_unprepared(&format!("DROP TABLE {table}"))
            .await
            .ok();
        assert!(result.is_ok(), "no RLS on the table must always pass");
    }

    #[tokio::test]
    async fn passes_when_rls_enabled_and_connected_as_the_owning_role() {
        let Some(db) = owner_db().await else {
            eprintln!("[SKIP] TEST_DATABASE_URL not set");
            return;
        };
        let table = unique_table("owner");
        db.execute_unprepared(&format!(
            "CREATE TABLE {table} (id int); ALTER TABLE {table} ENABLE ROW LEVEL SECURITY;"
        ))
        .await
        .unwrap();

        let result = assert_table_rls_bypass_role(&db, &table).await;

        db.execute_unprepared(&format!("DROP TABLE {table}"))
            .await
            .ok();
        assert!(
            result.is_ok(),
            "the role that created the table always has USAGE on itself"
        );
    }

    /// RLS enabled, but connected as a role that neither owns the table nor is a member of its
    /// owner: must fail loudly.
    #[tokio::test]
    async fn fails_when_rls_enabled_and_connected_as_an_unrelated_role() {
        let Some(db) = owner_db().await else {
            eprintln!("[SKIP] TEST_DATABASE_URL not set");
            return;
        };
        let url = std::env::var("TEST_DATABASE_URL").unwrap();
        let table = unique_table("stranger");
        let role = format!("worker_rls_test_role_{}", uuid::Uuid::new_v4().simple());

        db.execute_unprepared(&format!(
            "CREATE TABLE {table} (id int); ALTER TABLE {table} ENABLE ROW LEVEL SECURITY;"
        ))
        .await
        .unwrap();
        db.execute_unprepared(&format!(
            "CREATE ROLE {role} LOGIN PASSWORD 'worker-rls-test-pw';"
        ))
        .await
        .unwrap();

        let stranger_dsn = dsn_with_role(&url, &role, "worker-rls-test-pw");
        let stranger_db = arcodash_core::db::connect(&stranger_dsn)
            .await
            .expect("connect as unrelated role");

        let result = assert_table_rls_bypass_role(&stranger_db, &table).await;

        drop(stranger_db);
        db.execute_unprepared(&format!("DROP TABLE {table}"))
            .await
            .ok();
        db.execute_unprepared(&format!("DROP ROLE {role}"))
            .await
            .ok();

        assert!(
            result.is_err(),
            "an unrelated role under RLS must fail this check, not silently pass"
        );
    }

    /// Swap the user:password segment of a Postgres DSN — `postgres://user:pass@host:port/db`.
    fn dsn_with_role(dsn: &str, user: &str, pass: &str) -> String {
        let (scheme, rest) = dsn.split_once("://").expect("dsn missing scheme");
        let at = rest.rfind('@').expect("dsn missing @host");
        let host_and_rest = &rest[at + 1..];
        format!("{scheme}://{user}:{pass}@{host_and_rest}")
    }
}

#[cfg(all(test, unix))]
mod graceful_drain_tests {
    //! Real subprocess SIGTERM test: (a) an in-flight fast-tier job still completes, and (b) a
    //! long-tier job that can't finish inside its drain grace doesn't hang exit. Spawns the
    //! compiled worker binary to exercise the real shutdown wiring. Needs real Postgres + Redis +
    //! target Postgres + a storage emulator (skips without
    //! `TEST_DATABASE_URL`/`TEST_REDIS_URL`/`TEST_TARGET_DATABASE_URL`/`STORAGE_EMULATOR_HOST`).
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use arcodash_core::queue::{self as core_queue, JobMessage, JobStatus, Queue, QueueName};

    use crate::scheduler::test_support::*;

    /// Builds the worker binary from current sources and returns its path (inline tests get no
    /// `CARGO_BIN_EXE_*`).
    fn worker_binary_path() -> std::path::PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = root.join(
            std::env::var_os("CARGO_TARGET_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| "target".into()),
        );
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "-p", "arcodash-worker"])
            .env("CARGO_TARGET_DIR", &target)
            .current_dir(&root)
            .status()
            .expect("failed to invoke cargo build for the worker binary");
        assert!(status.success(), "cargo build -p arcodash-worker failed");
        target.join("debug/arcodash-worker")
    }

    async fn wait_for_status(
        redis: &redis::aio::ConnectionManager,
        job_id: &str,
        deadline: Duration,
    ) -> core_queue::JobStatusRecord {
        let start = Instant::now();
        loop {
            if let Some(status) = core_queue::get_job_status(redis, job_id).await.unwrap() {
                return status;
            }
            assert!(
                start.elapsed() < deadline,
                "job {job_id} never reported a status within {deadline:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn sigterm_drains_fast_job_and_caps_wait_on_long_job() {
        let Ok(database_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let Ok(storage_emulator) = std::env::var("STORAGE_EMULATOR_HOST") else {
            eprintln!("skipping: STORAGE_EMULATOR_HOST not set");
            return;
        };
        let redis_url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL required");
        let bin = worker_binary_path();
        assert!(bin.exists(), "worker binary missing at {bin:?}");

        let db = arcodash_core::db::connect(&database_url).await.unwrap();
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();

        let encryption_key = [9u8; 32];
        let encryption_key_hex = arcodash_core::secrets::hex_encode(&encryption_key);
        let org = seed_org(&db, &format!("org-sigterm-{}", uuid::Uuid::new_v4())).await;
        let Some(ds) = seed_target_pg_data_source(&db, org.id, &encryption_key).await else {
            eprintln!("skipping: TEST_TARGET_DATABASE_URL not set");
            return;
        };

        // Other suites leave jobs on these shared queues; with LONG_CONCURRENCY=1 a backlog would
        // block the long job.
        let _: () = redis::cmd("DEL")
            .arg(QueueName::Queries.queue_key())
            .arg(QueueName::ScheduledQueries.queue_key())
            .query_async(&mut redis.clone())
            .await
            .unwrap();

        // Enqueued before the child starts, so BLPOP picks it up immediately with no startup race.
        let queue = arcodash_core::queue::ValkeyQueue::new(redis.clone());
        let fast_job_id = format!("sigterm-fast-{}", uuid::Uuid::new_v4());
        queue
            .enqueue(
                QueueName::Queries,
                JobMessage::ExecuteQuery {
                    job_id: fast_job_id.clone(),
                    org_id: org.id,
                    data_source_id: ds.id,
                    query_text: "SELECT 1".to_string(),
                    parameters: serde_json::json!({}),
                    user_id: 1,
                    query_id: None,
                    scheduled: false,
                    run_id: None,
                    repoint_published: false,
                    max_rows: None,
                },
            )
            .await
            .unwrap();
        let long_job_id = format!("sigterm-long-{}", uuid::Uuid::new_v4());
        queue
            .enqueue(
                QueueName::ScheduledQueries,
                JobMessage::ExecuteQuery {
                    job_id: long_job_id.clone(),
                    org_id: org.id,
                    data_source_id: ds.id,
                    query_text: "SELECT pg_sleep(30)".to_string(),
                    parameters: serde_json::json!({}),
                    user_id: 1,
                    query_id: None,
                    scheduled: true,
                    run_id: None,
                    repoint_published: false,
                    max_rows: None,
                },
            )
            .await
            .unwrap();

        // Free port for the health server; cwd outside the repo keeps dotenvy from loading `.env`.
        let health_port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut child = tokio::process::Command::new(&bin)
            .current_dir(std::env::temp_dir())
            .env_clear()
            .env("APP_HOST", "127.0.0.1")
            .env("APP_PORT", health_port.to_string())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("DATABASE_URL", &database_url)
            .env("VALKEY_URL", &redis_url)
            .env("ENCRYPTION_KEY", &encryption_key_hex)
            .env("COOKIE_SECRET", "sigterm-test-cookie-secret")
            .env("UPLOAD_BUCKET", "sigterm-test-bucket")
            .env("STORAGE_EMULATOR_HOST", &storage_emulator)
            .env("WORKER_QUEUES", "queries,scheduled_queries")
            .env("FAST_CONCURRENCY", "2")
            .env("LONG_CONCURRENCY", "1")
            .env("LONG_DRAIN_SECS", "3")
            .env("ARCODASH_ALLOW_PRIVATE_CONNECTORS", "1")
            .env("RUST_LOG", "warn")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // A failed assertion must not orphan a worker that drains the shared test queues.
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn worker binary");
        let pid = child.id().expect("child must have a pid right after spawn");

        // Both jobs must be dispatched (Started) before SIGTERM, or the test proves nothing about
        // draining.
        wait_for_status(&redis, &fast_job_id, Duration::from_secs(10)).await;
        wait_for_status(&redis, &long_job_id, Duration::from_secs(10)).await;

        let shutdown_start = Instant::now();
        let killed = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .expect("failed to invoke kill(1)");
        assert!(killed.success(), "kill -TERM {pid} failed to send");

        // LONG_DRAIN_SECS=3 caps the long job's grace; the fast job finishes well inside it. 10s
        // leaves headroom above 3s but far below the long job's 30s pg_sleep, so waiting on it
        // would time out.
        let exit_status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("worker process did not exit within 10s — drain grace failed to cap the wait")
            .expect("failed to wait on child process");
        let shutdown_elapsed = shutdown_start.elapsed();
        assert!(
            exit_status.success(),
            "worker process exited non-zero after SIGTERM: {exit_status:?}"
        );
        assert!(
            shutdown_elapsed < Duration::from_secs(8),
            "shutdown took {shutdown_elapsed:?} — expected it capped near LONG_DRAIN_SECS=3s, \
             not approaching the long job's 30s sleep"
        );

        let fast_status = core_queue::get_job_status(&redis, &fast_job_id)
            .await
            .unwrap()
            .expect("fast job status must still be present");
        assert_eq!(
            fast_status.status,
            JobStatus::Success,
            "fast-tier job must complete before the process exits, not be killed mid-flight by \
             the old fire-and-forget spawn (error: {:?})",
            fast_status.error
        );

        let long_status = core_queue::get_job_status(&redis, &long_job_id)
            .await
            .unwrap()
            .expect("long job status must still be present");
        assert_eq!(
            long_status.status,
            JobStatus::Started,
            "long-tier job should still be Started (abandoned once its drain grace expired), \
             not Success — proves the drain grace actually capped the wait rather than letting \
             the 30s sleep finish"
        );

        eprintln!(
            "worker exited {shutdown_elapsed:?} after SIGTERM (LONG_DRAIN_SECS=3, long job's \
             pg_sleep(30) still in flight)"
        );
    }
}
