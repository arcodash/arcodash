//! Integration-test harness for arcodash-backend.
//!
//! `/api/login` is deliberately CSRF-exempt: no `csrf_token` cookie exists before the first login,
//! and the cookie is `SameSite=Strict`, so the only attack is login CSRF onto the attacker's own
//! account. `TestApp::login`'s `GET /api/session` preflight mirrors a real browser.
//!
//! Tests skip gracefully when `TEST_DATABASE_URL` is unset; use `skip_if_none!` in each test.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::{header, HeaderValue, Method};
use tower_http::cors::CorsLayer;

use arcodash_backend::{build_app, AppExtensions, AppState, PublicDashboardAccess};

pub const ADMIN_EMAIL: &str = "admin@test.local";
pub const ADMIN_PASSWORD: &str = "Adm1nT3st!";

#[allow(dead_code)] // helpers used across different integration test files
pub struct TestApp {
    pub address: String,
    pub db: sea_orm::DatabaseConnection,
    /// The exact `AppState` the app was built from, for tests that call services directly.
    pub state: AppState,
    /// Same Redis connection the app enqueues on, so a test can pop a real `JobMessage` without a
    /// worker.
    pub redis: redis::aio::ConnectionManager,
    /// Held for the test's lifetime so only one `spawn_app`-based test mutates the shared DB at a
    /// time (closing the session releases the Postgres advisory lock).
    _serialization_lock: sea_orm::DatabaseConnection,
}

impl TestApp {
    /// Two-step login (GET /session for csrf_token, then POST /login); returns
    /// `(session_cookie_value, csrf_token_value)`.
    pub async fn login(&self, email: &str, password: &str) -> (String, String) {
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .build()
            .unwrap();

        let sess_resp = client
            .get(format!("{}/api/session", self.address))
            .send()
            .await
            .expect("GET /api/session failed");
        assert!(
            sess_resp.status().is_success(),
            "GET /session returned {}",
            sess_resp.status()
        );

        let csrf_token = extract_set_cookie(sess_resp.headers(), "csrf_token")
            .expect("GET /session must set csrf_token cookie");

        let login_resp = client
            .post(format!("{}/api/login", self.address))
            .header("X-CSRF-Token", &csrf_token)
            .json(&serde_json::json!({ "email": email, "password": password }))
            .send()
            .await
            .expect("POST /api/login failed");

        assert_eq!(
            login_resp.status().as_u16(),
            200,
            "login({email}) returned {}: {}",
            login_resp.status(),
            login_resp.text().await.unwrap_or_default()
        );

        let session = extract_set_cookie(login_resp.headers(), "arcodash_session")
            .expect("POST /login must set arcodash_session cookie");

        (session, csrf_token)
    }

    /// An authenticated reqwest Client with session + CSRF cookies/header pre-set.
    #[allow(dead_code)]
    pub async fn admin_client(&self) -> reqwest::Client {
        let (session, csrf) = self.login(ADMIN_EMAIL, ADMIN_PASSWORD).await;
        authed_client(&self.address, &session, &csrf)
    }

    /// Truncate mutable tables and re-seed org + admin so tests start clean.
    #[allow(dead_code)]
    pub async fn cleanup(&self) {
        use sea_orm::ConnectionTrait;
        // Hardcoded truncation order; non-builtin groups are DELETEd (not TRUNCATEd) to preserve
        // migration-seeded builtins.
        self.db
            .execute_unprepared(
                "TRUNCATE TABLE
                    access_permissions, favorites, changes,
                    runs, schedules,
                    query_results, queries,
                    monitor_observations, monitor_subscriptions, monitors,
                    dashboard_narratives,
                    notification_destinations,
                    widgets, visualizations, dashboards,
                    data_source_groups, data_sources,
                    users,
                    events, api_keys,
                    query_snippets,
                    embeds, org_embed_settings,
                    ai_usage
                 RESTART IDENTITY CASCADE;
                 DELETE FROM groups WHERE type = 'regular';
                 DELETE FROM roles WHERE type = 'custom';
                 UPDATE organizations SET settings = '{}' WHERE slug = 'default'",
            )
            .await
            .expect("cleanup truncate failed");
        // Builtin groups/roles are kept; leftover fixture rows would collide on the unique (org_id,
        // name) index. The default org's `settings` is reset because tests mutate it.
        seed_admin_user(&self.db).await;
        // Redis state keyed by data source id would otherwise leak to a later data source reusing
        // the id.
        let mut redis = self.redis.clone();
        let stale: Vec<String> = redis::cmd("KEYS")
            .arg("arcodash:ai_profile:*")
            .query_async(&mut redis)
            .await
            .expect("cleanup redis keys failed");
        if !stale.is_empty() {
            let _: () = redis::cmd("DEL")
                .arg(stale)
                .query_async(&mut redis)
                .await
                .expect("cleanup redis del failed");
        }
    }
}

/// Spin up the Axum app on port 0. Returns `None` when `TEST_DATABASE_URL` is unset.
#[allow(dead_code)]
pub async fn spawn_app() -> Option<TestApp> {
    spawn_app_with(|_| {}).await
}

/// Like `spawn_app`, but with a real SMTP mailer against the test-stack Mailpit (default `localhost:1026`;
/// override via `MAILPIT_SMTP_HOST`/`MAILPIT_SMTP_PORT`).
#[allow(dead_code)]
pub async fn spawn_app_with_real_mailer() -> Option<TestApp> {
    let (mut state, serialization_lock) = build_test_state(|_| {}).await?;
    state.config.smtp_host =
        Some(std::env::var("MAILPIT_SMTP_HOST").unwrap_or_else(|_| "localhost".to_string()));
    state.config.smtp_port = std::env::var("MAILPIT_SMTP_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1026);
    state.config.smtp_tls = "none".to_string();
    state.mailer = arcodash_core::mailer::build_mailer(&state.config);
    Some(spawn_with_state(state, serialization_lock).await)
}

/// Like `spawn_app_with_real_mailer`, but pointed at a closed port, so a real SMTP failure (fails
/// fast) is logged rather than swallowed.
#[allow(dead_code)]
pub async fn spawn_app_with_unreachable_mailer() -> Option<TestApp> {
    let (mut state, serialization_lock) = build_test_state(|_| {}).await?;
    state.config.smtp_host = Some("127.0.0.1".to_string());
    state.config.smtp_port = 1;
    state.config.smtp_tls = "none".to_string();
    state.mailer = arcodash_core::mailer::build_mailer(&state.config);
    Some(spawn_with_state(state, serialization_lock).await)
}

/// Like `spawn_app` but lets a test tweak the `Config` before the app is built
/// (e.g. `spawn_app_with(|c| c.disable_public_urls = true)`).
#[allow(dead_code)]
pub async fn spawn_app_with(
    mutate_config: impl FnOnce(&mut arcodash_core::Config),
) -> Option<TestApp> {
    let (state, serialization_lock) = build_test_state(mutate_config).await?;
    Some(spawn_with_state(state, serialization_lock).await)
}

/// Like `spawn_app_with`, but also sets `AppState.org_resolver` to confirm the hook is consulted.
#[allow(dead_code)]
pub async fn spawn_app_with_org_resolver(
    resolver: std::sync::Arc<dyn arcodash_backend::auth::OrgResolver>,
) -> Option<TestApp> {
    let (mut state, serialization_lock) = build_test_state(|_| {}).await?;
    state.org_resolver = Some(resolver);
    Some(spawn_with_state(state, serialization_lock).await)
}

/// Entitlements that deny AI features and block scheduled work.
#[allow(dead_code)]
pub struct DenyAll;

#[async_trait::async_trait]
impl arcodash_core::entitlements::OrgEntitlements for DenyAll {
    async fn scheduled_work(
        &self,
        _db: &sea_orm::DatabaseConnection,
        _org_id: i64,
    ) -> arcodash_core::entitlements::WorkGate {
        arcodash_core::entitlements::WorkGate::Blocked { reason: "denied" }
    }

    fn ai_features(&self, _org: &arcodash_core::entities::organization::Model) -> bool {
        false
    }
}

/// Like `spawn_app`, but built with the given `AppExtensions` (entitlements, reserved
/// org-settings keys).
#[allow(dead_code)]
pub async fn spawn_app_with_extensions(ext: AppExtensions) -> Option<TestApp> {
    let (state, serialization_lock) = build_test_state(|_| {}).await?;
    Some(spawn_with_state_ext(state, serialization_lock, ext).await)
}

/// Like `spawn_app`, but also wires a real producer `Conveyor` onto `AppState.conveyor` (`None`
/// elsewhere), for HTTP-level tests of the `CONVEYOR_EXECUTE_QUERY_ENABLED` routing. The caller
/// sets/unsets the env var.
#[allow(dead_code)]
pub async fn spawn_app_with_conveyor() -> Option<TestApp> {
    let (mut state, serialization_lock) = build_test_state(|_| {}).await?;
    let redis_url =
        std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6380".to_string());
    state.conveyor = Some(arcodash_core::query_exec::build_producer_conveyor(
        state.db.clone(),
        state.redis.clone(),
        redis_url,
    ));
    Some(spawn_with_state(state, serialization_lock).await)
}

/// Connects to the test DB/Redis, runs migrations, and builds an `AppState`; `None` when
/// `TEST_DATABASE_URL` is unset.
async fn build_test_state(
    mutate_config: impl FnOnce(&mut arcodash_core::Config),
) -> Option<(AppState, sea_orm::DatabaseConnection)> {
    let db_url = std::env::var("TEST_DATABASE_URL").ok()?;
    let redis_url =
        std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6380".to_string());

    // `--test-threads=1` only serializes within one binary; this session-level advisory lock
    // serializes across binaries, acquired before any table mutation and held on TestApp until
    // dropped.
    use sea_orm::ConnectionTrait;
    let mut lock_opts = sea_orm::ConnectOptions::new(db_url.clone());
    lock_opts.max_connections(1).min_connections(1);
    let serialization_lock = sea_orm::Database::connect(lock_opts)
        .await
        .expect("connect test serialization lock");
    serialization_lock
        .execute_unprepared("SELECT pg_advisory_lock(4021957)")
        .await
        .expect("acquire test serialization lock");

    let db = arcodash_core::db::connect(&db_url)
        .await
        .expect("cannot connect to test DB");

    use sea_orm_migration::MigratorTrait;
    arcodash_migration::Migrator::up(&db, None)
        .await
        .expect("migrations failed");

    seed_admin_user(&db).await;

    let redis = arcodash_core::valkey_client::connect(&redis_url)
        .await
        .expect("cannot connect to test Redis");

    let mut config = test_config(db_url, redis_url);
    mutate_config(&mut config);
    let store = test_object_store().await;
    let connectors = Arc::new(
        arcodash_core::connectors::registry::ConnectorRegistry::with_db(db.clone(), store.clone()),
    );
    let queue =
        arcodash_core::queue::build_queue(&config, redis.clone()).expect("failed to build queue");

    Some((
        AppState {
            db: db.clone(),
            conveyor_db: db.clone(),
            redis,
            config,
            connectors,
            store,
            queue,
            conveyor: None,
            mailer: None,
            http: reqwest::Client::new(),
            org_resolver: None,
            request_txn_hook: None,
            public_dashboard_access: PublicDashboardAccess::Anonymous,
            embed_default_expiry: None,
            org_access_check: None,
            entitlements: Arc::new(arcodash_core::entitlements::Unrestricted),
            reserved_org_settings_keys: Arc::new(Vec::new()),
            external_auth: None,
        },
        serialization_lock,
    ))
}

/// Binds the app built from `state` on port 0 and spawns it; `serialization_lock` stays held on the
/// returned `TestApp`.
async fn spawn_with_state(
    state: AppState,
    serialization_lock: sea_orm::DatabaseConnection,
) -> TestApp {
    spawn_with_state_ext(state, serialization_lock, AppExtensions::default()).await
}

async fn spawn_with_state_ext(
    state: AppState,
    serialization_lock: sea_orm::DatabaseConnection,
    ext: AppExtensions,
) -> TestApp {
    let db = state.db.clone();
    let redis = state.redis.clone();
    let state_for_test_app = state.clone();

    let cors = CorsLayer::new()
        .allow_origin(HeaderValue::from_static("http://localhost:3000"))
        .allow_credentials(true)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        .expose_headers([header::HeaderName::from_static("x-total-count")]);

    let app = build_app(state, cors, ext);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port 0 failed");
    let port = listener.local_addr().unwrap().port();
    let address = format!("http://127.0.0.1:{port}");

    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("axum serve error");
    });

    TestApp {
        address,
        db,
        state: state_for_test_app,
        redis,
        _serialization_lock: serialization_lock,
    }
}

/// Build a `reqwest::Client` sending session cookie + CSRF header on every request. Deliberately no
/// cookie jar: cookies are injected as a static `Cookie` header so rotated csrf_token cookies are
/// ignored and the pair always matches.
#[allow(dead_code)]
pub fn authed_client(_base: &str, session_cookie: &str, csrf_token: &str) -> reqwest::Client {
    let mut default_headers = reqwest::header::HeaderMap::new();
    default_headers.insert(
        reqwest::header::COOKIE,
        reqwest::header::HeaderValue::from_str(&format!(
            "arcodash_session={session_cookie}; csrf_token={csrf_token}"
        ))
        .unwrap(),
    );
    default_headers.insert(
        "X-CSRF-Token",
        reqwest::header::HeaderValue::from_str(csrf_token).unwrap(),
    );
    reqwest::Client::builder()
        .cookie_store(false)
        .default_headers(default_headers)
        .build()
        .unwrap()
}

/// Extract a named cookie's value from `Set-Cookie` response headers.
pub fn extract_set_cookie(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    for v in headers.get_all("set-cookie") {
        if let Ok(s) = v.to_str() {
            for part in s.split(';') {
                let part = part.trim();
                if let Some(rest) = part.strip_prefix(name) {
                    if let Some(val) = rest.strip_prefix('=') {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }
    None
}

// Capability-ceiling authz helpers shared by the `integration_authz_*` suites: users whose
// permissions come from a role.

/// Asserts a 200 and parses the body as JSON; `ctx` labels the failure message.
#[allow(dead_code)]
pub async fn ok_json(resp: reqwest::Response, ctx: &str) -> serde_json::Value {
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "{ctx}: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{ctx} body not JSON ({e}): {body}"))
}

/// Id of the seeded `default` org.
#[allow(dead_code)]
pub async fn default_org_id(db: &sea_orm::DatabaseConnection) -> i64 {
    use arcodash_core::entities::organization;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(db)
        .await
        .expect("org query failed")
        .expect("default org must exist")
        .id
}

/// A client that authenticates every request with `Authorization: Key <api_key>`.
#[allow(dead_code)]
pub fn api_key_client(api_key: &str) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Key {api_key}").parse().unwrap(),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

/// Insert a fresh org with a random slug, isolated from "default" and concurrent tests.
#[allow(dead_code)]
pub async fn make_org(db: &sea_orm::DatabaseConnection) -> i64 {
    use arcodash_core::entities::organization;
    use sea_orm::{ActiveValue::*, EntityTrait};

    organization::Entity::insert(organization::ActiveModel {
        id: NotSet,
        name: Set("Authz Test Org".to_string()),
        slug: Set(format!("authz-test-{}", uuid::Uuid::new_v4().simple())),
        settings: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    })
    .exec(db)
    .await
    .expect("org insert failed")
    .last_insert_id
}

/// The plaintext API key for a user created by these helpers (derived from the unique email, since
/// only the digest is stored).
#[allow(dead_code)]
pub fn test_api_key(email: &str) -> String {
    format!("testkey-{email}")
}

/// Build a user in `org_id` whose sole group is linked to a fresh custom role carrying exactly
/// `perms`; returns `(user, role_id)`. Mutate the role's `permissions` to change capabilities
/// mid-test (no caching).
#[allow(dead_code)]
pub async fn user_with_role(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    perms: &[&str],
) -> (arcodash_core::entities::user::Model, i64) {
    use arcodash_core::entities::{group, group_role, role, user};
    use sea_orm::{ActiveValue::*, EntityTrait};

    let role_id = role::Entity::insert(role::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set(format!("role-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(perms.iter().map(|s| s.to_string()).collect()),
        r#type: Set("custom".to_string()),
        created_at: NotSet,
        updated_at: NotSet,
    })
    .exec(db)
    .await
    .expect("role insert failed")
    .last_insert_id;

    let group_id = group::Entity::insert(group::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set(format!("group-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
        created_at: NotSet,
        updated_at: NotSet,
        data_source_access_restricted: Set(false),
    })
    .exec(db)
    .await
    .expect("group insert failed")
    .last_insert_id;

    group_role::Entity::insert(group_role::ActiveModel {
        id: NotSet,
        group_id: Set(group_id),
        role_id: Set(role_id),
        created_at: NotSet,
        updated_at: NotSet,
    })
    .exec(db)
    .await
    .expect("group_role insert failed");

    let email = format!("authz-test-{}@example.com", uuid::Uuid::new_v4().simple());
    let user_id = user::Entity::insert(user::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set("Authz Test User".to_string()),
        email: Set(email.clone()),
        password_hash: Set(None),
        groups: Set(vec![group_id]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(&test_api_key(&email))),
        disabled_at: Set(None),
        is_invitation_pending: Set(false),
        profile_image_url: Set(None),
        details: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
        digest_enabled: Set(false),
        digest_unsubscribe_token: Set(None),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("user insert failed")
    .last_insert_id;

    let user = user::Entity::find_by_id(user_id)
        .one(db)
        .await
        .expect("user query failed")
        .expect("user must exist after insert");
    (user, role_id)
}

/// Overwrite an existing role's permission set (e.g. to downgrade a user mid-test).
#[allow(dead_code)]
pub async fn set_role_permissions(db: &sea_orm::DatabaseConnection, role_id: i64, perms: &[&str]) {
    use arcodash_core::entities::role;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

    let existing = role::Entity::find_by_id(role_id)
        .one(db)
        .await
        .expect("role query failed")
        .expect("role must exist");
    let mut active: role::ActiveModel = existing.into();
    active.permissions = Set(perms.iter().map(|s| s.to_string()).collect());
    active.update(db).await.expect("role update failed");
}

/// Skip a test when `spawn_app()` returned `None` (no TEST_DATABASE_URL).
#[macro_export]
macro_rules! skip_if_none {
    ($opt:expr) => {
        match $opt {
            Some(v) => v,
            None => {
                eprintln!("[SKIP] TEST_DATABASE_URL not set — skipping integration test");
                return;
            }
        }
    };
}

/// Placeholder emulator address so `ObjectStore::from_env()` succeeds for the `ConnectorRegistry`
/// without a real emulator (the upload tests set real values).
async fn test_object_store() -> std::sync::Arc<arcodash_core::storage::ObjectStore> {
    if std::env::var("UPLOAD_BUCKET").is_err() {
        std::env::set_var("UPLOAD_BUCKET", "arcodash-uploads-test");
    }
    if std::env::var("STORAGE_EMULATOR_HOST").is_err() {
        // docker-compose.testing.yml maps the fake-gcs-server's 4443 to host port 4444.
        std::env::set_var("STORAGE_EMULATOR_HOST", "http://127.0.0.1:4444");
    }
    let store = arcodash_core::storage::ObjectStore::from_env()
        .expect("failed to initialize object storage");
    // The emulator's `-backend memory` starts with no buckets, so every spawn ensures it exists; a
    // 409 is a no-op.
    store
        .ensure_bucket()
        .await
        .expect("failed to ensure test upload bucket exists");
    store
}

fn test_config(db_url: String, valkey_url: String) -> arcodash_core::Config {
    arcodash_core::Config {
        app_host: "127.0.0.1".parse().unwrap(),
        app_port: 0,
        database_url: db_url,
        valkey_url,
        rust_log: "error".to_string(),
        base_url: "http://localhost:3000".to_string(),
        encryption_key: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .to_string(),
        cookie_secret: "integration-test-cookie-secret".to_string(),
        disable_public_urls: false,
        worker_queues: arcodash_core::queue::QueueName::ALL.to_vec(),
        fast_concurrency: 1,
        long_concurrency: 1,
        long_drain_secs: 1,
        smtp_host: None,
        smtp_port: 587,
        smtp_username: None,
        smtp_password: None,
        mail_from: "test@localhost".to_string(),
        smtp_tls: "wrapper".to_string(),
        remote_user_header: None,
        trusted_proxy_hops: 0,
        saml_idp_metadata_xml: None,
        saml_sp_entity_id: None,
        ldap_url: None,
        ldap_bind_dn: None,
        ldap_bind_password: None,
        ldap_search_base_dn: None,
        ldap_user_filter: "(uid={username})".to_string(),
        ldap_email_attribute: "mail".to_string(),
        ldap_first_name_attribute: "givenName".to_string(),
        ldap_last_name_attribute: "sn".to_string(),
        ldap_group_attribute: None,
        gemini_api_key: None,
        openai_api_key: None,
        anthropic_api_key: None,
        ai_rate_limit_per_min: 10,
        gemini_model: "gemini-2.0-flash".to_string(),
        openai_model: "gpt-5.6-terra".to_string(),
        anthropic_model: "claude-sonnet-5-5".to_string(),
        query_timeout_secs: 300,
        max_query_rows: 10_000,
    }
}

async fn seed_admin_user(db: &sea_orm::DatabaseConnection) {
    use arcodash_core::entities::{group, organization, user};
    use sea_orm::{sea_query::OnConflict, ActiveValue::*, ColumnTrait, EntityTrait, QueryFilter};

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(db)
        .await
        .expect("org query failed")
        .expect("default org must exist after migrations");

    let admin_group = group::Entity::find()
        .filter(group::Column::OrgId.eq(org.id))
        .filter(group::Column::Name.eq("admin"))
        .one(db)
        .await
        .expect("group query failed")
        .expect("admin group must exist after migrations");

    let hash = {
        use argon2::{Argon2, PasswordHasher};
        Argon2::default()
            .hash_password(ADMIN_PASSWORD.as_bytes())
            .expect("hash failed")
            .to_string()
    };

    let api_key = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );

    // ON CONFLICT avoids the find-then-insert TOCTOU race (23505) under parallel tests, and
    // restores password_hash so a prior password-changing test can't lock later tests out.
    let result = user::Entity::insert(user::ActiveModel {
        id: NotSet,
        org_id: Set(org.id),
        name: Set("Test Admin".to_string()),
        email: Set(ADMIN_EMAIL.to_string()),
        password_hash: Set(Some(hash)),
        groups: Set(vec![admin_group.id]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(&api_key)),
        disabled_at: Set(None),
        is_invitation_pending: Set(false),
        profile_image_url: Set(None),
        details: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
        digest_enabled: Set(false),
        digest_unsubscribe_token: Set(None),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::columns([user::Column::OrgId, user::Column::Email])
            .update_columns([
                user::Column::PasswordHash,
                user::Column::DisabledAt,
                user::Column::Groups,
            ])
            .to_owned(),
    )
    .exec(db)
    .await;
    match result {
        Ok(_) => {}
        Err(e) => panic!("admin user seed failed: {e}"),
    }
}

/// A `pg` data source aimed at a closed loopback port, so any live lookup fails. Creation validates
/// the host, so the private-address opt-in is set for that one call and cleared even on panic.
#[allow(dead_code)] // used by some integration test files only
pub async fn create_unroutable_pg_ds(client: &reqwest::Client, base: &str) -> i64 {
    struct OptIn;
    impl OptIn {
        fn set() -> Self {
            std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
            OptIn
        }
    }
    impl Drop for OptIn {
        fn drop(&mut self) {
            std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
        }
    }

    let _guard = OptIn::set();
    let resp = client
        .post(format!("{base}/api/data_sources"))
        .json(&serde_json::json!({
            "name": "unroutable-ds",
            "type": "pg",
            "options": { "host": "127.0.0.1", "port": 1, "dbname": "x", "user": "x", "password": "x" }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "create unroutable ds: {body}");
    serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_i64()
        .unwrap()
}
