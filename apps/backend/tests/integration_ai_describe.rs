//! Request-path describe trigger: a `DescribeDataSource` job is queued instead of a provider call.
//! The `--ignored` tests at the bottom call each real provider's `describe_schema` once.

mod common;
use common::{spawn_app, TestApp};

use arcodash_backend::api::handlers::ai::describe::{
    build_describe_prompt, spawn_for_request, SchemaDescriptions,
};
use arcodash_backend::api::handlers::ai::{AiProvider, AiProviderKind};
use arcodash_backend::api::handlers::ai_settings::ResolvedProvider;
use arcodash_backend::db::RequestScope;
use arcodash_core::connectors::{ColumnSchema, TableSchema};
use arcodash_core::entities::data_source;
use arcodash_core::models::DataSourceAiProfile;
use arcodash_core::queue::QueueName;
use arcodash_core::test_support::{new_data_source, new_organization};
use redis::AsyncCommands;
use sea_orm::{ActiveModelTrait, ActiveValue::Set};

struct Setup {
    app: TestApp,
    org: arcodash_core::entities::organization::Model,
    ds: data_source::Model,
    profile: DataSourceAiProfile,
}

async fn setup() -> Option<Setup> {
    let app = spawn_app().await?;
    let mut o = new_organization();
    o.slug = Set(format!("ai-describe-{}", uuid::Uuid::new_v4().simple()));
    let org = o.insert(&app.db).await.unwrap();
    let ds = new_data_source(org.id).insert(&app.db).await.unwrap();
    let raw = serde_json::json!({
        "tables": [{ "name": "orders", "sampled_rows": 10, "columns": [{
            "name": "status", "null_frac": 0.0, "distinct_in_sample": 2
        }]}],
        "relationships": [],
        "schema_refreshed_at_seen": "2026-01-01T00:00:00Z"
    });
    let mut a: data_source::ActiveModel = ds.into();
    a.ai_profile = Set(Some(raw.clone()));
    let ds = a.update(&app.db).await.unwrap();
    let mut conn = app.redis.clone();
    let _: () = conn
        .del(arcodash_core::models::ai_profile_describing_key(ds.id))
        .await
        .unwrap();
    Some(Setup {
        app,
        org,
        ds,
        profile: serde_json::from_value(raw).unwrap(),
    })
}

fn chain(n: usize) -> Vec<ResolvedProvider> {
    (0..n)
        .map(|_| ResolvedProvider {
            kind: AiProviderKind::Gemini,
            api_key: "k".into(),
            model: Some("m".into()),
            base_url: None,
        })
        .collect()
}

fn trigger(s: &Setup, profile: &DataSourceAiProfile, chain: &[ResolvedProvider]) {
    spawn_for_request(
        &s.app.state,
        &RequestScope::pooled(s.app.db.clone()),
        &s.org,
        &s.ds,
        Some(profile),
        &[],
        chain,
        true,
        1,
    );
}

/// Describe jobs for this data source currently on the shared schemas queue.
async fn queued(s: &Setup) -> usize {
    // The enqueue runs on a spawned task.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let mut conn = s.app.redis.clone();
    let jobs: Vec<String> = conn
        .lrange(QueueName::Schemas.queue_key(), 0, -1)
        .await
        .unwrap();
    jobs.iter()
        .filter(|j| {
            let v: serde_json::Value = serde_json::from_str(j).unwrap_or_default();
            v["type"] == "DescribeDataSource"
                && v["data_source_id"].as_i64() == Some(s.ds.id)
                && v["org_id"].as_i64() == Some(s.org.id)
        })
        .count()
}

#[tokio::test]
async fn undescribed_profile_queues_a_describe_job() {
    let s = skip_if_none!(setup().await);
    trigger(&s, &s.profile, &chain(1));
    assert_eq!(queued(&s).await, 1);
}

#[tokio::test]
async fn no_provider_queues_nothing() {
    let s = skip_if_none!(setup().await);
    trigger(&s, &s.profile, &chain(0));
    assert_eq!(queued(&s).await, 0);
}

#[tokio::test]
async fn described_profile_queues_nothing() {
    let s = skip_if_none!(setup().await);
    let mut p = s.profile.clone();
    p.tables[0].columns[0].description = Some("x".into());
    trigger(&s, &p, &chain(1));
    assert_eq!(queued(&s).await, 0);
}

#[tokio::test]
async fn held_guard_queues_nothing() {
    let s = skip_if_none!(setup().await);
    let mut conn = s.app.redis.clone();
    let _: () = conn
        .set_ex(
            arcodash_core::models::ai_profile_describing_key(s.ds.id),
            1,
            60,
        )
        .await
        .unwrap();
    trigger(&s, &s.profile, &chain(1));
    assert_eq!(queued(&s).await, 0);
    let _: () = conn
        .del(arcodash_core::models::ai_profile_describing_key(s.ds.id))
        .await
        .unwrap();
}

fn live_prompt() -> String {
    let profile: DataSourceAiProfile = serde_json::from_value(serde_json::json!({
        "tables": [{ "name": "orders", "sampled_rows": 3, "columns": [
            { "name": "status", "null_frac": 0.0, "distinct_in_sample": 2, "samples": ["shipped", "pending"] },
            { "name": "created_at", "null_frac": 0.0, "distinct_in_sample": 3 }
        ]}],
        "relationships": [],
        "schema_refreshed_at_seen": "2026-01-01T00:00:00Z"
    }))
    .unwrap();
    let t = vec![TableSchema {
        name: "orders".into(),
        columns: vec![
            ColumnSchema::typed("status", "text"),
            ColumnSchema::typed("created_at", "timestamptz"),
        ],
    }];
    build_describe_prompt(&t, &profile, true)
}

fn report(provider: &str, d: &SchemaDescriptions) {
    let first = d
        .tables
        .first()
        .and_then(|t| t.columns.first())
        .expect("at least one described column");
    assert!(!first.description.trim().is_empty());
    let shown: String = first.description.chars().take(200).collect();
    println!("{provider} -> {}: {shown}", first.column);
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set to run this live test"))
}

#[tokio::test]
#[ignore = "requires a real GEMINI_API_KEY"]
async fn gemini_describe_schema_live() {
    use arcodash_backend::api::handlers::ai::gemini::GeminiProvider;
    let model = std::env::var("GEMINI_MODEL")
        .unwrap_or_else(|_| GeminiProvider.default_model().to_string());
    let d = GeminiProvider
        .describe_schema(
            &reqwest::Client::new(),
            &env("GEMINI_API_KEY"),
            &model,
            &live_prompt(),
        )
        .await
        .unwrap_or_else(|e| panic!("live gemini describe_schema failed: {e:?}"));
    report("gemini", &d);
}

#[tokio::test]
#[ignore = "requires a real OPENAI_API_KEY"]
async fn openai_describe_schema_live() {
    use arcodash_backend::api::handlers::ai::openai::OpenAiProvider;
    let p = OpenAiProvider::new(None);
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| p.default_model().to_string());
    let d = p
        .describe_schema(
            &reqwest::Client::new(),
            &env("OPENAI_API_KEY"),
            &model,
            &live_prompt(),
        )
        .await
        .unwrap_or_else(|e| panic!("live openai describe_schema failed: {e:?}"));
    report("openai", &d);
}

#[tokio::test]
#[ignore = "requires a real ANTHROPIC_API_KEY"]
async fn anthropic_describe_schema_live() {
    use arcodash_backend::api::handlers::ai::anthropic::AnthropicProvider;
    let model = std::env::var("ANTHROPIC_MODEL")
        .unwrap_or_else(|_| AnthropicProvider.default_model().to_string());
    let d = AnthropicProvider
        .describe_schema(
            &reqwest::Client::new(),
            &env("ANTHROPIC_API_KEY"),
            &model,
            &live_prompt(),
        )
        .await
        .unwrap_or_else(|e| panic!("live anthropic describe_schema failed: {e:?}"));
    report("anthropic", &d);
}
