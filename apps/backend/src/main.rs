use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::http::HeaderValue;
use tracing_subscriber::EnvFilter;

use arcodash_core::connectors::registry::ConnectorRegistry;
use arcodash_core::entitlements::Unrestricted;
use arcodash_core::queue::build_queue;
use arcodash_core::storage::ObjectStore;
use arcodash_core::{config::Config, db, valkey_client};

// The lib target (src/lib.rs) re-exports these under the crate name `arcodash_backend`.
use arcodash_backend::{
    bootstrap, build_app, services, AppExtensions, AppState, PublicDashboardAccess,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Fine if absent (e.g. containers where env is injected directly).
    let _ = dotenvy::dotenv();
    let config = Config::from_env()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_new(&config.rust_log).unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // SCHEDULE_EVERY_SECS_FLOOR overrides schedule_spec::validate's interval floor; compute_next
    // does not clamp to its own EVERY_SECS_FLOOR, it honors whatever interval was accepted at
    // write time. So a mismatch here means the worker will actually run a schedule this API let
    // through faster than the worker's own constant intends, not slower.
    let backend_floor = services::schedule_service::every_secs_floor();
    if backend_floor != arcodash_core::schedule_spec::EVERY_SECS_FLOOR {
        tracing::warn!(
            schedule_every_secs_floor = backend_floor,
            conveyor_every_secs_floor = arcodash_core::schedule_spec::EVERY_SECS_FLOOR,
            "SCHEDULE_EVERY_SECS_FLOOR disagrees with the conveyor crate's own interval floor"
        );
    }

    tracing::info!("connecting to database and redis");
    let db = db::connect(&config.database_url).await?;
    let mut redis = valkey_client::connect(&config.valkey_url).await?;

    // Cross-binary config agreement: the five CONVEYOR_*_ENABLED flags and MAX_QUERY_ROWS must
    // match between this process and `apps/worker` — see `cross_binary_fingerprint`'s doc for
    // why a mismatch matters. Warning only: a rolling deploy legitimately has one side ahead of
    // the other for a few minutes, and refusing to boot mid-rollout would be worse than the drift
    // this is meant to catch.
    let config_fingerprint = arcodash_core::config::cross_binary_fingerprint(&config);
    tracing::info!(config_fingerprint, "cross-binary config fingerprint");
    arcodash_core::config::publish_and_check_cross_binary_fingerprint(
        &mut redis,
        "backend",
        "worker",
        &config_fingerprint,
    )
    .await;
    arcodash_core::config::spawn_cross_binary_fingerprint_check(
        redis.clone(),
        "backend",
        "worker",
        config_fingerprint.clone(),
    );

    bootstrap::bootstrap_admin_user(&db)
        .await
        .context("first-admin bootstrap failed")?;

    let addr = SocketAddr::new(config.app_host, config.app_port);
    let frontend_origin = HeaderValue::from_str(&config.base_url)
        .context("BASE_URL must be a valid header value (origin)")?;
    let cors = arcodash_backend::middleware::cors::cors(frontend_origin);

    let store = ObjectStore::from_env().context("failed to initialize object storage")?;
    if std::env::var("STORAGE_EMULATOR_HOST").is_ok_and(|v| !v.trim().is_empty()) {
        store
            .ensure_bucket()
            .await
            .context("failed to ensure upload bucket exists against the storage emulator")?;
    }
    let connectors = Arc::new(ConnectorRegistry::with_db(db.clone(), store.clone()));
    let queue = build_queue(&config, redis.clone()).context("failed to build job queue")?;
    // Producer-only: this process never registers a handler or consumes a tier (that's
    // `apps/worker`'s job) — it only ever calls `enqueue()`/`status()`. Built unconditionally on
    // the flag alone (no dependency on the worker having started) since enqueuing doesn't require
    // a consumer to be up yet.
    let conveyor = arcodash_core::query_exec::conveyor_execute_query_enabled().then(|| {
        arcodash_core::query_exec::build_producer_conveyor(
            db.clone(),
            redis.clone(),
            config.valkey_url.clone(),
        )
    });
    let mailer = services::mailer::build_mailer(&config);
    let http = reqwest::Client::new();
    let state = AppState {
        entitlements: Arc::new(Unrestricted),
        reserved_org_settings_keys: Arc::new(Vec::new()),
        external_auth: None,
        storage: Arc::new(arcodash_core::org_storage::DefaultStorageResolver::new(
            store.clone(),
        )),
        db: db.clone(),
        // OSS binary: one pool, same connection as `db`.
        conveyor_db: db,
        redis,
        config,
        connectors,
        store,
        queue,
        conveyor,
        mailer,
        http,
        // OSS binary: no org-resolution hook, so `auth::resolve_org` falls back to the seeded
        // `default` org — a deployment supplies multi-tenant resolution via an `OrgResolver`.
        org_resolver: None,
        // OSS binary: no per-request transaction hook — every request runs on the plain pooled
        // connection (`Db::Pool`). A composition binary injects its own transaction-per-request
        // strategy here.
        request_txn_hook: None,
        // OSS binary: anonymous public dashboards, single org, gated only by DISABLE_PUBLIC_URLS.
        public_dashboard_access: PublicDashboardAccess::Anonymous,
        // OSS: public embeds never auto-expire by default.
        embed_default_expiry: None,
        // OSS binary: no org access gate.
        org_access_check: None,
    };

    // OSS binary: no extensions; a deployment can pass its own `AppExtensions`.
    let app = build_app(state, cors, AppExtensions::default());

    tracing::info!(%addr, "starting arcodash-backend");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(arcodash_core::shutdown::shutdown_signal())
    .await?;

    Ok(())
}
