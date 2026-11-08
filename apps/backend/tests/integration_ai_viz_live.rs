//! Live "Auto Visualize" provider calls — a real-provider check of a structured-output feature:
//! a hardcoded schema/model name can be syntactically perfect and still be rejected by the
//! provider (see `openai.rs`'s strict-mode schema comment). Ignored
//! unless a real provider key is present; run with:
//! GEMINI_API_KEY=... [GEMINI_MODEL=...] cargo test -p arcodash-backend --test integration_ai_viz_live -- --ignored gemini
//! OPENAI_API_KEY=... cargo test -p arcodash-backend --test integration_ai_viz_live -- --ignored openai
//! ANTHROPIC_API_KEY=... cargo test -p arcodash-backend --test integration_ai_viz_live -- --ignored anthropic
//! OPENAI_API_KEY=... cargo test -p arcodash-backend --test integration_ai_viz_live -- --ignored base_url

use arcodash_backend::api::handlers::ai::anthropic::AnthropicProvider;
use arcodash_backend::api::handlers::ai::gemini::GeminiProvider;
use arcodash_backend::api::handlers::ai::openai::OpenAiProvider;
use arcodash_backend::api::handlers::ai::viz::{
    build_viz_prompt, sanitize_chart_suggestion, ColumnMeta, VizSuggestion,
};
use arcodash_backend::api::handlers::ai::AiProvider;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set to run this live test"))
}

fn columns() -> Vec<ColumnMeta> {
    vec![
        ColumnMeta {
            name: "status".to_string(),
            friendly_type: "string".to_string(),
        },
        ColumnMeta {
            name: "count".to_string(),
            friendly_type: "integer".to_string(),
        },
    ]
}

/// A "net revenue by store and month" result (18 stores x ~18 months = 328 rows) — a group-by
/// that must render as one series per store, not 18 stores collapsed onto one sawtooth series.
fn store_month_revenue_columns() -> Vec<ColumnMeta> {
    vec![
        ColumnMeta {
            name: "store".to_string(),
            friendly_type: "string".to_string(),
        },
        ColumnMeta {
            name: "month".to_string(),
            friendly_type: "date".to_string(),
        },
        ColumnMeta {
            name: "net_revenue".to_string(),
            friendly_type: "float".to_string(),
        },
    ]
}

#[tokio::test]
#[ignore = "requires a real GEMINI_API_KEY"]
async fn gemini_suggest_visualization_live() {
    let key = env("GEMINI_API_KEY");
    let model = std::env::var("GEMINI_MODEL")
        .unwrap_or_else(|_| GeminiProvider.default_model().to_string());
    let prompt = build_viz_prompt(&columns(), 4);
    let http = reqwest::Client::new();

    let result = GeminiProvider
        .suggest_visualization(&http, &key, &model, &prompt)
        .await
        .expect("live gemini suggest_visualization call must succeed");

    assert!(
        matches!(
            result.suggestion.type_str(),
            "CHART" | "COUNTER" | "FUNNEL" | "TABLE"
        ),
        "unexpected viz type: {}",
        result.suggestion.type_str()
    );
    assert!(result.tokens_in.is_some(), "expected a prompt token count");
    assert!(
        result.tokens_out.is_some(),
        "expected a completion token count"
    );
}

/// Highest-risk live call: OpenAI strict mode rejects the whole schema (not just one field) if
/// any property is missing from `required` or an optional field isn't a nullable type union —
/// exactly the invariant `openai_viz_schema_strict_mode_invariants` checks statically, but only a
/// live call catches the provider actually accepting it end to end.
#[tokio::test]
#[ignore = "requires a real OPENAI_API_KEY"]
async fn openai_suggest_visualization_live() {
    let key = env("OPENAI_API_KEY");
    let provider = OpenAiProvider::new(None);
    let model =
        std::env::var("OPENAI_MODEL").unwrap_or_else(|_| provider.default_model().to_string());
    let prompt = build_viz_prompt(&columns(), 4);
    let http = reqwest::Client::new();

    let result = provider
        .suggest_visualization(&http, &key, &model, &prompt)
        .await
        .expect("live openai suggest_visualization call must succeed");

    assert!(
        matches!(
            result.suggestion.type_str(),
            "CHART" | "COUNTER" | "FUNNEL" | "TABLE"
        ),
        "unexpected viz type: {}",
        result.suggestion.type_str()
    );
    assert!(result.tokens_in.is_some(), "expected a prompt token count");
    assert!(
        result.tokens_out.is_some(),
        "expected a completion token count"
    );
}

/// A store x month result whose net_revenue must render as one line per store: Auto Visualize
/// has to be able to express a group-by. Run with `--nocapture` to see the model's raw options.
#[tokio::test]
#[ignore = "requires a real GEMINI_API_KEY"]
async fn gemini_suggest_visualization_live_groups_repeating_dimension() {
    let key = env("GEMINI_API_KEY");
    let model = std::env::var("GEMINI_MODEL")
        .unwrap_or_else(|_| GeminiProvider.default_model().to_string());
    let cols = store_month_revenue_columns();
    let prompt = build_viz_prompt(&cols, 328);
    println!("=== PROMPT ===\n{prompt}\n=== END PROMPT ===");
    let http = reqwest::Client::new();

    let result = GeminiProvider
        .suggest_visualization(&http, &key, &model, &prompt)
        .await
        .expect("live gemini suggest_visualization call must succeed");

    println!("title: {:?}", result.title);
    println!("viz_type: {}", result.suggestion.type_str());
    println!(
        "raw options: {}",
        serde_json::to_string_pretty(&result.suggestion.options_json()).unwrap()
    );

    if let VizSuggestion::Chart(opts) = &result.suggestion {
        let sanitized = sanitize_chart_suggestion(opts.clone(), &cols);
        println!(
            "sanitized options: {}",
            serde_json::to_string_pretty(&sanitized).unwrap()
        );
        assert_eq!(
            sanitized.series_column.as_deref(),
            Some("store"),
            "expected the model to group by the repeating `store` dimension, got {:?}",
            sanitized.series_column
        );
        assert_eq!(
            sanitized.aggregation.as_deref(),
            Some("sum"),
            "expected aggregation to be sum once a group-by is chosen, got {:?}",
            sanitized.aggregation
        );
        assert_eq!(
            sanitized.x_column.as_deref(),
            Some("month"),
            "expected the model to plot the other dimension on X, got {:?}",
            sanitized.x_column
        );
        assert_eq!(sanitized.y_columns, Some(vec!["net_revenue".to_string()]));
    } else {
        panic!(
            "expected a CHART suggestion for a store x month revenue result, got {}",
            result.suggestion.type_str()
        );
    }
}

/// `OpenAiProvider::client_for` short-circuits to the shared client when `base_url` is `None`;
/// setting it explicitly (even to the default host) forces the guarded/pinned branch instead,
/// which is what every org-configured OpenAI-compatible endpoint actually runs through.
#[tokio::test]
#[ignore = "requires a real OPENAI_API_KEY"]
async fn openai_test_key_live_via_explicit_base_url() {
    let key = env("OPENAI_API_KEY");
    let provider = OpenAiProvider::new(Some("https://api.openai.com".to_string()));
    let http = reqwest::Client::new();

    provider
        .test_key(&http, &key, provider.default_model())
        .await
        .expect("live openai test_key call through the pinned client must succeed");
}

/// `OpenAiProvider::test_key` must reject a model the key genuinely can't reach, not just an
/// invalid key — the per-model retrieve endpoint 404s for a nonexistent model even with a
/// perfectly valid key, which is exactly the "wrong configured model" case this probe exists to
/// catch.
#[tokio::test]
#[ignore = "requires a real OPENAI_API_KEY"]
async fn openai_test_key_live_rejects_a_model_the_key_cannot_reach() {
    let key = env("OPENAI_API_KEY");
    let provider = OpenAiProvider::new(None);
    let http = reqwest::Client::new();

    let result = provider
        .test_key(&http, &key, "definitely-not-a-real-model-xyz123")
        .await;
    assert!(
        result.is_err(),
        "a valid key probed against a model it can't reach must fail, not report the key as working"
    );
}

#[tokio::test]
#[ignore = "requires a real GEMINI_API_KEY"]
async fn gemini_test_key_live_probes_the_configured_model() {
    let key = env("GEMINI_API_KEY");
    let model = std::env::var("GEMINI_MODEL")
        .unwrap_or_else(|_| GeminiProvider.default_model().to_string());
    let http = reqwest::Client::new();

    GeminiProvider
        .test_key(&http, &key, &model)
        .await
        .expect("live gemini test_key call against the configured model must succeed");
}

/// Same "wrong configured model" case as OpenAI's, for Gemini's per-model retrieve endpoint.
#[tokio::test]
#[ignore = "requires a real GEMINI_API_KEY"]
async fn gemini_test_key_live_rejects_a_model_the_key_cannot_reach() {
    let key = env("GEMINI_API_KEY");
    let http = reqwest::Client::new();

    let result = GeminiProvider
        .test_key(&http, &key, "definitely-not-a-real-model-xyz123")
        .await;
    assert!(
        result.is_err(),
        "a valid key probed against a model it can't reach must fail, not report the key as working"
    );
}

#[tokio::test]
#[ignore = "requires a real ANTHROPIC_API_KEY"]
async fn anthropic_test_key_live_probes_the_configured_model() {
    let key = env("ANTHROPIC_API_KEY");
    let model = std::env::var("ANTHROPIC_MODEL")
        .unwrap_or_else(|_| AnthropicProvider.default_model().to_string());
    let http = reqwest::Client::new();

    AnthropicProvider
        .test_key(&http, &key, &model)
        .await
        .expect("live anthropic test_key call against the configured model must succeed");
}

#[tokio::test]
#[ignore = "requires a real ANTHROPIC_API_KEY"]
async fn anthropic_suggest_visualization_live() {
    let key = env("ANTHROPIC_API_KEY");
    let model = std::env::var("ANTHROPIC_MODEL")
        .unwrap_or_else(|_| AnthropicProvider.default_model().to_string());
    let prompt = build_viz_prompt(&columns(), 4);
    let http = reqwest::Client::new();

    let result = AnthropicProvider
        .suggest_visualization(&http, &key, &model, &prompt)
        .await
        .expect("live anthropic suggest_visualization call must succeed");

    assert!(
        matches!(
            result.suggestion.type_str(),
            "CHART" | "COUNTER" | "FUNNEL" | "TABLE"
        ),
        "unexpected viz type: {}",
        result.suggestion.type_str()
    );
    assert!(result.tokens_in.is_some(), "expected a prompt token count");
    assert!(
        result.tokens_out.is_some(),
        "expected a completion token count"
    );
}
