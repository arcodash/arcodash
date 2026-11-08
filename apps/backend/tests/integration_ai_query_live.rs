//! Live text-to-SQL provider call with a typed cached-schema prompt. Ignored unless a real key is
//! present; run with: OPENAI_API_KEY=... cargo test -p arcodash-backend --test
//! integration_ai_query_live -- --ignored --nocapture openai ANTHROPIC_API_KEY=... cargo test -p
//! arcodash-backend --test integration_ai_query_live -- --ignored --nocapture anthropic

mod common;
use common::spawn_app;

use arcodash_backend::api::handlers::ai::anthropic::AnthropicProvider;
use arcodash_backend::api::handlers::ai::openai::OpenAiProvider;
use arcodash_backend::api::handlers::ai::{format_schema_for_prompt, AiProvider, AiProviderKind};
use arcodash_backend::api::handlers::ai_query::build_prompt;
use arcodash_core::connectors::{ColumnSchema, TableSchema};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set to run this live test"))
}

fn prompt() -> String {
    let schema = vec![
        TableSchema {
            name: "customers".into(),
            columns: vec![
                ColumnSchema::typed("id", "integer"),
                ColumnSchema::typed("name", "text"),
            ],
        },
        TableSchema {
            name: "orders".into(),
            columns: vec![
                ColumnSchema::typed("id", "integer"),
                ColumnSchema::typed("customer_id", "integer"),
                ColumnSchema::typed("created_at", "timestamp with time zone"),
            ],
        },
    ];
    build_prompt(
        "pg",
        &format_schema_for_prompt(&schema),
        "top 5 customers by order count last 30 days",
        None,
    )
}

fn assert_well_formed(sql: &str) {
    let lower = sql.to_lowercase();
    assert!(lower.contains("select"), "not a SELECT: {sql}");
    assert!(lower.contains("orders"), "ignores orders table: {sql}");
    assert!(lower.contains("join"), "expected a join: {sql}");
}

#[tokio::test]
#[ignore = "requires a real OPENAI_API_KEY"]
async fn openai_generate_sql_live() {
    let key = env("OPENAI_API_KEY");
    let provider = OpenAiProvider::new(None);
    let model =
        std::env::var("OPENAI_MODEL").unwrap_or_else(|_| provider.default_model().to_string());
    let out = provider
        .generate_sql(&reqwest::Client::new(), &key, &model, &prompt())
        .await
        .expect("live openai generate_sql must succeed");
    println!("SERVED SQL:\n{}", out.sql);
    assert_well_formed(&out.sql);
}

#[tokio::test]
#[ignore = "requires a real ANTHROPIC_API_KEY"]
async fn anthropic_generate_sql_live() {
    let key = env("ANTHROPIC_API_KEY");
    let model = std::env::var("ANTHROPIC_MODEL")
        .unwrap_or_else(|_| AnthropicProvider.default_model().to_string());
    let out = AnthropicProvider
        .generate_sql(&reqwest::Client::new(), &key, &model, &prompt())
        .await
        .expect("live anthropic generate_sql must succeed");
    println!("SERVED SQL:\n{}", out.sql);
    assert_well_formed(&out.sql);
}

/// Drives the metered `run_fallback_chain` with the real provider and its default model, then
/// asserts the persisted `ai_usage` row carries the caller and a positive cost.
async fn assert_metered_cost(kind: AiProviderKind, key_var: &str) {
    use arcodash_backend::api::handlers::ai_query::run_fallback_chain;
    use arcodash_backend::api::handlers::ai_settings::ResolvedProvider;
    use arcodash_backend::db::RequestScope;
    use arcodash_core::entities::{ai_usage, organization};
    use arcodash_core::test_support::new_organization;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

    let key = env(key_var);
    let app = skip_if_none!(spawn_app().await);
    let mut org = new_organization();
    org.slug = Set(format!("ai-cost-live-{}", uuid::Uuid::new_v4().simple()));
    let org: organization::Model = org.insert(&app.db).await.expect("insert org");
    let user_id = 424_242_i64;
    let lookup = |k: AiProviderKind, base_url: Option<String>| -> Box<dyn AiProvider> {
        match k {
            AiProviderKind::Gemini => {
                Box::new(arcodash_backend::api::handlers::ai::gemini::GeminiProvider)
            }
            AiProviderKind::OpenAi => Box::new(OpenAiProvider::new(base_url)),
            AiProviderKind::Anthropic => Box::new(AnthropicProvider),
        }
    };
    let default_model = lookup(kind, None).default_model().to_string();
    let chain = [ResolvedProvider {
        kind,
        api_key: key,
        model: Some(default_model),
        base_url: None,
    }];
    let (_, outcome) = run_fallback_chain(
        &RequestScope::pooled(app.db.clone()),
        &org,
        Some(user_id),
        &reqwest::Client::new(),
        &chain,
        false,
        "sql_generation",
        &prompt(),
        lookup,
        |p, http, k, m, pr| p.generate_sql(http, k, m, pr),
        |g: &arcodash_core::ai::SqlGeneration| (g.tokens_in, g.tokens_out),
    )
    .await;
    outcome.expect("live generate_sql through the metered chain must succeed");
    let row = ai_usage::Entity::find()
        .filter(ai_usage::Column::OrgId.eq(org.id))
        .one(&app.db)
        .await
        .unwrap()
        .expect("usage row written");
    println!(
        "ROW provider={} model={} tokens_in={:?} tokens_out={:?} cost_usd={:?} user_id={:?}",
        row.provider, row.model, row.tokens_in, row.tokens_out, row.cost_usd, row.user_id
    );
    assert_eq!(row.user_id, Some(user_id));
    assert!(row.tokens_in.unwrap_or(0) > 0 && row.tokens_out.unwrap_or(0) > 0);
    let cost = row
        .cost_usd
        .unwrap_or_else(|| panic!("no price row for model {}", row.model));
    assert!(cost > 0.0, "cost = {cost}");
}

#[tokio::test]
#[ignore = "requires a real GEMINI_API_KEY"]
async fn gemini_metered_cost_live() {
    assert_metered_cost(AiProviderKind::Gemini, "GEMINI_API_KEY").await;
}

#[tokio::test]
#[ignore = "requires a real OPENAI_API_KEY"]
async fn openai_metered_cost_live() {
    assert_metered_cost(AiProviderKind::OpenAi, "OPENAI_API_KEY").await;
}

#[tokio::test]
#[ignore = "requires a real ANTHROPIC_API_KEY"]
async fn anthropic_metered_cost_live() {
    assert_metered_cost(AiProviderKind::Anthropic, "ANTHROPIC_API_KEY").await;
}
