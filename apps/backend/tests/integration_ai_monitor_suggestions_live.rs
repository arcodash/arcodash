//! Live "AI data-quality monitor suggestions" provider calls — a real-provider check of this
//! feature's structured-output schema: array-of-objects is new to all three providers (every
//! other schema — `SqlGeneration`/`VizGeneration` — is a single flat object), so a static
//! schema-invariant test alone proves nothing about whether the provider actually accepts the
//! shape. Ignored unless a real provider key is present; run with:
//! GEMINI_API_KEY=...    cargo test -p arcodash-backend --test integration_ai_monitor_suggestions_live -- --ignored gemini
//! OPENAI_API_KEY=...    cargo test -p arcodash-backend --test integration_ai_monitor_suggestions_live -- --ignored openai
//! ANTHROPIC_API_KEY=... cargo test -p arcodash-backend --test integration_ai_monitor_suggestions_live -- --ignored anthropic

use arcodash_backend::api::handlers::ai::anthropic::AnthropicProvider;
use arcodash_backend::api::handlers::ai::gemini::GeminiProvider;
use arcodash_backend::api::handlers::ai::monitor_suggestions::{
    build_response, build_suggest_prompt, CheckKind, RawSuggestion,
};
use arcodash_backend::api::handlers::ai::openai::OpenAiProvider;
use arcodash_backend::api::handlers::ai::AiProvider;
use arcodash_core::connectors::{ColumnSchema, TableSchema};
use std::str::FromStr;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set to run this live test"))
}

fn schema() -> Vec<TableSchema> {
    vec![
        TableSchema {
            name: "orders".to_string(),
            columns: vec![
                ColumnSchema::typed("id", "bigint"),
                ColumnSchema::typed("created_at", "timestamptz"),
                ColumnSchema::typed("customer_id", "bigint"),
            ],
        },
        TableSchema {
            name: "customers".to_string(),
            columns: vec![
                ColumnSchema::typed("id", "bigint"),
                ColumnSchema::typed("email", "text"),
                ColumnSchema::typed("phone", "text"),
            ],
        },
    ]
}

/// Asserts the shape every live call must satisfy: the call succeeds at all (a strict-mode/schema
/// rejection surfaces here, nowhere else), the model actually proposed something, every item's
/// `kind` is one of our four known shapes, token counts are present, and feeding the raw result
/// through `build_response` doesn't discard it wholesale (a live-only failure mode distinct from a
/// transport/schema rejection).
///
/// Deliberately does NOT assert `s.column == kind.pinned_alias()` on the raw suggestion: that's
/// the model's own self-report of a value fully determined by `kind`, and it drifts across all
/// three providers in practice — `validate_one` derives `column` from `kind`
/// server-side and checks the SQL's actual alias instead, which is what `build_response` below
/// exercises. A self-report mismatch here is expected, not a failure.
fn assert_live_suggestions_are_well_formed(
    suggestions: &[RawSuggestion],
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
) {
    assert!(
        !suggestions.is_empty(),
        "the model must propose at least one suggestion"
    );
    for s in suggestions {
        let kind = CheckKind::from_str(&s.kind)
            .unwrap_or_else(|_| panic!("model returned an unrecognized kind: {}", s.kind));
        if s.column != kind.pinned_alias() {
            eprintln!(
                "note: kind {} self-reported column={} instead of its pinned alias {} — not \
                 trusted, validate_one derives it from kind instead",
                s.kind,
                s.column,
                kind.pinned_alias()
            );
        }
    }
    assert!(tokens_in.is_some(), "expected a prompt token count");
    assert!(tokens_out.is_some(), "expected a completion token count");

    let (built, _covered) = build_response(&schema(), suggestions, &[]);
    assert!(
        !built.is_empty(),
        "build_response must not discard every live suggestion wholesale"
    );
    for sg in &built {
        let kind = CheckKind::from_str(sg.kind).expect("build_response only emits known kinds");
        assert_eq!(
            sg.column,
            kind.pinned_alias(),
            "a validated suggestion's column must be its kind's pinned alias"
        );
    }
}

/// 30 tables x 14 columns, the size of a real sample/demo source.
fn large_schema() -> Vec<TableSchema> {
    (0..30)
        .map(|t| TableSchema {
            name: format!("table_{t}"),
            columns: (0..14)
                .map(|c| {
                    let ty = ["bigint", "text", "timestamptz", "numeric"][c % 4];
                    let name = match c {
                        0 => "id".to_string(),
                        1 => "created_at".to_string(),
                        2 => "email".to_string(),
                        _ => format!("col_{c}"),
                    };
                    ColumnSchema::typed(&name, ty)
                })
                .collect(),
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires a real GEMINI_API_KEY"]
async fn gemini_suggest_monitors_live() {
    let key = env("GEMINI_API_KEY");
    let model = std::env::var("GEMINI_MODEL")
        .unwrap_or_else(|_| GeminiProvider.default_model().to_string());
    let prompt = build_suggest_prompt("pg", &large_schema());
    let http = reqwest::Client::new();

    let result = GeminiProvider
        .suggest_monitors(&http, &key, &model, &prompt)
        .await
        .unwrap_or_else(|e| panic!("live gemini suggest_monitors failed: {e:?}"));

    assert_live_suggestions_are_well_formed(
        &result.suggestions,
        result.tokens_in,
        result.tokens_out,
    );
}

/// SQLite-typed source with a dotted table name (the JSON connector's `d.json` shape): every
/// suggested SQL must actually execute on SQLite, not merely avoid Postgres-only tokens.
#[tokio::test]
#[ignore = "requires a real GEMINI_API_KEY"]
async fn gemini_suggest_monitors_sqlite_sql_executes() {
    let key = env("GEMINI_API_KEY");
    let model = std::env::var("GEMINI_MODEL")
        .unwrap_or_else(|_| GeminiProvider.default_model().to_string());
    let schema = vec![
        TableSchema {
            name: "d.json".to_string(),
            columns: vec![
                ColumnSchema::typed("id", "INTEGER"),
                ColumnSchema::typed("created_at", "TEXT"),
                ColumnSchema::typed("email", "TEXT"),
            ],
        },
        TableSchema {
            name: "orders".to_string(),
            columns: vec![
                ColumnSchema::typed("id", "INTEGER"),
                ColumnSchema::typed("updated_at", "TEXT"),
                ColumnSchema::typed("customer_id", "INTEGER"),
            ],
        },
    ];
    let prompt = build_suggest_prompt("json", &schema);
    let http = reqwest::Client::new();
    let result = GeminiProvider
        .suggest_monitors(&http, &key, &model, &prompt)
        .await
        .unwrap_or_else(|e| panic!("live gemini suggest_monitors failed: {e:?}"));
    assert!(!result.suggestions.is_empty(), "no suggestions returned");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE \"d.json\" (id INTEGER, created_at TEXT, email TEXT);
         INSERT INTO \"d.json\" VALUES (1, '2026-10-01T10:00:00Z', 'a@x.io'), (2, '2026-10-02T10:00:00Z', NULL);
         CREATE TABLE orders (id INTEGER, updated_at TEXT, customer_id INTEGER);
         INSERT INTO orders VALUES (1, '2026-10-01 09:00:00', 7), (2, '2026-10-02 09:00:00', NULL);",
    )
    .unwrap();
    for s in &result.suggestions {
        eprintln!("[{}] {}", s.kind, s.sql);
        let got: Result<f64, _> = conn.query_row(&s.sql, [], |r| r.get(0));
        assert!(
            got.is_ok(),
            "SQL failed on SQLite: {}\n{:?}",
            s.sql,
            got.err()
        );
    }
}

/// Highest-risk live call: OpenAI strict mode rejects the WHOLE schema (not just
/// one field) if any property is missing from `required` or `additionalProperties: false` is
/// absent at either the object or item level — `openai_suggest_schema_strict_mode_invariants`
/// checks that statically; only this live call proves the provider actually agrees.
#[tokio::test]
#[ignore = "requires a real OPENAI_API_KEY"]
async fn openai_suggest_monitors_live() {
    let key = env("OPENAI_API_KEY");
    let provider = OpenAiProvider::new(None);
    let model =
        std::env::var("OPENAI_MODEL").unwrap_or_else(|_| provider.default_model().to_string());
    let prompt = build_suggest_prompt("pg", &schema());
    let http = reqwest::Client::new();

    let result = provider
        .suggest_monitors(&http, &key, &model, &prompt)
        .await
        .expect("live openai suggest_monitors call must succeed");

    assert_live_suggestions_are_well_formed(
        &result.suggestions,
        result.tokens_in,
        result.tokens_out,
    );
}

#[tokio::test]
#[ignore = "requires a real ANTHROPIC_API_KEY"]
async fn anthropic_suggest_monitors_live() {
    let key = env("ANTHROPIC_API_KEY");
    let model = std::env::var("ANTHROPIC_MODEL")
        .unwrap_or_else(|_| AnthropicProvider.default_model().to_string());
    let prompt = build_suggest_prompt("pg", &schema());
    let http = reqwest::Client::new();

    let result = AnthropicProvider
        .suggest_monitors(&http, &key, &model, &prompt)
        .await
        .expect("live anthropic suggest_monitors call must succeed");

    assert_live_suggestions_are_well_formed(
        &result.suggestions,
        result.tokens_in,
        result.tokens_out,
    );
}
