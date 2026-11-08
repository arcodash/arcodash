//! Integration tests for the fallback chain (`ai_query::run_fallback_chain`) with an in-test fake
//! `AiProvider`: retries/no-retries and key-health/usage bookkeeping, no network calls.

mod common;
use common::ok_json;
use common::{spawn_app, TestApp};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arcodash_core::entities::{ai_usage, organization};
use arcodash_core::test_support::new_organization;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

use arcodash_backend::api::handlers::ai::describe::SchemaDescriptions;
use arcodash_backend::api::handlers::ai::monitor_suggestions::MonitorSuggestionGeneration;
use arcodash_backend::api::handlers::ai::viz::{ChartOptions, VizGeneration, VizSuggestion};
use arcodash_backend::api::handlers::ai::{AiError, AiProvider, AiProviderKind, SqlGeneration};
use arcodash_backend::api::handlers::ai_query::{check_rate_limit, run_fallback_chain};
use arcodash_backend::api::handlers::ai_settings::ResolvedProvider;
use arcodash_backend::db::RequestScope;

/// A canned outcome a `FakeProvider` returns (the real `Result` isn't `Clone`).
#[derive(Debug, Clone, Copy)]
enum FakeOutcome {
    Success,
    InvalidKey,
    QuotaExceeded,
    Network,
    GenerationFailed,
}

impl FakeOutcome {
    fn into_result(self) -> Result<SqlGeneration, AiError> {
        match self {
            FakeOutcome::Success => Ok(SqlGeneration {
                sql: "SELECT 1".to_string(),
                explanation: Some("test".to_string()),
                tokens_in: Some(10),
                tokens_out: Some(5),
            }),
            FakeOutcome::InvalidKey => Err(AiError::InvalidKey),
            FakeOutcome::QuotaExceeded => Err(AiError::QuotaExceeded { free_tier: false }),
            FakeOutcome::Network => Err(AiError::Network("connection reset".to_string())),
            FakeOutcome::GenerationFailed => Err(AiError::GenerationFailed("bad json".to_string())),
        }
    }

    fn into_viz_result(self) -> Result<VizGeneration, AiError> {
        match self {
            FakeOutcome::Success => Ok(VizGeneration {
                title: Some("Fake viz".to_string()),
                suggestion: VizSuggestion::Chart(ChartOptions {
                    global_series_type: Some("bar".to_string()),
                    ..Default::default()
                }),
                tokens_in: Some(10),
                tokens_out: Some(5),
            }),
            FakeOutcome::InvalidKey => Err(AiError::InvalidKey),
            FakeOutcome::QuotaExceeded => Err(AiError::QuotaExceeded { free_tier: false }),
            FakeOutcome::Network => Err(AiError::Network("connection reset".to_string())),
            FakeOutcome::GenerationFailed => Err(AiError::GenerationFailed("bad json".to_string())),
        }
    }

    fn into_describe_result(self) -> Result<SchemaDescriptions, AiError> {
        match self {
            FakeOutcome::Success => Ok(SchemaDescriptions {
                tokens_in: Some(10),
                tokens_out: Some(5),
                ..Default::default()
            }),
            FakeOutcome::InvalidKey => Err(AiError::InvalidKey),
            FakeOutcome::QuotaExceeded => Err(AiError::QuotaExceeded { free_tier: false }),
            FakeOutcome::Network => Err(AiError::Network("connection reset".to_string())),
            FakeOutcome::GenerationFailed => Err(AiError::GenerationFailed("bad json".to_string())),
        }
    }

    fn into_suggest_result(self) -> Result<MonitorSuggestionGeneration, AiError> {
        match self {
            FakeOutcome::Success => Ok(MonitorSuggestionGeneration {
                suggestions: Vec::new(),
                tokens_in: Some(10),
                tokens_out: Some(5),
            }),
            FakeOutcome::InvalidKey => Err(AiError::InvalidKey),
            FakeOutcome::QuotaExceeded => Err(AiError::QuotaExceeded { free_tier: false }),
            FakeOutcome::Network => Err(AiError::Network("connection reset".to_string())),
            FakeOutcome::GenerationFailed => Err(AiError::GenerationFailed("bad json".to_string())),
        }
    }
}

/// Stands in for a real provider; records that it was called.
struct FakeProvider {
    kind: AiProviderKind,
    outcome: FakeOutcome,
    calls: Arc<Mutex<Vec<AiProviderKind>>>,
}

#[async_trait::async_trait]
impl AiProvider for FakeProvider {
    fn kind(&self) -> AiProviderKind {
        self.kind
    }
    fn default_model(&self) -> &'static str {
        "fake-model"
    }
    fn curated_models(&self) -> &'static [&'static str] {
        &[]
    }
    async fn test_key(
        &self,
        _http: &reqwest::Client,
        _api_key: &str,
        _model: &str,
    ) -> Result<(), AiError> {
        unimplemented!("not exercised by these tests")
    }
    async fn generate_sql(
        &self,
        _http: &reqwest::Client,
        _api_key: &str,
        _model: &str,
        _prompt: &str,
    ) -> Result<SqlGeneration, AiError> {
        self.calls.lock().unwrap().push(self.kind);
        self.outcome.into_result()
    }
    async fn suggest_visualization(
        &self,
        _http: &reqwest::Client,
        _api_key: &str,
        _model: &str,
        _prompt: &str,
    ) -> Result<VizGeneration, AiError> {
        self.calls.lock().unwrap().push(self.kind);
        self.outcome.into_viz_result()
    }
    async fn suggest_monitors(
        &self,
        _http: &reqwest::Client,
        _api_key: &str,
        _model: &str,
        _prompt: &str,
    ) -> Result<MonitorSuggestionGeneration, AiError> {
        self.calls.lock().unwrap().push(self.kind);
        self.outcome.into_suggest_result()
    }
    async fn describe_schema(
        &self,
        _http: &reqwest::Client,
        _api_key: &str,
        _model: &str,
        _prompt: &str,
    ) -> Result<SchemaDescriptions, AiError> {
        self.calls.lock().unwrap().push(self.kind);
        self.outcome.into_describe_result()
    }
}

/// Builds a `provider_lookup` closure resolving each kind to a `FakeProvider` sharing one `calls`
/// log.
fn fake_lookup(
    outcomes: HashMap<AiProviderKind, FakeOutcome>,
    calls: Arc<Mutex<Vec<AiProviderKind>>>,
) -> impl Fn(AiProviderKind, Option<String>) -> Box<dyn AiProvider> {
    move |kind, _base_url| {
        let outcome = *outcomes
            .get(&kind)
            .unwrap_or_else(|| panic!("test didn't configure an outcome for {kind:?}"));
        Box::new(FakeProvider {
            kind,
            outcome,
            calls: calls.clone(),
        })
    }
}

fn two_provider_chain() -> Vec<ResolvedProvider> {
    vec![
        ResolvedProvider {
            kind: AiProviderKind::Gemini,
            api_key: "gemini-key".to_string(),
            model: Some("gemini-model".to_string()),
            base_url: None,
        },
        ResolvedProvider {
            kind: AiProviderKind::OpenAi,
            api_key: "openai-key".to_string(),
            model: Some("openai-model".to_string()),
            base_url: None,
        },
    ]
}

/// A fresh org row with a random slug (`TestApp::cleanup()` doesn't truncate `organizations`).
async fn make_org(db: &sea_orm::DatabaseConnection) -> organization::Model {
    let mut org = new_organization();
    org.slug = Set(format!("ai-fallback-{}", uuid::Uuid::new_v4().simple()));
    org.insert(db).await.expect("insert org")
}

/// (a) Fallback disabled: only the primary is tried, even on a fallback-eligible error.
#[tokio::test]
async fn fallback_disabled_only_tries_primary_even_on_failure() {
    let app = skip_if_none!(spawn_app().await);
    let org = make_org(&app.db).await;
    let scope = RequestScope::pooled(app.db.clone());
    let http = reqwest::Client::new();

    let calls = Arc::new(Mutex::new(Vec::new()));
    let outcomes = HashMap::from([
        (AiProviderKind::Gemini, FakeOutcome::InvalidKey),
        (AiProviderKind::OpenAi, FakeOutcome::Success),
    ]);

    let (served, outcome) = run_fallback_chain(
        &scope,
        &org,
        None,
        &http,
        &two_provider_chain(),
        false,
        "sql_generation",
        "prompt",
        fake_lookup(outcomes, calls.clone()),
        |provider, http, api_key, model, prompt| {
            provider.generate_sql(http, api_key, model, prompt)
        },
        |gen: &SqlGeneration| (gen.tokens_in, gen.tokens_out),
    )
    .await;

    assert_eq!(served, AiProviderKind::Gemini);
    assert!(matches!(outcome, Err(AiError::InvalidKey)));
    assert_eq!(*calls.lock().unwrap(), vec![AiProviderKind::Gemini]);
}

/// (b) Fallback enabled, primary fails `InvalidKey`, secondary succeeds: serves from the secondary
/// and both get a `last_status` write.
#[tokio::test]
async fn fallback_enabled_retries_on_invalid_key_and_records_both_statuses() {
    let app = skip_if_none!(spawn_app().await);
    let org = make_org(&app.db).await;
    let scope = RequestScope::pooled(app.db.clone());
    let http = reqwest::Client::new();

    let calls = Arc::new(Mutex::new(Vec::new()));
    let outcomes = HashMap::from([
        (AiProviderKind::Gemini, FakeOutcome::InvalidKey),
        (AiProviderKind::OpenAi, FakeOutcome::Success),
    ]);

    let (served, outcome) = run_fallback_chain(
        &scope,
        &org,
        None,
        &http,
        &two_provider_chain(),
        true,
        "sql_generation",
        "prompt",
        fake_lookup(outcomes, calls.clone()),
        |provider, http, api_key, model, prompt| {
            provider.generate_sql(http, api_key, model, prompt)
        },
        |gen: &SqlGeneration| (gen.tokens_in, gen.tokens_out),
    )
    .await;

    assert_eq!(served, AiProviderKind::OpenAi);
    assert!(outcome.is_ok());
    assert_eq!(
        *calls.lock().unwrap(),
        vec![AiProviderKind::Gemini, AiProviderKind::OpenAi]
    );

    let reloaded = organization::Entity::find_by_id(org.id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("org still exists");
    assert_eq!(
        reloaded.settings["ai"]["providers"]["gemini"]["last_status"],
        "invalid_key"
    );
    assert_eq!(
        reloaded.settings["ai"]["providers"]["openai"]["last_status"],
        "ok"
    );
}

/// (c) `Network`/`GenerationFailed` are not fallback-eligible: the chain stops at the primary.
#[tokio::test]
async fn fallback_enabled_does_not_retry_on_network_or_generation_failed() {
    for failure in [FakeOutcome::Network, FakeOutcome::GenerationFailed] {
        let app = skip_if_none!(spawn_app().await);
        let org = make_org(&app.db).await;
        let scope = RequestScope::pooled(app.db.clone());
        let http = reqwest::Client::new();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let outcomes = HashMap::from([
            (AiProviderKind::Gemini, failure),
            (AiProviderKind::OpenAi, FakeOutcome::Success),
        ]);

        let (served, outcome) = run_fallback_chain(
            &scope,
            &org,
            None,
            &http,
            &two_provider_chain(),
            true,
            "sql_generation",
            "prompt",
            fake_lookup(outcomes, calls.clone()),
            |provider, http, api_key, model, prompt| {
                provider.generate_sql(http, api_key, model, prompt)
            },
            |gen: &SqlGeneration| (gen.tokens_in, gen.tokens_out),
        )
        .await;

        assert_eq!(served, AiProviderKind::Gemini, "failure case: {failure:?}");
        assert!(outcome.is_err(), "failure case: {failure:?}");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![AiProviderKind::Gemini],
            "must not attempt the secondary for {failure:?}"
        );
    }
}

/// (d) Every entry fails: the response is the LAST entry's outcome and every entry was
/// attempted/recorded.
#[tokio::test]
async fn fallback_enabled_all_fail_returns_last_entrys_error() {
    let app = skip_if_none!(spawn_app().await);
    let org = make_org(&app.db).await;
    let scope = RequestScope::pooled(app.db.clone());
    let http = reqwest::Client::new();

    let calls = Arc::new(Mutex::new(Vec::new()));
    let outcomes = HashMap::from([
        (AiProviderKind::Gemini, FakeOutcome::QuotaExceeded),
        (AiProviderKind::OpenAi, FakeOutcome::InvalidKey),
    ]);

    let (served, outcome) = run_fallback_chain(
        &scope,
        &org,
        None,
        &http,
        &two_provider_chain(),
        true,
        "sql_generation",
        "prompt",
        fake_lookup(outcomes, calls.clone()),
        |provider, http, api_key, model, prompt| {
            provider.generate_sql(http, api_key, model, prompt)
        },
        |gen: &SqlGeneration| (gen.tokens_in, gen.tokens_out),
    )
    .await;

    assert_eq!(served, AiProviderKind::OpenAi);
    assert!(matches!(outcome, Err(AiError::InvalidKey)));
    assert_eq!(
        *calls.lock().unwrap(),
        vec![AiProviderKind::Gemini, AiProviderKind::OpenAi]
    );

    let reloaded = organization::Entity::find_by_id(org.id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("org still exists");
    assert_eq!(
        reloaded.settings["ai"]["providers"]["gemini"]["last_status"],
        "quota_exceeded"
    );
    assert_eq!(
        reloaded.settings["ai"]["providers"]["openai"]["last_status"],
        "invalid_key"
    );
}

/// The rate limit is charged once per request regardless of fallback attempts (`check_rate_limit`
/// is outside `run_fallback_chain`).
#[tokio::test]
async fn rate_limiter_charged_once_despite_multiple_fallback_attempts() {
    let app = skip_if_none!(spawn_app().await);
    let org = make_org(&app.db).await;
    let scope = RequestScope::pooled(app.db.clone());
    let http = reqwest::Client::new();

    let mut redis = app.redis.clone();
    let user_id = unique_key_component();

    let charge = check_rate_limit(&mut redis, org.id, user_id, 5)
        .await
        .expect("redis must be reachable");
    assert!(charge.is_none(), "first call must not be rate-limited");

    let calls = Arc::new(Mutex::new(Vec::new()));
    let outcomes = HashMap::from([
        (AiProviderKind::Gemini, FakeOutcome::QuotaExceeded),
        (AiProviderKind::OpenAi, FakeOutcome::Success),
    ]);
    let _ = run_fallback_chain(
        &scope,
        &org,
        None,
        &http,
        &two_provider_chain(),
        true,
        "sql_generation",
        "prompt",
        fake_lookup(outcomes, calls.clone()),
        |provider, http, api_key, model, prompt| {
            provider.generate_sql(http, api_key, model, prompt)
        },
        |gen: &SqlGeneration| (gen.tokens_in, gen.tokens_out),
    )
    .await;
    assert_eq!(
        *calls.lock().unwrap(),
        vec![AiProviderKind::Gemini, AiProviderKind::OpenAi],
        "sanity check: the fallback actually made two attempts"
    );

    let count = ai_rate_limit_counter(&mut redis, org.id, user_id).await;
    assert_eq!(
        count, 1,
        "run_fallback_chain must never touch the rate limiter — it's charged once by generate_sql before any dispatch"
    );
}

/// Recovery of a counter key stuck with no TTL (process death between INCR and EXPIRE), which
/// `EXPIRE ... NX` must repair; the key is SET manually to mimic that.
#[tokio::test]
async fn rate_limiter_self_heals_a_key_stuck_without_a_ttl() {
    let app = skip_if_none!(spawn_app().await);
    let org = make_org(&app.db).await;
    let mut redis = app.redis.clone();
    let user_id = unique_key_component();
    let key = format!("arcodash:ai_rate_limit:{}:{}", org.id, user_id);

    use redis::AsyncCommands;
    let _: () = redis.set(&key, 1).await.expect("redis must be reachable");
    let stuck_ttl: i64 = redis.ttl(&key).await.expect("redis must be reachable");
    assert_eq!(
        stuck_ttl, -1,
        "sanity check: key has no TTL, matching the stuck-counter state"
    );

    let _ = check_rate_limit(&mut redis, org.id, user_id, 5)
        .await
        .expect("redis must be reachable");

    let healed_ttl: i64 = redis.ttl(&key).await.expect("redis must be reachable");
    assert!(
        healed_ttl > 0,
        "a key left without a TTL by a prior failure must get one on the very next call, not \
         stay permanently rate limited; got ttl={healed_ttl}"
    );
}

/// Reads the raw Redis counter `check_rate_limit` maintains (replicating its private key format).
async fn ai_rate_limit_counter(
    redis: &mut redis::aio::ConnectionManager,
    org_id: i64,
    user_id: i64,
) -> i64 {
    use redis::AsyncCommands;
    redis
        .get::<_, Option<i64>>(format!("arcodash:ai_rate_limit:{org_id}:{user_id}"))
        .await
        .expect("redis must be reachable")
        .unwrap_or(0)
}

/// Distinct per test-process value so tests never collide on a counter key.
fn unique_key_component() -> i64 {
    static COUNTER: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    nanos.wrapping_add(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// VIZ-path mirror of the fallback retry test: each attempt gets its own `auto_visualize` usage
/// row.
#[tokio::test]
async fn viz_path_serves_via_fallback_records_status_and_meters_auto_visualize() {
    let app = skip_if_none!(spawn_app().await);
    let org = make_org(&app.db).await;
    let scope = RequestScope::pooled(app.db.clone());
    let http = reqwest::Client::new();

    let calls = Arc::new(Mutex::new(Vec::new()));
    let outcomes = HashMap::from([
        (AiProviderKind::Gemini, FakeOutcome::InvalidKey),
        (AiProviderKind::OpenAi, FakeOutcome::Success),
    ]);

    let (served, outcome) = run_fallback_chain(
        &scope,
        &org,
        None,
        &http,
        &two_provider_chain(),
        true,
        "auto_visualize",
        "prompt",
        fake_lookup(outcomes, calls.clone()),
        |provider, http, api_key, model, prompt| {
            provider.suggest_visualization(http, api_key, model, prompt)
        },
        |gen: &VizGeneration| (gen.tokens_in, gen.tokens_out),
    )
    .await;

    assert_eq!(served, AiProviderKind::OpenAi);
    assert!(outcome.is_ok());
    assert_eq!(
        *calls.lock().unwrap(),
        vec![AiProviderKind::Gemini, AiProviderKind::OpenAi]
    );

    let reloaded = organization::Entity::find_by_id(org.id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("org still exists");
    assert_eq!(
        reloaded.settings["ai"]["providers"]["gemini"]["last_status"],
        "invalid_key"
    );
    assert_eq!(
        reloaded.settings["ai"]["providers"]["openai"]["last_status"],
        "ok"
    );

    let usage_rows = ai_usage::Entity::find()
        .filter(ai_usage::Column::OrgId.eq(org.id))
        .all(&app.db)
        .await
        .unwrap();
    let served_row = usage_rows
        .iter()
        .find(|r| r.feature == "auto_visualize" && r.provider == "openai")
        .expect("a usage row for the serving provider must exist");
    assert_eq!(served_row.status, "ok");
    assert_eq!(served_row.tokens_in, Some(10));
    assert_eq!(served_row.tokens_out, Some(5));
    assert!(
        usage_rows.iter().any(|r| r.feature == "auto_visualize"
            && r.provider == "gemini"
            && r.status == "invalid_key"),
        "the failed attempt must also be metered: {usage_rows:#?}"
    );
}

/// VIZ-path mirror: a bad visualization answer must not trigger a fallback hop.
#[tokio::test]
async fn viz_path_does_not_retry_on_generation_failed() {
    let app = skip_if_none!(spawn_app().await);
    let org = make_org(&app.db).await;
    let scope = RequestScope::pooled(app.db.clone());
    let http = reqwest::Client::new();

    let calls = Arc::new(Mutex::new(Vec::new()));
    let outcomes = HashMap::from([
        (AiProviderKind::Gemini, FakeOutcome::GenerationFailed),
        (AiProviderKind::OpenAi, FakeOutcome::Success),
    ]);

    let (served, outcome) = run_fallback_chain(
        &scope,
        &org,
        None,
        &http,
        &two_provider_chain(),
        true,
        "auto_visualize",
        "prompt",
        fake_lookup(outcomes, calls.clone()),
        |provider, http, api_key, model, prompt| {
            provider.suggest_visualization(http, api_key, model, prompt)
        },
        |gen: &VizGeneration| (gen.tokens_in, gen.tokens_out),
    )
    .await;

    assert_eq!(served, AiProviderKind::Gemini);
    assert!(outcome.is_err());
    assert_eq!(*calls.lock().unwrap(), vec![AiProviderKind::Gemini]);
}

/// VIZ-path mirror: the per-(org,user) limiter is charged once per request regardless of fallback
/// attempts.
#[tokio::test]
async fn viz_rate_limiter_charged_once_around_viz_fallback() {
    let app = skip_if_none!(spawn_app().await);
    let org = make_org(&app.db).await;
    let scope = RequestScope::pooled(app.db.clone());
    let http = reqwest::Client::new();

    let mut redis = app.redis.clone();
    let user_id = unique_key_component();

    let charge = check_rate_limit(&mut redis, org.id, user_id, 5)
        .await
        .expect("redis must be reachable");
    assert!(charge.is_none(), "first call must not be rate-limited");

    let calls = Arc::new(Mutex::new(Vec::new()));
    let outcomes = HashMap::from([
        (AiProviderKind::Gemini, FakeOutcome::QuotaExceeded),
        (AiProviderKind::OpenAi, FakeOutcome::Success),
    ]);
    let _ = run_fallback_chain(
        &scope,
        &org,
        None,
        &http,
        &two_provider_chain(),
        true,
        "auto_visualize",
        "prompt",
        fake_lookup(outcomes, calls.clone()),
        |provider, http, api_key, model, prompt| {
            provider.suggest_visualization(http, api_key, model, prompt)
        },
        |gen: &VizGeneration| (gen.tokens_in, gen.tokens_out),
    )
    .await;
    assert_eq!(
        *calls.lock().unwrap(),
        vec![AiProviderKind::Gemini, AiProviderKind::OpenAi],
        "sanity check: the fallback actually made two attempts"
    );

    let count = ai_rate_limit_counter(&mut redis, org.id, user_id).await;
    assert_eq!(
        count, 1,
        "run_fallback_chain must never touch the rate limiter for the viz path either"
    );
}

/// Two backend replicas (independent `ConnectionManager`s on the same Redis) must share one cap per
/// (org, user).
#[tokio::test]
async fn ai_rate_limit_is_shared_across_independent_connection_managers() {
    let redis_url =
        std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6380".to_string());
    let Ok(replica_a) = arcodash_core::valkey_client::connect(&redis_url).await else {
        eprintln!("[SKIP] TEST_REDIS_URL unreachable — skipping integration test");
        return;
    };
    let Ok(mut replica_b) = arcodash_core::valkey_client::connect(&redis_url).await else {
        eprintln!("[SKIP] TEST_REDIS_URL unreachable — skipping integration test");
        return;
    };

    let org_id = unique_key_component();
    let user_id = unique_key_component();
    let limit = 3u32;

    // 3 charges split across the two replicas must exhaust the shared cap...
    for mut replica in [replica_a.clone(), replica_b.clone(), replica_a.clone()] {
        let charge = check_rate_limit(&mut replica, org_id, user_id, limit)
            .await
            .expect("redis must be reachable");
        assert!(
            charge.is_none(),
            "charge must succeed while under the shared limit of {limit}"
        );
    }

    // ...so the 4th, from the other replica, is rejected.
    let over = check_rate_limit(&mut replica_b, org_id, user_id, limit)
        .await
        .expect("redis must be reachable");
    assert!(
        over.is_some(),
        "the 4th charge across two replicas must exceed the shared limit of {limit}"
    );
}

// HTTP-level: provider-not-configured beats rate-limiting. Both handlers check
// `effective.provider.is_none()` before `check_rate_limit`, so a rejected request never costs a
// rate-limit slot.

/// A `pg` data source with no real connection; enough for the access check since the connector is
/// never reached.
async fn create_ds_fake(client: &reqwest::Client, base: &str) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/data_sources"))
            .json(&serde_json::json!({ "name": "fake-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create fake ds",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

/// Test config's `AI_RATE_LIMIT_PER_MIN` (see `common/mod.rs::test_config`).
const AI_RATE_LIMIT_PER_MIN: usize = 10;

#[tokio::test]
async fn generate_sql_no_provider_configured_never_rate_limited() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds_fake(&admin, &app.address).await;

    for i in 0..=AI_RATE_LIMIT_PER_MIN {
        let resp = admin
            .post(format!("{}/api/queries/generate_sql", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "question": "how many orders were placed?"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "call {i}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body["status"], "not_configured",
            "call {i} must stay not_configured, never rate_limited: {body}"
        );
    }
}

#[tokio::test]
async fn suggest_visualization_no_provider_configured_never_rate_limited() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    for i in 0..=AI_RATE_LIMIT_PER_MIN {
        let resp = admin
            .post(format!("{}/api/queries/suggest_visualization", app.address))
            .json(&serde_json::json!({
                "columns": [{ "name": "total", "friendly_type": "number" }],
                "row_count": 3
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "call {i}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body["status"], "not_configured",
            "call {i} must stay not_configured, never rate_limited: {body}"
        );
    }
}

// HTTP-level: the data source points at no reachable database, so a request that still serves
// proves the handler never called `connector.get_schema`.

/// Stub OpenAI-compatible server that records each prompt; returns `(base_url, captured_prompts)`.
async fn spawn_capturing_stub() -> (String, Arc<Mutex<Vec<String>>>) {
    use axum::{routing::post, Json, Router};

    #[derive(Clone)]
    struct StubState(Arc<Mutex<Vec<String>>>);

    async fn handler(
        axum::extract::State(state): axum::extract::State<StubState>,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let prompt = body["messages"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        state.0.lock().unwrap().push(prompt);
        Json(serde_json::json!({
            "choices": [{ "message": { "content": r#"{"sql":"SELECT 1","explanation":"x"}"# } }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
        }))
    }

    let captured = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new()
        .route("/v1/chat/completions", post(handler))
        .with_state(StubState(captured.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), captured)
}

async fn configure_stub_provider(admin: &reqwest::Client, base: &str, stub_url: &str) {
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let resp = admin
        .post(format!("{base}/api/organization/ai"))
        .json(&serde_json::json!({
            "providers": { "openai": { "api_key": "sk-test-stub-key-0000000", "base_url": stub_url } },
            "provider_order": ["openai"]
        }))
        .send()
        .await
        .unwrap();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    assert_eq!(resp.status().as_u16(), 200, "configure stub provider");
}

async fn cache_schema(app: &TestApp, ds_id: i64, schema: Option<serde_json::Value>) {
    use arcodash_core::entities::data_source;
    let ds = data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: data_source::ActiveModel = ds.into();
    active.schema = Set(schema);
    active.update(&app.db).await.unwrap();
}

async fn reset_limit(app: &TestApp) {
    use redis::AsyncCommands;
    let _: () = app
        .redis
        .clone()
        .del("arcodash:ai_rate_limit:1:1")
        .await
        .unwrap();
}

#[tokio::test]
async fn generate_sql_uses_cached_schema_with_column_types() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_limit(&app).await;
    let ds_id = create_ds_fake(&admin, &app.address).await;
    cache_schema(
        &app,
        ds_id,
        Some(serde_json::json!({ "tables": [{
            "name": "orders",
            "columns": [
                { "name": "id", "type_name": "integer" },
                { "name": "created_at", "type_name": "timestamp with time zone" }
            ]
        }]})),
    )
    .await;
    let (stub_url, captured) = spawn_capturing_stub().await;
    configure_stub_provider(&admin, &app.address, &stub_url).await;

    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let resp = admin
        .post(format!("{}/api/queries/generate_sql", app.address))
        .json(&serde_json::json!({ "data_source_id": ds_id, "question": "orders per day?" }))
        .send()
        .await
        .unwrap();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    let body = ok_json(resp, "generate_sql cached").await;
    assert_eq!(body["status"], "ok", "body: {body}");

    let prompts = captured.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    assert!(
        prompts[0].contains("orders(id integer, created_at timestamp with time zone)"),
        "prompt: {}",
        prompts[0]
    );
    assert!(prompts[0].contains("Rules:"));
}

/// Cache states that must not feed the prompt: each falls through to the live lookup.
async fn assert_generate_sql_goes_live(cache: Option<serde_json::Value>) {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_limit(&app).await;
    let ds_id = common::create_unroutable_pg_ds(&admin, &app.address).await;
    cache_schema(&app, ds_id, cache).await;
    let (stub_url, captured) = spawn_capturing_stub().await;
    configure_stub_provider(&admin, &app.address, &stub_url).await;

    let resp = admin
        .post(format!("{}/api/queries/generate_sql", app.address))
        .json(&serde_json::json!({ "data_source_id": ds_id, "question": "orders per day?" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
    assert!(captured.lock().unwrap().is_empty());
}

#[tokio::test]
async fn generate_sql_without_cached_schema_falls_back_to_live_lookup() {
    assert_generate_sql_goes_live(None).await;
}

#[tokio::test]
async fn generate_sql_with_empty_cached_schema_falls_back_to_live_lookup() {
    assert_generate_sql_goes_live(Some(serde_json::json!({ "tables": [] }))).await;
}

#[tokio::test]
async fn generate_sql_with_unreadable_cached_schema_falls_back_to_live_lookup() {
    assert_generate_sql_goes_live(Some(serde_json::json!({ "tables": "nope" }))).await;
}

/// Caches a schema plus a profile with one recognisable sample value.
async fn cache_schema_and_profile(app: &TestApp, ds_id: i64) {
    use arcodash_core::entities::data_source;
    let now = chrono::SubsecRound::trunc_subsecs(chrono::Utc::now(), 6);
    let ds = data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: data_source::ActiveModel = ds.into();
    active.schema = Set(Some(serde_json::json!({ "tables": [{
        "name": "orders",
        "columns": [{ "name": "status", "type_name": "text" }]
    }]})));
    active.schema_refreshed_at = Set(Some(now.into()));
    active.ai_profile = Set(Some(serde_json::json!({
        "tables": [{ "name": "orders", "sampled_rows": 10, "columns": [{
            "name": "status", "null_frac": 0.0, "distinct_in_sample": 2,
            "samples": ["zebra_shipped_7781", "pending"]
        }]}],
        "relationships": [],
        "schema_refreshed_at_seen": now
    })));
    active.update(&app.db).await.unwrap();
}

async fn generate_and_capture_prompt(
    app: &TestApp,
    admin: &reqwest::Client,
    ds_id: i64,
    captured: &Arc<Mutex<Vec<String>>>,
) -> String {
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let resp = admin
        .post(format!("{}/api/queries/generate_sql", app.address))
        .json(&serde_json::json!({ "data_source_id": ds_id, "question": "shipped orders?" }))
        .send()
        .await
        .unwrap();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    let body = ok_json(resp, "generate_sql profile").await;
    assert_eq!(body["status"], "ok", "body: {body}");
    let prompts = captured.lock().unwrap();
    prompts.last().cloned().expect("prompt captured")
}

async fn sample_toggle_events(app: &TestApp, ds_id: i64) -> usize {
    use arcodash_core::entities::{data_source, event};
    let org_id = data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .org_id;
    event::Entity::find()
        .filter(event::Column::OrgId.eq(org_id))
        .filter(event::Column::Action.eq("ai_settings.sample_values_toggled"))
        .all(&app.db)
        .await
        .unwrap()
        .len()
}

#[tokio::test]
async fn generate_sql_prompt_contains_profile_samples_and_toggle_off_removes_them() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_limit(&app).await;
    let ds_id = create_ds_fake(&admin, &app.address).await;
    cache_schema_and_profile(&app, ds_id).await;
    let (stub_url, captured) = spawn_capturing_stub().await;
    configure_stub_provider(&admin, &app.address, &stub_url).await;

    let on = generate_and_capture_prompt(&app, &admin, ds_id, &captured).await;
    assert!(on.contains("zebra_shipped_7781"), "prompt: {on}");
    let events_before = sample_toggle_events(&app, ds_id).await;

    let resp = admin
        .post(format!("{}/api/organization/ai", app.address))
        .json(&serde_json::json!({ "include_sample_values": false }))
        .send()
        .await
        .unwrap();
    let settings = ok_json(resp, "toggle off").await;
    assert_eq!(settings["include_sample_values"], false);
    assert_eq!(
        sample_toggle_events(&app, ds_id).await,
        events_before + 1,
        "toggling include_sample_values must record one audit event"
    );
    reset_limit(&app).await;

    let off = generate_and_capture_prompt(&app, &admin, ds_id, &captured).await;
    assert!(!off.contains("zebra_shipped_7781"), "prompt: {off}");
    assert!(!off.contains("samples:"));
    assert!(off.contains("orders("));
}

#[tokio::test]
async fn ai_settings_include_sample_values_defaults_true_and_round_trips() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let get = |admin: reqwest::Client, base: String| async move {
        let r = admin
            .get(format!("{base}/api/organization/ai"))
            .send()
            .await
            .unwrap();
        ok_json(r, "get ai settings").await
    };
    assert_eq!(
        get(admin.clone(), app.address.clone()).await["include_sample_values"],
        true
    );
    for want in [false, true] {
        let r = admin
            .post(format!("{}/api/organization/ai", app.address))
            .json(&serde_json::json!({ "include_sample_values": want }))
            .send()
            .await
            .unwrap();
        assert_eq!(ok_json(r, "patch").await["include_sample_values"], want);
        assert_eq!(
            get(admin.clone(), app.address.clone()).await["include_sample_values"],
            want
        );
    }
}

#[tokio::test]
async fn ai_settings_explain_alerts_defaults_true_and_round_trips() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let get = |admin: reqwest::Client, base: String| async move {
        let r = admin
            .get(format!("{base}/api/organization/ai"))
            .send()
            .await
            .unwrap();
        ok_json(r, "get ai settings").await
    };
    assert_eq!(
        get(admin.clone(), app.address.clone()).await["explain_alerts"],
        true
    );
    for want in [false, true] {
        let r = admin
            .post(format!("{}/api/organization/ai", app.address))
            .json(&serde_json::json!({ "explain_alerts": want }))
            .send()
            .await
            .unwrap();
        assert_eq!(ok_json(r, "patch").await["explain_alerts"], want);
        assert_eq!(
            get(admin.clone(), app.address.clone()).await["explain_alerts"],
            want
        );
    }
}

#[tokio::test]
async fn ai_settings_dashboard_narratives_defaults_true_and_round_trips() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let get = |admin: reqwest::Client, base: String| async move {
        let r = admin
            .get(format!("{base}/api/organization/ai"))
            .send()
            .await
            .unwrap();
        ok_json(r, "get ai settings").await
    };
    assert_eq!(
        get(admin.clone(), app.address.clone()).await["dashboard_narratives"],
        true
    );
    for want in [false, true] {
        let r = admin
            .post(format!("{}/api/organization/ai", app.address))
            .json(&serde_json::json!({ "dashboard_narratives": want }))
            .send()
            .await
            .unwrap();
        assert_eq!(ok_json(r, "patch").await["dashboard_narratives"], want);
        assert_eq!(
            get(admin.clone(), app.address.clone()).await["dashboard_narratives"],
            want
        );
    }
}

#[tokio::test]
async fn suggest_monitors_prompt_contains_profile_samples_and_toggle_off_removes_them() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_limit(&app).await;
    let ds_id = create_ds_fake(&admin, &app.address).await;
    cache_schema_and_profile(&app, ds_id).await;
    let (stub_url, captured) = spawn_capturing_stub().await;
    configure_stub_provider(&admin, &app.address, &stub_url).await;

    let suggest = |app: &TestApp, admin: reqwest::Client| {
        let url = format!("{}/api/data_sources/{ds_id}/suggest_monitors", app.address);
        async move {
            std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
            let resp = admin
                .post(url)
                .json(&serde_json::json!({ "tables": ["orders"] }))
                .send()
                .await
                .unwrap();
            std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
            resp.status().as_u16()
        }
    };

    let _status = suggest(&app, admin.clone()).await;
    let on = captured
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("prompt captured");
    assert!(on.contains("zebra_shipped_7781"), "prompt: {on}");

    let resp = admin
        .post(format!("{}/api/organization/ai", app.address))
        .json(&serde_json::json!({ "include_sample_values": false }))
        .send()
        .await
        .unwrap();
    ok_json(resp, "toggle off").await;
    reset_limit(&app).await;

    suggest(&app, admin.clone()).await;
    let off = captured
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("prompt captured");
    assert!(!off.contains("zebra_shipped_7781"), "prompt: {off}");
    assert!(!off.contains("samples:"), "prompt: {off}");
    assert!(off.contains("orders("), "prompt: {off}");
}

fn single_chain(kind: AiProviderKind, model: &str) -> Vec<ResolvedProvider> {
    vec![ResolvedProvider {
        kind,
        api_key: "key".to_string(),
        model: Some(model.to_string()),
        base_url: None,
    }]
}

async fn usage_row_for(
    app: &TestApp,
    org_id: i64,
    model: &str,
    user_id: Option<i64>,
) -> ai_usage::Model {
    let scope = RequestScope::pooled(app.db.clone());
    let org = organization::Entity::find_by_id(org_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let kind = AiProviderKind::Gemini;
    let outcomes = HashMap::from([(kind, FakeOutcome::Success)]);
    let (_, outcome) = run_fallback_chain(
        &scope,
        &org,
        user_id,
        &reqwest::Client::new(),
        &single_chain(kind, model),
        false,
        "sql_generation",
        "prompt",
        fake_lookup(outcomes, calls),
        |provider, http, api_key, model, prompt| {
            provider.generate_sql(http, api_key, model, prompt)
        },
        |gen: &SqlGeneration| (gen.tokens_in, gen.tokens_out),
    )
    .await;
    assert!(outcome.is_ok());
    ai_usage::Entity::find()
        .filter(ai_usage::Column::OrgId.eq(org_id))
        .one(&app.db)
        .await
        .unwrap()
        .expect("usage row written")
}

/// A priced model records the caller and cost = (10 in * $0.75 + 5 out * $3.75) / 1M.
#[tokio::test]
async fn usage_row_records_user_and_cost_for_priced_model() {
    let app = skip_if_none!(spawn_app().await);
    let org = make_org(&app.db).await;
    let row = usage_row_for(&app, org.id, "gemini-3.8-flash", Some(4242)).await;
    assert_eq!(row.user_id, Some(4242));
    let cost = row.cost_usd.expect("priced model has a cost");
    assert!((cost - 26.25e-6).abs() < 1e-12, "cost = {cost}");
}

/// An unpriced model stores NULL cost, never zero.
#[tokio::test]
async fn usage_row_cost_is_null_for_unpriced_model() {
    let app = skip_if_none!(spawn_app().await);
    let org = make_org(&app.db).await;
    let row = usage_row_for(&app, org.id, "gemini-unlisted-model", None).await;
    assert_eq!(row.user_id, None);
    assert_eq!(row.cost_usd, None);
}

async fn set_budget(admin: &reqwest::Client, base: &str, value: serde_json::Value) -> u16 {
    admin
        .post(format!("{base}/api/organization/ai"))
        .json(&serde_json::json!({ "monthly_budget_usd": value }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn seed_cost(app: &TestApp, cost: f64, created_at: chrono::DateTime<chrono::Utc>) {
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    ai_usage::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        org_id: Set(org.id),
        provider: Set("openai".to_string()),
        model: Set("m".to_string()),
        feature: Set("sql_generation".to_string()),
        status: Set("ok".to_string()),
        tokens_in: Set(None),
        tokens_out: Set(None),
        user_id: Set(None),
        cost_usd: Set(Some(cost)),
        created_at: Set(created_at.into()),
    }
    .insert(&app.db)
    .await
    .unwrap();
}

async fn usage_rows(app: &TestApp) -> usize {
    ai_usage::Entity::find().all(&app.db).await.unwrap().len()
}

async fn generate_status(
    admin: &reqwest::Client,
    app: &TestApp,
    ds_id: i64,
) -> (u16, serde_json::Value) {
    reset_limit(app).await;
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let resp = admin
        .post(format!("{}/api/queries/generate_sql", app.address))
        .json(&serde_json::json!({ "data_source_id": ds_id, "question": "orders per day?" }))
        .send()
        .await
        .unwrap();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    (resp.status().as_u16(), resp.json().await.unwrap())
}

#[tokio::test]
async fn budget_cap_spent_this_month_returns_429_without_provider_call() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds_fake(&admin, &app.address).await;
    cache_schema(
        &app,
        ds_id,
        Some(serde_json::json!({ "tables": [{ "name": "orders", "columns": [{ "name": "id", "type_name": "integer" }]}]})),
    )
    .await;
    let (stub_url, captured) = spawn_capturing_stub().await;
    configure_stub_provider(&admin, &app.address, &stub_url).await;
    assert_eq!(
        set_budget(&admin, &app.address, serde_json::json!(1.0)).await,
        200
    );
    seed_cost(&app, 1.5, chrono::Utc::now()).await;
    let before = usage_rows(&app).await;

    let (status, body) = generate_status(&admin, &app, ds_id).await;
    assert_eq!(status, 429, "body: {body}");
    assert_eq!(body["error_code"], "ai_budget_exceeded");
    assert!(
        body["message"].as_str().unwrap().contains("$1.00"),
        "{body}"
    );
    assert!(captured.lock().unwrap().is_empty());
    assert_eq!(usage_rows(&app).await, before);
}

#[tokio::test]
async fn budget_cap_ignores_last_month_spend_and_null_cap_is_unlimited() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds_fake(&admin, &app.address).await;
    cache_schema(
        &app,
        ds_id,
        Some(serde_json::json!({ "tables": [{ "name": "orders", "columns": [{ "name": "id", "type_name": "integer" }]}]})),
    )
    .await;
    let (stub_url, captured) = spawn_capturing_stub().await;
    configure_stub_provider(&admin, &app.address, &stub_url).await;
    assert_eq!(
        set_budget(&admin, &app.address, serde_json::json!(1.0)).await,
        200
    );
    let (month_start, _) = arcodash_core::ai::budget::month_window(chrono::Utc::now());
    seed_cost(&app, 5.0, month_start - chrono::Duration::hours(1)).await;

    let (status, body) = generate_status(&admin, &app, ds_id).await;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body["status"], "ok");

    seed_cost(&app, 5.0, chrono::Utc::now()).await;
    assert_eq!(generate_status(&admin, &app, ds_id).await.0, 429);
    assert_eq!(
        set_budget(&admin, &app.address, serde_json::Value::Null).await,
        200
    );
    assert_eq!(generate_status(&admin, &app, ds_id).await.0, 200);
    assert_eq!(captured.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn budget_cap_blocks_viz_and_monitor_suggestions() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds_fake(&admin, &app.address).await;
    cache_schema_and_profile(&app, ds_id).await;
    let (stub_url, captured) = spawn_capturing_stub().await;
    configure_stub_provider(&admin, &app.address, &stub_url).await;
    assert_eq!(
        set_budget(&admin, &app.address, serde_json::json!(1.0)).await,
        200
    );
    seed_cost(&app, 2.0, chrono::Utc::now()).await;

    reset_limit(&app).await;
    let viz = admin
        .post(format!("{}/api/queries/suggest_visualization", app.address))
        .json(&serde_json::json!({
            "columns": [{ "name": "total", "friendly_type": "number" }],
            "row_count": 3
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(viz.status().as_u16(), 429);
    let body: serde_json::Value = viz.json().await.unwrap();
    assert_eq!(body["error_code"], "ai_budget_exceeded");

    reset_limit(&app).await;
    let mon = admin
        .post(format!(
            "{}/api/data_sources/{ds_id}/suggest_monitors",
            app.address
        ))
        .json(&serde_json::json!({ "tables": ["orders"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(mon.status().as_u16(), 429);
    assert!(captured.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ai_settings_monthly_budget_validates_and_round_trips() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let get = || async {
        let r = admin
            .get(format!("{}/api/organization/ai", app.address))
            .send()
            .await
            .unwrap();
        ok_json(r, "get ai settings").await
    };
    assert!(get().await["monthly_budget_usd"].is_null());
    for bad in [0.0, -1.0] {
        assert_eq!(
            set_budget(&admin, &app.address, serde_json::json!(bad)).await,
            400
        );
    }
    assert_eq!(
        set_budget(&admin, &app.address, serde_json::json!(25.5)).await,
        200
    );
    assert_eq!(get().await["monthly_budget_usd"], 25.5);
    let r = admin
        .post(format!("{}/api/organization/ai", app.address))
        .json(&serde_json::json!({ "fallback_enabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(get().await["monthly_budget_usd"], 25.5);
    assert_eq!(
        set_budget(&admin, &app.address, serde_json::Value::Null).await,
        200
    );
    assert!(get().await["monthly_budget_usd"].is_null());
}
