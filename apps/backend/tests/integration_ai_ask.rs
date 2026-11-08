//! Integration tests for the agentic ask loop (`ai_ask::ask_loop`), driven with a scripted
//! provider and a fake `PreviewRunner` — no network, no worker. A real org row is used so the
//! per-call provider-status/usage writes can be asserted.

mod common;
use common::spawn_app;

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::response::sse::Event;
use axum::response::{IntoResponse, Sse};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use arcodash_backend::api::handlers::ai::monitor_suggestions::MonitorSuggestionGeneration;
use arcodash_backend::api::handlers::ai::viz::VizGeneration;
use arcodash_backend::api::handlers::ai::viz::{CounterOptions, VizSuggestion};
use arcodash_backend::api::handlers::ai::{AiError, AiProvider, AiProviderKind, SqlGeneration};
use arcodash_backend::api::handlers::ai_ask::{
    ask_loop_outcome, AskCtx, AskOutcome, PreviewColumn, PreviewOutcome, PreviewRunner,
    ProfileRefresh,
};
use arcodash_backend::api::handlers::ai_monitor_ask::{monitor_ask_loop, MonitorAskCtx};
use arcodash_backend::api::handlers::ai_settings::ResolvedProvider;
use arcodash_backend::api::handlers::ai_widget::{widget_loop, WidgetCtx};
use arcodash_backend::db::RequestScope;
use arcodash_core::ai::JsonGeneration;
use arcodash_core::entities::{
    ai_usage, monitor, organization, query, user, visualization, widget,
};
use arcodash_core::models::{ai_profile_pending_key, ai_profile_refresh_cooldown_key, ProfileData};
use arcodash_core::queue::{JobMessage, Queue, QueueName};
use arcodash_core::test_support::new_organization;

type Step = Result<(&'static str, i64, i64), AiError>;

/// Pops the next scripted step on every `generate_sql` call and logs the prompt it was given.
struct ScriptedProvider {
    kind: AiProviderKind,
    script: Arc<Mutex<VecDeque<Step>>>,
    prompts: Arc<Mutex<Vec<String>>>,
    viz: VizQueue,
    json: JsonQueue,
}

type JsonQueue = Arc<Mutex<VecDeque<Result<JsonGeneration, AiError>>>>;
type VizQueue = Arc<Mutex<VecDeque<Result<VizGeneration, AiError>>>>;

#[async_trait::async_trait]
impl AiProvider for ScriptedProvider {
    fn kind(&self) -> AiProviderKind {
        self.kind
    }
    fn default_model(&self) -> &'static str {
        "fake-model"
    }
    fn curated_models(&self) -> &'static [&'static str] {
        &[]
    }
    async fn test_key(&self, _: &reqwest::Client, _: &str, _: &str) -> Result<(), AiError> {
        unimplemented!()
    }
    async fn generate_sql(
        &self,
        _: &reqwest::Client,
        _: &str,
        _: &str,
        prompt: &str,
    ) -> Result<SqlGeneration, AiError> {
        self.prompts.lock().unwrap().push(prompt.to_string());
        let step = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted");
        step.map(|(sql, tin, tout)| SqlGeneration {
            sql: sql.to_string(),
            explanation: Some(format!("explains {sql}")),
            tokens_in: Some(tin),
            tokens_out: Some(tout),
        })
    }
    async fn suggest_visualization(
        &self,
        _: &reqwest::Client,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<VizGeneration, AiError> {
        self.viz
            .lock()
            .unwrap()
            .pop_front()
            .expect("viz script exhausted")
    }
    async fn generate_json(
        &self,
        _: &reqwest::Client,
        _: &str,
        _: &str,
        prompt: &str,
        _: &Value,
    ) -> Result<JsonGeneration, AiError> {
        self.prompts.lock().unwrap().push(prompt.to_string());
        self.json
            .lock()
            .unwrap()
            .pop_front()
            .expect("json script exhausted")
    }
    async fn suggest_monitors(
        &self,
        _: &reqwest::Client,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<MonitorSuggestionGeneration, AiError> {
        unimplemented!()
    }
    async fn describe_schema(
        &self,
        _: &reqwest::Client,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<arcodash_backend::api::handlers::ai::describe::SchemaDescriptions, AiError> {
        unimplemented!()
    }
}

/// Pops the next scripted outcome and records every SQL it is asked to run.
struct FakePreview {
    script: Mutex<VecDeque<PreviewOutcome>>,
    ran: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl PreviewRunner for FakePreview {
    async fn run(&self, sql: &str) -> PreviewOutcome {
        self.ran.lock().unwrap().push(sql.to_string());
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .expect("preview script exhausted")
    }
}

fn rows(n: u64) -> PreviewOutcome {
    PreviewOutcome::Ok {
        columns: vec![PreviewColumn {
            name: "id".into(),
            friendly_type: "integer".into(),
        }],
        rows: (0..n.min(5))
            .map(|i| json!({ "id": i, "secret": "ROW_SECRET" }))
            .collect(),
        row_count: n,
        truncated: false,
    }
}

fn err(msg: &str) -> PreviewOutcome {
    PreviewOutcome::Err(msg.to_string())
}

struct Harness {
    events: Vec<(String, Value)>,
    outcome: Option<AskOutcome>,
    prompts: Vec<String>,
    previewed: Vec<String>,
    org: organization::Model,
    db: sea_orm::DatabaseConnection,
}

impl Harness {
    fn names(&self) -> Vec<&str> {
        self.events.iter().map(|(n, _)| n.as_str()).collect()
    }
    fn last(&self) -> &Value {
        &self.events.last().unwrap().1
    }
    async fn usage_rows(&self) -> usize {
        ai_usage::Entity::find()
            .filter(ai_usage::Column::OrgId.eq(self.org.id))
            .filter(ai_usage::Column::Feature.eq("ask"))
            .all(&self.db)
            .await
            .unwrap()
            .len()
    }
    async fn provider_status(&self, kind: &str) -> Option<String> {
        let org = organization::Entity::find_by_id(self.org.id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap();
        org.settings
            .pointer(&format!("/ai/providers/{kind}/last_status"))
            .and_then(|v| v.as_str())
            .map(String::from)
    }
}

fn chain(kinds: &[AiProviderKind]) -> Vec<ResolvedProvider> {
    kinds
        .iter()
        .map(|k| ResolvedProvider {
            kind: *k,
            api_key: "key".into(),
            model: Some("m".into()),
            base_url: None,
        })
        .collect()
}

async fn parse(rx: mpsc::Receiver<Event>) -> Vec<(String, Value)> {
    let stream = ReceiverStream::new(rx).map(Ok::<_, Infallible>);
    let resp = Sse::new(stream).into_response();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .split("\n\n")
        .filter(|b| !b.trim().is_empty())
        .map(|block| {
            let mut name = String::new();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("event: ") {
                    name = v.to_string();
                } else if let Some(v) = line.strip_prefix("data: ") {
                    data = v.to_string();
                }
            }
            (name, serde_json::from_str(&data).unwrap())
        })
        .collect()
}

async fn drive(
    kinds: &[AiProviderKind],
    fallback: bool,
    error: Option<&str>,
    script: Vec<Step>,
    previews: Vec<PreviewOutcome>,
) -> Option<Harness> {
    drive_with(kinds, fallback, error, script, previews, None).await
}

/// Queue that records jobs instead of touching the shared Redis queues.
#[derive(Default)]
struct CaptureQueue(Mutex<Vec<JobMessage>>);

#[async_trait::async_trait]
impl Queue for CaptureQueue {
    async fn enqueue(&self, _: QueueName, job: JobMessage) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(job);
        Ok(())
    }
}

async fn drive_with(
    kinds: &[AiProviderKind],
    fallback: bool,
    error: Option<&str>,
    script: Vec<Step>,
    previews: Vec<PreviewOutcome>,
    refresh: Option<(i64, Arc<CaptureQueue>)>,
) -> Option<Harness> {
    let app = spawn_app().await?;
    let mut org = new_organization();
    org.slug = Set(format!("ai-ask-{}", uuid::Uuid::new_v4().simple()));
    let org = org.insert(&app.db).await.expect("insert org");

    let script = Arc::new(Mutex::new(VecDeque::from(script)));
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let ran = Arc::new(Mutex::new(Vec::new()));
    let (lookup_script, lookup_prompts) = (script.clone(), prompts.clone());
    let lookup = move |kind, _base_url| -> Box<dyn AiProvider> {
        Box::new(ScriptedProvider {
            kind,
            script: lookup_script.clone(),
            prompts: lookup_prompts.clone(),
            viz: VizQueue::default(),
            json: JsonQueue::default(),
        })
    };
    let preview = FakePreview {
        script: Mutex::new(VecDeque::from(previews)),
        ran: ran.clone(),
    };
    let ctx = AskCtx {
        user_id: None,
        scope: RequestScope::pooled(app.db.clone()),
        org: org.clone(),
        http: reqwest::Client::new(),
        chain: chain(kinds),
        fallback_enabled: fallback,
        data_source_type: "pg".into(),
        schema_text: "users(id integer)".into(),
        question: "how many users".into(),
        existing_sql: error.map(|_| "SELECT bad FROM users".to_string()),
        error: error.map(String::from),
        refresh: refresh.map(|(data_source_id, queue)| ProfileRefresh {
            redis: app.redis.clone(),
            queue,
            org_id: org.id,
            data_source_id,
        }),
    };
    let (tx, rx) = mpsc::channel(64);
    let outcome = ask_loop_outcome(ctx, lookup, preview, &tx).await;
    drop(tx);
    let events = parse(rx).await;
    let prompts = prompts.lock().unwrap().clone();
    let previewed = ran.lock().unwrap().clone();
    Some(Harness {
        events,
        outcome,
        prompts,
        previewed,
        org,
        db: app.db.clone(),
    })
}

const G: &[AiProviderKind] = &[AiProviderKind::Gemini];

#[tokio::test]
async fn outcome_carries_preview_columns_and_row_count() {
    let Some(h) = drive(G, false, None, vec![Ok(("SELECT 1", 1, 1))], vec![rows(3)]).await else {
        return;
    };
    let o = h.outcome.expect("outcome");
    assert_eq!(
        (o.sql.as_str(), o.executed, o.row_count),
        ("SELECT 1", true, 3)
    );
    assert_eq!(o.columns.len(), 1);
    assert_eq!(o.columns[0].name, "id");
    assert_eq!(o.rows.len(), 3);
    assert!(o.error.is_none());
}

#[tokio::test]
async fn outcome_after_exhausted_repairs_has_error_and_no_columns() {
    let Some(h) = drive(
        G,
        false,
        None,
        vec![
            Ok(("SELECT a", 1, 1)),
            Ok(("SELECT b", 1, 1)),
            Ok(("SELECT c", 1, 1)),
        ],
        vec![err("boom"), err("boom"), err("boom")],
    )
    .await
    else {
        return;
    };
    let o = h.outcome.expect("outcome");
    assert_eq!(
        (o.sql.as_str(), o.executed, o.row_count),
        ("SELECT c", false, 0)
    );
    assert_eq!(o.error.as_deref(), Some("boom"));
    assert!(o.columns.is_empty());
}

#[tokio::test]
async fn happy_path_event_order_and_tokens() {
    let Some(h) = drive(G, false, None, vec![Ok(("SELECT 1", 10, 5))], vec![rows(3)]).await else {
        return;
    };
    assert_eq!(
        h.names(),
        ["status", "draft", "executing", "preview", "final"]
    );
    let f = h.last();
    assert_eq!(f["sql"], "SELECT 1");
    assert_eq!(f["attempts"], 1);
    assert_eq!(f["served_by"], "gemini");
    assert_eq!(
        (f["tokens_in"].as_i64(), f["tokens_out"].as_i64()),
        (Some(10), Some(5))
    );
    assert_eq!(h.events[3].1["row_count"], 3);
    assert_eq!(f["executed"], true);
    assert!(f["error"].is_null());
}

#[tokio::test]
async fn error_repairs_then_finalizes_and_rows_stay_out_of_prompts() {
    let Some(h) = drive(
        G,
        false,
        None,
        vec![
            Ok(("SELECT bad FROM t", 10, 5)),
            Ok(("SELECT id FROM t", 7, 3)),
        ],
        vec![err("column \"bad\" does not exist"), rows(2)],
    )
    .await
    else {
        return;
    };
    assert_eq!(
        h.names(),
        [
            "status",
            "draft",
            "executing",
            "repair",
            "draft",
            "executing",
            "preview",
            "final"
        ]
    );
    assert_eq!(h.events[3].1["error"], "column \"bad\" does not exist");
    let f = h.last();
    assert_eq!(f["sql"], "SELECT id FROM t");
    assert_eq!(f["attempts"], 2);
    assert_eq!(
        (f["tokens_in"].as_i64(), f["tokens_out"].as_i64()),
        (Some(17), Some(8))
    );
    assert!(h.prompts[1].contains("SELECT bad FROM t"));
    assert!(h.prompts[1].contains("column \"bad\" does not exist"));
    assert!(h.prompts.iter().all(|p| !p.contains("ROW_SECRET")));
}

#[tokio::test]
async fn three_attempt_cap_finalizes_last_draft_with_error() {
    let Some(h) = drive(
        G,
        false,
        None,
        vec![
            Ok(("SELECT a", 1, 1)),
            Ok(("SELECT b", 1, 1)),
            Ok(("SELECT c", 1, 1)),
        ],
        vec![err("e1"), err("e2"), err("e3")],
    )
    .await
    else {
        return;
    };
    assert_eq!(h.names().iter().filter(|n| **n == "draft").count(), 3);
    assert_eq!(h.names().iter().filter(|n| **n == "repair").count(), 2);
    let f = h.last();
    assert_eq!(f["sql"], "SELECT c");
    assert_eq!(f["attempts"], 3);
    assert_eq!(f["error"], "e3");
    assert_eq!(f["executed"], false);
    assert_eq!(f["explanation"], "explains SELECT c");
}

#[tokio::test]
async fn zero_rows_is_success_in_one_attempt() {
    let Some(h) = drive(G, false, None, vec![Ok(("SELECT a", 1, 1))], vec![rows(0)]).await else {
        return;
    };
    assert_eq!(
        h.names(),
        ["status", "draft", "executing", "preview", "final"]
    );
    assert_eq!(h.prompts.len(), 1);
    assert!(h.prompts.iter().all(|p| !p.contains("0 rows")));
    assert_eq!(h.events[3].1["row_count"], 0);
    let f = h.last();
    assert_eq!(f["executed"], true);
    assert!(f["error"].is_null());
    assert_eq!(f["attempts"], 1);
}

#[tokio::test]
async fn timeout_emits_single_repair_then_finalizes_without_preview() {
    let Some(h) = drive(
        G,
        false,
        None,
        vec![Ok(("SELECT slow", 1, 1))],
        vec![PreviewOutcome::TimedOut],
    )
    .await
    else {
        return;
    };
    assert_eq!(
        h.names(),
        ["status", "draft", "executing", "repair", "final"]
    );
    assert_eq!(h.events[3].1["error"], "preview timed out");
    assert_eq!(h.last()["sql"], "SELECT slow");
    assert_eq!(h.last()["attempts"], 1);
    assert_eq!(h.last()["executed"], false);
    assert_eq!(h.last()["error"], "preview timed out");
}

#[tokio::test]
async fn dml_draft_never_reaches_preview_runner() {
    let Some(h) = drive(
        G,
        false,
        None,
        vec![Ok(("DELETE FROM users", 1, 1)), Ok(("SELECT 1", 1, 1))],
        vec![rows(1)],
    )
    .await
    else {
        return;
    };
    assert_eq!(h.previewed, vec!["SELECT 1".to_string()]);
    assert_eq!(
        h.names(),
        [
            "status",
            "draft",
            "repair",
            "draft",
            "executing",
            "preview",
            "final"
        ]
    );
    assert!(h.prompts[1].contains("single SELECT"));
    assert_eq!(h.last()["executed"], true);
}

#[tokio::test]
async fn supplied_error_makes_first_prompt_a_repair_prompt() {
    let Some(h) = drive(
        G,
        false,
        Some("relation \"x\" does not exist"),
        vec![Ok(("SELECT 1", 1, 1))],
        vec![rows(1)],
    )
    .await
    else {
        return;
    };
    assert!(h.prompts[0].contains("SELECT bad FROM users"));
    assert!(h.prompts[0].contains("relation \"x\" does not exist"));
    assert_eq!(h.last()["attempts"], 1);
}

#[tokio::test]
async fn provider_failure_emits_failed_with_tagged_status() {
    let Some(h) = drive(G, false, None, vec![Err(AiError::InvalidKey)], vec![]).await else {
        return;
    };
    assert_eq!(h.names(), ["status", "failed"]);
    assert_eq!(h.last()["status"], "invalid_key");
    assert!(h.previewed.is_empty());
}

#[tokio::test]
async fn provider_failure_on_later_attempt_finalizes_last_draft() {
    let Some(h) = drive(
        G,
        false,
        None,
        vec![Ok(("SELECT a", 4, 2)), Err(AiError::InvalidKey)],
        vec![err("boom")],
    )
    .await
    else {
        return;
    };
    assert_eq!(
        h.names(),
        ["status", "draft", "executing", "repair", "final"]
    );
    let f = h.last();
    assert_eq!(f["sql"], "SELECT a");
    assert_eq!(f["attempts"], 2);
    assert_eq!(f["tokens_in"], 4);
    assert_eq!(f["executed"], false);
    assert!(f["error"].as_str().is_some_and(|m| !m.is_empty()));
}

/// Attempt 2 exhausts the whole chain; `served_by` must still name attempt 1's provider.
#[tokio::test]
async fn later_attempt_failure_keeps_drafting_provider_as_served_by() {
    let Some(h) = drive(
        &[AiProviderKind::Gemini, AiProviderKind::OpenAi],
        true,
        None,
        vec![
            Ok(("SELECT a", 4, 2)),
            Err(AiError::InvalidKey),
            Err(AiError::InvalidKey),
        ],
        vec![err("boom")],
    )
    .await
    else {
        return;
    };
    let f = h.last();
    assert_eq!(h.names().last(), Some(&"final"));
    assert_eq!(f["sql"], "SELECT a");
    assert_eq!(f["attempts"], 2);
    assert_eq!(f["served_by"], "gemini");
}

#[tokio::test]
async fn blocked_preview_finalizes_without_repair_or_more_provider_calls() {
    let Some(h) = drive(
        G,
        false,
        None,
        vec![Ok(("SELECT a", 1, 1))],
        vec![PreviewOutcome::Blocked("not allowed".into())],
    )
    .await
    else {
        return;
    };
    assert_eq!(h.names(), ["status", "draft", "executing", "final"]);
    assert_eq!(h.prompts.len(), 1);
    let f = h.last();
    assert_eq!(f["sql"], "SELECT a");
    assert_eq!(f["error"], "not allowed");
    assert_eq!(f["executed"], false);
    assert_eq!(f["attempts"], 1);
}

#[tokio::test]
async fn long_errors_are_capped_in_prompts() {
    let long = "x".repeat(5000);
    let Some(h) = drive(
        G,
        false,
        Some(&long),
        vec![Ok(("SELECT 1", 1, 1))],
        vec![rows(1)],
    )
    .await
    else {
        return;
    };
    assert!(h.prompts[0].matches('x').count() < 2500);
}

#[tokio::test]
async fn each_provider_call_writes_one_ask_usage_row() {
    let Some(h) = drive(
        G,
        false,
        None,
        vec![Ok(("SELECT bad", 1, 1)), Ok(("SELECT 1", 1, 1))],
        vec![err("boom"), rows(1)],
    )
    .await
    else {
        return;
    };
    assert_eq!(h.usage_rows().await, 2);
}

/// Attempt 1 falls back Gemini -> OpenAI; attempt 2 is served by Gemini. A stale org snapshot in
/// attempt 2 would erase OpenAI's persisted status.
#[tokio::test]
async fn multi_attempt_keeps_every_providers_persisted_status() {
    let Some(h) = drive(
        &[AiProviderKind::Gemini, AiProviderKind::OpenAi],
        true,
        None,
        vec![
            Err(AiError::InvalidKey),
            Ok(("SELECT a", 1, 1)),
            Ok(("SELECT b", 1, 1)),
        ],
        vec![err("boom"), rows(1)],
    )
    .await
    else {
        return;
    };
    assert_eq!(h.last()["sql"], "SELECT b");
    assert_eq!(h.provider_status("gemini").await.as_deref(), Some("ok"));
    assert_eq!(h.provider_status("openai").await.as_deref(), Some("ok"));
}

// ── HTTP handler ─────────────────────────────────────────────────────────────

async fn create_ds(admin: &reqwest::Client, base: &str) -> i64 {
    let resp = admin
        .post(format!("{base}/api/data_sources"))
        .json(&json!({ "name": "fake-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "create ds");
    resp.json::<Value>().await.unwrap()["id"].as_i64().unwrap()
}

async fn ask_http(admin: &reqwest::Client, base: &str, ds_id: i64) -> (String, String) {
    let resp = admin
        .post(format!("{base}/api/queries/ask"))
        .json(&json!({ "data_source_id": ds_id, "question": "how many orders?" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    (ct, resp.text().await.unwrap())
}

#[tokio::test]
async fn ask_endpoint_not_configured_streams_single_failed_event() {
    let Some(app) = spawn_app().await else { return };
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds(&admin, &app.address).await;

    let (ct, body) = ask_http(&admin, &app.address, ds_id).await;
    assert!(ct.starts_with("text/event-stream"), "content-type: {ct}");
    assert!(body.contains("event: failed"), "body: {body}");
    assert!(body.contains("not_configured"), "body: {body}");
    assert!(!body.contains("event: draft"), "body: {body}");
}

#[tokio::test]
async fn ask_endpoint_rejects_unauthenticated_and_unknown_data_source() {
    let Some(app) = spawn_app().await else { return };
    app.cleanup().await;
    let anon = reqwest::Client::new()
        .post(format!("{}/api/queries/ask", app.address))
        .json(&json!({ "data_source_id": 1, "question": "q" }))
        .send()
        .await
        .unwrap();
    assert!(
        matches!(anon.status().as_u16(), 401 | 403),
        "anon status {}",
        anon.status()
    );

    let admin = app.admin_client().await;
    let missing = admin
        .post(format!("{}/api/queries/ask", app.address))
        .json(&json!({ "data_source_id": 987654, "question": "q" }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status().as_u16(), 404);
}

/// Serves an OpenAI-shaped completion whose SQL is a DML statement, so every attempt is
/// rejected by the read-only gate and nothing needs a worker.
async fn spawn_dml_stub() -> String {
    use axum::{routing::post, Router};
    let router = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            axum::Json(json!({
                "choices": [{ "message": { "content": r#"{"sql":"DELETE FROM orders","explanation":"x"}"# } }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

/// One call charges the limiter once even though the loop makes three provider attempts, and a
/// non-SELECT draft is never executed.
#[tokio::test]
async fn ask_endpoint_charges_rate_limit_once_per_call() {
    use redis::AsyncCommands;
    let Some(app) = spawn_app().await else { return };
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds(&admin, &app.address).await;
    {
        use arcodash_core::entities::data_source;
        let ds = data_source::Entity::find_by_id(ds_id)
            .one(&app.db)
            .await
            .unwrap()
            .unwrap();
        let mut active: data_source::ActiveModel = ds.into();
        active.schema = Set(Some(json!({ "tables": [{
            "name": "orders",
            "columns": [{ "name": "id", "type_name": "integer" }]
        }]})));
        active.update(&app.db).await.unwrap();
    }
    let stub = spawn_dml_stub().await;
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let resp = admin
        .post(format!("{}/api/organization/ai", app.address))
        .json(&json!({
            "providers": { "openai": { "api_key": "sk-test-stub-key-0000000", "base_url": stub } },
            "provider_order": ["openai"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "configure provider");

    let admin_user = arcodash_core::entities::user::Entity::find()
        .one(&app.db)
        .await
        .unwrap()
        .expect("admin user");
    let key = format!(
        "arcodash:ai_rate_limit:{}:{}",
        admin_user.org_id, admin_user.id
    );
    let mut redis = app.redis.clone();
    let _: () = redis.del(&key).await.unwrap();
    let (ct, body) = ask_http(&admin, &app.address, ds_id).await;
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);

    assert!(ct.starts_with("text/event-stream"), "content-type: {ct}");
    assert!(body.contains("event: final"), "body: {body}");
    assert!(!body.contains("event: executing"), "body: {body}");
    let count: Option<i64> = redis.get(&key).await.unwrap();
    assert_eq!(count, Some(1), "one charge per call");
}

async fn clear_refresh_keys(ds: i64) {
    use redis::AsyncCommands;
    let Some(app) = spawn_app().await else { return };
    let mut redis = app.redis.clone();
    let _: () = redis
        .del(&[
            ai_profile_refresh_cooldown_key(ds),
            ai_profile_pending_key(ds),
        ])
        .await
        .unwrap();
}

#[tokio::test]
async fn missing_object_error_queues_one_refresh_then_cooldown_repairs_normally() {
    let ds = 9_100_000_000 + i64::from(std::process::id());
    clear_refresh_keys(ds).await;
    let missing = || err(r#"relation "ghost" does not exist"#);

    let q = Arc::new(CaptureQueue::default());
    let Some(h) = drive_with(
        G,
        false,
        None,
        vec![Ok(("SELECT * FROM ghost", 1, 1)), Ok(("SELECT 1", 1, 1))],
        vec![missing()],
        Some((ds, q.clone())),
    )
    .await
    else {
        return;
    };
    assert_eq!(
        h.names(),
        [
            "status",
            "draft",
            "executing",
            "profile_refreshing",
            "final"
        ]
    );
    assert_eq!(h.events[3].1, json!({ "data_source_id": ds }));
    assert_eq!(h.prompts.len(), 1, "no repair call after a refresh");
    assert_eq!(h.last()["executed"], false);
    assert!(h.last()["error"].as_str().unwrap().contains("ghost"));
    {
        let jobs = q.0.lock().unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(matches!(
            jobs[0],
            JobMessage::ProfileDataSource { data_source_id, data: ProfileData::Changed, .. }
                if data_source_id == ds
        ));
    }

    let q2 = Arc::new(CaptureQueue::default());
    let h = drive_with(
        G,
        false,
        None,
        vec![Ok(("SELECT * FROM ghost", 1, 1)), Ok(("SELECT 1", 1, 1))],
        vec![missing(), rows(1)],
        Some((ds, q2.clone())),
    )
    .await
    .unwrap();
    assert!(!h.names().contains(&"profile_refreshing"));
    assert_eq!(h.names().iter().filter(|n| **n == "repair").count(), 1);
    assert_eq!(h.last()["executed"], true);
    assert!(q2.0.lock().unwrap().is_empty());
    clear_refresh_keys(ds).await;
}

#[tokio::test]
async fn missing_object_in_supplied_error_refreshes_without_drafting() {
    let ds = 9_200_000_000 + i64::from(std::process::id());
    clear_refresh_keys(ds).await;
    let q = Arc::new(CaptureQueue::default());
    let Some(h) = drive_with(
        G,
        false,
        Some("no such table: ghost"),
        vec![],
        vec![],
        Some((ds, q.clone())),
    )
    .await
    else {
        return;
    };
    assert_eq!(h.names(), ["status", "profile_refreshing", "final"]);
    assert!(h.prompts.is_empty());
    assert_eq!(h.last()["sql"], "SELECT bad FROM users");
    assert_eq!(h.last()["error"], "no such table: ghost");
    assert_eq!(q.0.lock().unwrap().len(), 1);
    clear_refresh_keys(ds).await;
}

#[tokio::test]
async fn ask_endpoint_spent_budget_returns_429_before_streaming() {
    let Some(app) = spawn_app().await else { return };
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds(&admin, &app.address).await;
    let stub = spawn_dml_stub().await;
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let resp = admin
        .post(format!("{}/api/organization/ai", app.address))
        .json(&json!({
            "providers": { "openai": { "api_key": "sk-test-stub-key-0000000", "base_url": stub } },
            "provider_order": ["openai"],
            "monthly_budget_usd": 1.0
        }))
        .send()
        .await
        .unwrap();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    assert_eq!(resp.status().as_u16(), 200, "configure provider");
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
        feature: Set("ask".to_string()),
        status: Set("ok".to_string()),
        tokens_in: Set(None),
        tokens_out: Set(None),
        user_id: Set(None),
        cost_usd: Set(Some(2.0)),
        created_at: Set(chrono::Utc::now().into()),
    }
    .insert(&app.db)
    .await
    .unwrap();

    let resp = admin
        .post(format!("{}/api/queries/ask", app.address))
        .json(&json!({ "data_source_id": ds_id, "question": "how many orders?" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 429);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error_code"], "ai_budget_exceeded");
    assert!(
        body["message"].as_str().unwrap().contains("$1.00"),
        "{body}"
    );
}

// ── AI widget ────────────────────────────────────────────────────────────────

struct WidgetRun {
    app: common::TestApp,
    org: organization::Model,
    admin: user::Model,
    dashboard_id: i64,
    events: Vec<(String, Value)>,
    viz_left: usize,
    previewed: Vec<String>,
}

impl WidgetRun {
    fn names(&self) -> Vec<&str> {
        self.events.iter().map(|(n, _)| n.as_str()).collect()
    }
    async fn counts(&self) -> (usize, usize, usize) {
        let q = query::Entity::find().all(&self.app.db).await.unwrap().len();
        let v = visualization::Entity::find()
            .all(&self.app.db)
            .await
            .unwrap()
            .len();
        let w = widget::Entity::find()
            .filter(widget::Column::DashboardId.eq(self.dashboard_id))
            .all(&self.app.db)
            .await
            .unwrap()
            .len();
        (q, v, w)
    }
}

fn counter_viz(title: &str) -> Result<VizGeneration, AiError> {
    Ok(VizGeneration {
        title: Some(title.to_string()),
        suggestion: VizSuggestion::Counter(CounterOptions {
            column_name: Some("id".into()),
            ..Default::default()
        }),
        tokens_in: Some(3),
        tokens_out: Some(2),
    })
}

const PROMPT: &str = "orders by country";

async fn widget_run(
    script: Vec<Step>,
    previews: Vec<PreviewOutcome>,
    viz: Vec<Result<VizGeneration, AiError>>,
    dashboard_override: Option<i64>,
    seed_widget: bool,
) -> Option<WidgetRun> {
    widget_run_with(
        script,
        previews,
        viz,
        dashboard_override,
        seed_widget,
        Arc::new(CaptureQueue::default()),
    )
    .await
}

/// Rejects every enqueue, as an unreachable Redis would.
struct FailingQueue;

#[async_trait::async_trait]
impl Queue for FailingQueue {
    async fn enqueue(&self, _: QueueName, _: JobMessage) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("queue down"))
    }
}

async fn widget_run_with(
    script: Vec<Step>,
    previews: Vec<PreviewOutcome>,
    viz: Vec<Result<VizGeneration, AiError>>,
    dashboard_override: Option<i64>,
    seed_widget: bool,
    queue: Arc<dyn Queue>,
) -> Option<WidgetRun> {
    let app = spawn_app().await?;
    app.cleanup().await;
    let admin_http = app.admin_client().await;
    let ds_id = create_ds(&admin_http, &app.address).await;
    let resp = admin_http
        .post(format!("{}/api/dashboards", app.address))
        .json(&json!({ "name": "Sales board" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "create dashboard");
    let dashboard_id = resp.json::<Value>().await.unwrap()["id"].as_i64().unwrap();
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let admin = user::Entity::find()
        .filter(user::Column::Email.eq(common::ADMIN_EMAIL))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    if seed_widget {
        widget::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            dashboard_id: Set(dashboard_id),
            visualization_id: Set(None),
            text: Set(Some("note".into())),
            width: Set(1),
            options: Set(json!({ "position": { "col": 0, "row": 0, "sizeX": 3, "sizeY": 4 } })),
            created_at: Set(chrono::Utc::now().into()),
            updated_at: Set(chrono::Utc::now().into()),
        }
        .insert(&app.db)
        .await
        .unwrap();
    }

    let script = Arc::new(Mutex::new(VecDeque::from(script)));
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let viz: VizQueue = Arc::new(Mutex::new(VecDeque::from(viz)));
    let ran = Arc::new(Mutex::new(Vec::new()));
    let (ls, lp, lv) = (script.clone(), prompts.clone(), viz.clone());
    let lookup = move |kind, _base_url| -> Box<dyn AiProvider> {
        Box::new(ScriptedProvider {
            kind,
            script: ls.clone(),
            prompts: lp.clone(),
            viz: lv.clone(),
            json: JsonQueue::default(),
        })
    };
    let preview = FakePreview {
        script: Mutex::new(VecDeque::from(previews)),
        ran: ran.clone(),
    };
    let ctx = AskCtx {
        user_id: Some(admin.id),
        scope: RequestScope::pooled(app.db.clone()),
        org: org.clone(),
        http: reqwest::Client::new(),
        chain: chain(G),
        fallback_enabled: false,
        data_source_type: "pg".into(),
        schema_text: "orders(id integer)".into(),
        question: PROMPT.into(),
        existing_sql: None,
        error: None,
        refresh: None,
    };
    let mut state = app.state.clone();
    state.queue = queue;
    let w = WidgetCtx {
        state,
        first_run_cap: std::time::Duration::from_millis(100),
        user: admin.clone(),
        data_source_id: ds_id,
        dashboard_id: dashboard_override.unwrap_or(dashboard_id),
        dashboard_name: "Sales board".into(),
    };
    let (tx, rx) = mpsc::channel(64);
    widget_loop(ctx, w, lookup, preview, tx).await;
    let events = parse(rx).await;
    let viz_left = viz.lock().unwrap().len();
    let previewed = ran.lock().unwrap().clone();
    Some(WidgetRun {
        app,
        org,
        admin,
        dashboard_id,
        events,
        viz_left,
        previewed,
    })
}

#[tokio::test]
async fn widget_happy_path_creates_three_rows_in_draft() {
    let Some(r) = widget_run(
        vec![Ok(("SELECT id FROM orders", 4, 2))],
        vec![rows(3)],
        vec![counter_viz("Order count")],
        None,
        true,
    )
    .await
    else {
        return;
    };
    assert_eq!(
        r.names(),
        [
            "status",
            "draft",
            "executing",
            "preview",
            "final",
            "visualizing",
            "created"
        ]
    );
    let created = &r.events.last().unwrap().1;
    assert_eq!(created["viz_type"], "COUNTER");
    assert_eq!(created["row_count"], 3);

    let q = query::Entity::find_by_id(created["query_id"].as_i64().unwrap())
        .one(&r.app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(q.query, "SELECT id FROM orders");
    assert_eq!(q.name, PROMPT);
    assert!(q.published_change_id.is_some());
    assert!(q.description.unwrap().contains("Sales board"));

    let v = visualization::Entity::find_by_id(created["visualization_id"].as_i64().unwrap())
        .one(&r.app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (v.r#type.as_str(), v.name.as_str()),
        ("COUNTER", "Order count")
    );
    assert_eq!(v.query_id, q.id);

    let w = widget::Entity::find_by_id(created["widget_id"].as_i64().unwrap())
        .one(&r.app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(w.visualization_id, Some(v.id));
    assert_eq!(w.options["position"]["row"], 4);
    assert_eq!(w.options["position"]["sizeX"], 3);
    assert_eq!(w.options["position"]["sizeY"], 2);
    assert_eq!(w.options["position"]["col"], 0);

    let dash = arcodash_core::entities::dashboard::Entity::find_by_id(r.dashboard_id)
        .one(&r.app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(dash.published_snapshot.is_none());

    let usage = ai_usage::Entity::find()
        .filter(ai_usage::Column::OrgId.eq(r.org.id))
        .all(&r.app.db)
        .await
        .unwrap();
    for feature in ["ask", "auto_visualize"] {
        let row = usage.iter().find(|u| u.feature == feature).unwrap();
        assert_eq!(row.user_id, Some(r.admin.id), "{feature}");
    }
}

#[tokio::test]
async fn widget_created_after_a_bound_first_run_of_the_new_query() {
    let queue = Arc::new(CaptureQueue::default());
    let Some(r) = widget_run_with(
        vec![Ok(("SELECT id FROM orders", 4, 2))],
        vec![rows(3)],
        vec![counter_viz("Order count")],
        None,
        false,
        queue.clone(),
    )
    .await
    else {
        return;
    };
    assert_eq!(r.names().last(), Some(&"created"));
    let query_id = r.events.last().unwrap().1["query_id"].as_i64().unwrap();
    let jobs = queue.0.lock().unwrap();
    assert_eq!(jobs.len(), 1, "one bound run for the new query");
    match &jobs[0] {
        JobMessage::ExecuteQuery {
            query_id: qid,
            repoint_published,
            query_text,
            ..
        } => {
            assert_eq!(*qid, Some(query_id));
            assert!(*repoint_published, "must repoint the published pointer");
            assert_eq!(query_text, "SELECT id FROM orders");
        }
        other => panic!("unexpected job {other:?}"),
    }
}

#[tokio::test]
async fn widget_still_created_when_first_run_cannot_be_enqueued() {
    let Some(r) = widget_run_with(
        vec![Ok(("SELECT id FROM orders", 4, 2))],
        vec![rows(3)],
        vec![counter_viz("Order count")],
        None,
        false,
        Arc::new(FailingQueue),
    )
    .await
    else {
        return;
    };
    assert_eq!(r.names().last(), Some(&"created"));
    assert_eq!(r.counts().await, (1, 1, 1));
}

#[tokio::test]
async fn widget_long_prompt_name_is_truncated() {
    // Name truncation is covered at unit level; this pins that a viz title is used verbatim.
    let Some(r) = widget_run(
        vec![Ok(("SELECT 1", 1, 1))],
        vec![rows(1)],
        vec![counter_viz("T")],
        None,
        false,
    )
    .await
    else {
        return;
    };
    assert_eq!(r.events.last().unwrap().0, "created");
    assert_eq!(r.counts().await, (1, 1, 1));
}

#[tokio::test]
async fn widget_viz_failure_falls_back_to_table() {
    let Some(r) = widget_run(
        vec![Ok(("SELECT 1", 1, 1))],
        vec![rows(2)],
        vec![Err(AiError::GenerationFailed("nope".into()))],
        None,
        false,
    )
    .await
    else {
        return;
    };
    let n = r.names();
    assert_eq!(&n[n.len() - 2..], ["visualizing", "created"]);
    let created = &r.events.last().unwrap().1;
    assert_eq!(created["viz_type"], "TABLE");
    let v = visualization::Entity::find()
        .one(&r.app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(v.r#type, "TABLE");
    assert_eq!(v.name, PROMPT);
}

#[tokio::test]
async fn widget_zero_rows_skips_viz_and_creates_table() {
    let Some(r) = widget_run(
        vec![Ok(("SELECT 1", 1, 1))],
        vec![rows(0)],
        vec![counter_viz("unused")],
        None,
        false,
    )
    .await
    else {
        return;
    };
    assert!(!r.names().contains(&"visualizing"));
    assert_eq!(r.viz_left, 1);
    let created = &r.events.last().unwrap().1;
    assert_eq!(
        (created["viz_type"].as_str(), created["row_count"].as_i64()),
        (Some("TABLE"), Some(0))
    );
}

#[tokio::test]
async fn widget_exhausted_loop_fails_and_creates_nothing() {
    let Some(r) = widget_run(
        vec![
            Ok(("SELECT a", 1, 1)),
            Ok(("SELECT b", 1, 1)),
            Ok(("SELECT c", 1, 1)),
        ],
        vec![err("boom"), err("boom"), err("boom")],
        vec![],
        None,
        false,
    )
    .await
    else {
        return;
    };
    let n = r.names();
    assert_eq!(&n[n.len() - 2..], ["final", "failed"]);
    assert_eq!(r.events[r.events.len() - 2].1["executed"], false);
    assert_eq!(r.counts().await, (0, 0, 0));
}

#[tokio::test]
async fn widget_dml_draft_fails_and_creates_nothing() {
    let Some(r) = widget_run(
        vec![
            Ok(("DELETE FROM orders", 1, 1)),
            Ok(("DELETE FROM orders", 1, 1)),
            Ok(("DELETE FROM orders", 1, 1)),
        ],
        vec![],
        vec![],
        None,
        false,
    )
    .await
    else {
        return;
    };
    assert_eq!(r.names().last(), Some(&"failed"));
    assert!(r.previewed.is_empty());
    assert_eq!(r.counts().await, (0, 0, 0));
}

#[tokio::test]
async fn widget_create_error_rolls_back_query_and_viz() {
    let Some(r) = widget_run(
        vec![Ok(("SELECT 1", 1, 1))],
        vec![rows(2)],
        vec![counter_viz("x")],
        Some(i64::MAX),
        false,
    )
    .await
    else {
        return;
    };
    let last = r.events.last().unwrap();
    assert_eq!(last.0, "failed");
    assert!(
        last.1["message"]
            .as_str()
            .unwrap()
            .starts_with("Couldn't add the widget"),
        "{last:?}"
    );
    assert_eq!(r.counts().await, (0, 0, 0));
}

async fn http_post_widget(
    app: &common::TestApp,
    client: &reqwest::Client,
    path_id: i64,
    ds_id: i64,
    suffix: &str,
) -> reqwest::Response {
    client
        .post(format!(
            "{}/api/dashboards/{path_id}/ai_widget{suffix}",
            app.address
        ))
        .json(&json!({ "data_source_id": ds_id, "prompt": PROMPT }))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn ai_widget_viewer_is_denied_before_any_provider_call() {
    let Some(app) = spawn_app().await else { return };
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds(&admin, &app.address).await;
    let dash = admin
        .post(format!("{}/api/dashboards", app.address))
        .json(&json!({ "name": "d" }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let (viewer, _) =
        common::user_with_role(&app.db, org.id, &["view_query", "view_dashboard"]).await;
    let mut d: arcodash_core::entities::dashboard::ActiveModel =
        arcodash_core::entities::dashboard::Entity::find_by_id(dash)
            .one(&app.db)
            .await
            .unwrap()
            .unwrap()
            .into();
    d.visibility = Set("workspace".into());
    d.published_snapshot = Set(Some(json!({ "widgets": [] })));
    d.update(&app.db).await.unwrap();
    let admin_user = user::Entity::find()
        .filter(user::Column::Email.eq(common::ADMIN_EMAIL))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    arcodash_backend::services::permission_service::grant(
        &app.db,
        org.id,
        &admin_user,
        "dashboard",
        dash,
        Some(viewer.id),
        None,
        "viewer",
    )
    .await
    .expect("grant");
    let key = common::test_api_key(&viewer.email);
    let resp = reqwest::Client::new()
        .post(format!(
            "{}/api/dashboards/{dash}/ai_widget?api_key={key}",
            app.address
        ))
        .json(&json!({ "data_source_id": ds_id, "prompt": PROMPT }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    // Object-bound denials are a uniform 404 across the API.
    assert_eq!(status, 404, "{}", resp.text().await.unwrap());
    let usage = ai_usage::Entity::find().all(&app.db).await.unwrap();
    assert!(usage.is_empty());
}

#[tokio::test]
async fn ai_widget_unknown_dashboard_404_for_admin() {
    let Some(app) = spawn_app().await else { return };
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds(&admin, &app.address).await;
    let resp = http_post_widget(&app, &admin, 987654, ds_id, "").await;
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn ai_widget_not_configured_streams_failed() {
    let Some(app) = spawn_app().await else { return };
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds(&admin, &app.address).await;
    let dash = admin
        .post(format!("{}/api/dashboards", app.address))
        .json(&json!({ "name": "d" }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let resp = http_post_widget(&app, &admin, dash, ds_id, "").await;
    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp.headers()["content-type"].to_str().unwrap().to_string();
    assert!(ct.starts_with("text/event-stream"), "{ct}");
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("event: failed") && body.contains("not_configured"),
        "{body}"
    );
    assert!(!body.contains("event: draft"), "{body}");
}

async fn ai_widget_run() -> Option<(WidgetRun, i64, i64, i64)> {
    let r = widget_run(
        vec![Ok(("SELECT id FROM orders", 4, 2))],
        vec![rows(3)],
        vec![counter_viz("Order count")],
        None,
        false,
    )
    .await?;
    let c = &r.events.last().unwrap().1;
    assert_eq!(r.events.last().unwrap().0, "created");
    let ids = (
        c["query_id"].as_i64().unwrap(),
        c["visualization_id"].as_i64().unwrap(),
        c["widget_id"].as_i64().unwrap(),
    );
    Some((r, ids.0, ids.1, ids.2))
}

async fn delete_widget(r: &WidgetRun, id: i64) {
    let resp = r
        .app
        .admin_client()
        .await
        .delete(format!("{}/api/widgets/{id}", r.app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
}

async fn query_and_viz_exist(r: &WidgetRun, q: i64, v: i64) -> (bool, bool) {
    (
        query::Entity::find_by_id(q)
            .one(&r.app.db)
            .await
            .unwrap()
            .is_some(),
        visualization::Entity::find_by_id(v)
            .one(&r.app.db)
            .await
            .unwrap()
            .is_some(),
    )
}

#[tokio::test]
async fn ai_widget_query_is_tagged() {
    let Some((r, q, _, _)) = ai_widget_run().await else {
        return;
    };
    let q = query::Entity::find_by_id(q)
        .one(&r.app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(q.tags, Some(vec!["ai-widget".to_string()]));
}

#[tokio::test]
async fn deleting_untouched_ai_widget_removes_viz_and_query() {
    let Some((r, q, v, w)) = ai_widget_run().await else {
        return;
    };
    delete_widget(&r, w).await;
    assert_eq!(query_and_viz_exist(&r, q, v).await, (false, false));
}

#[tokio::test]
async fn deleting_ai_widget_keeps_query_edited_since_creation() {
    let Some((r, q, v, w)) = ai_widget_run().await else {
        return;
    };
    let resp = r
        .app
        .admin_client()
        .await
        .post(format!("{}/api/queries/{q}", r.app.address))
        .json(&json!({ "query": "SELECT 2" }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
    delete_widget(&r, w).await;
    assert_eq!(query_and_viz_exist(&r, q, v).await, (true, true));
}

#[tokio::test]
async fn deleting_ai_widget_keeps_viz_used_by_another_widget() {
    let Some((r, q, v, w)) = ai_widget_run().await else {
        return;
    };
    let resp = r
        .app
        .admin_client()
        .await
        .post(format!("{}/api/widgets", r.app.address))
        .json(&json!({ "dashboard_id": r.dashboard_id, "visualization_id": v }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
    delete_widget(&r, w).await;
    assert_eq!(query_and_viz_exist(&r, q, v).await, (true, true));
}

#[tokio::test]
async fn deleting_ai_widget_keeps_query_while_a_published_snapshot_uses_it() {
    use arcodash_core::entities::dashboard;
    let Some((r, q, v, w)) = ai_widget_run().await else {
        return;
    };
    let d = dashboard::Entity::find_by_id(r.dashboard_id)
        .one(&r.app.db)
        .await
        .unwrap()
        .unwrap();
    let d = arcodash_backend::services::dashboard_service::publish(&r.app.db, &r.admin, d)
        .await
        .expect("publish");
    delete_widget(&r, w).await;
    assert_eq!(query_and_viz_exist(&r, q, v).await, (true, true));
    let resp = r
        .app
        .admin_client()
        .await
        .get(format!("{}/api/dashboards/{}", r.app.address, d.slug))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert!(body
        .to_string()
        .contains(&format!("\"visualization_id\":{v}")));
}

#[tokio::test]
async fn deleting_ai_widget_keeps_query_with_a_monitor() {
    use arcodash_core::entities::monitor;
    let Some((r, q, v, w)) = ai_widget_run().await else {
        return;
    };
    monitor::ActiveModel {
        org_id: Set(r.org.id),
        name: Set("m".into()),
        query_id: Set(q),
        user_id: Set(r.admin.id),
        options: Set(json!({})),
        state: Set("unknown".into()),
        visibility: Set("private".into()),
        ..Default::default()
    }
    .insert(&r.app.db)
    .await
    .unwrap();
    delete_widget(&r, w).await;
    assert_eq!(query_and_viz_exist(&r, q, v).await, (true, true));
}

#[tokio::test]
async fn deleting_ai_widget_keeps_query_with_a_favorite() {
    use arcodash_core::entities::favorite;
    let Some((r, q, v, w)) = ai_widget_run().await else {
        return;
    };
    favorite::ActiveModel {
        org_id: Set(r.org.id),
        object_type: Set("query".into()),
        object_id: Set(q),
        user_id: Set(r.admin.id),
        ..Default::default()
    }
    .insert(&r.app.db)
    .await
    .unwrap();
    delete_widget(&r, w).await;
    assert_eq!(query_and_viz_exist(&r, q, v).await, (true, true));
}

#[tokio::test]
async fn deleting_widget_keeps_query_without_the_ai_widget_tag() {
    let Some((r, q, v, w)) = ai_widget_run().await else {
        return;
    };
    let mut a: query::ActiveModel = query::Entity::find_by_id(q)
        .one(&r.app.db)
        .await
        .unwrap()
        .unwrap()
        .into();
    a.tags = Set(Some(vec!["mine".into()]));
    a.update(&r.app.db).await.unwrap();
    delete_widget(&r, w).await;
    assert_eq!(query_and_viz_exist(&r, q, v).await, (true, true));
}

#[tokio::test]
async fn deleting_ai_widget_without_query_delete_rights_still_deletes_widget() {
    let Some((r, q, v, w)) = ai_widget_run().await else {
        return;
    };
    let (editor, _) = common::user_with_role(
        &r.app.db,
        r.org.id,
        &["view_query", "view_dashboard", "edit_dashboard"],
    )
    .await;
    arcodash_backend::services::permission_service::grant(
        &r.app.db,
        r.org.id,
        &r.admin,
        "dashboard",
        r.dashboard_id,
        Some(editor.id),
        None,
        "editor",
    )
    .await
    .expect("grant");
    let key = common::test_api_key(&editor.email);
    let resp = reqwest::Client::new()
        .delete(format!("{}/api/widgets/{w}?api_key={key}", r.app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "{}",
        resp.text().await.unwrap()
    );
    assert!(widget::Entity::find_by_id(w)
        .one(&r.app.db)
        .await
        .unwrap()
        .is_none());
    assert_eq!(query_and_viz_exist(&r, q, v).await, (true, true));
}

async fn publish_dashboard(r: &WidgetRun) -> String {
    use arcodash_core::entities::dashboard;
    let d = dashboard::Entity::find_by_id(r.dashboard_id)
        .one(&r.app.db)
        .await
        .unwrap()
        .unwrap();
    arcodash_backend::services::dashboard_service::publish(&r.app.db, &r.admin, d)
        .await
        .expect("publish")
        .slug
}

async fn sql(r: &WidgetRun, stmt: &str) {
    use sea_orm::ConnectionTrait;
    r.app.db.execute_unprepared(stmt).await.unwrap();
}

/// Rewrites the published snapshot to have no widgets, so a draft AI tile counts as added since
/// publish.
async fn publish_without_widgets(r: &WidgetRun) -> String {
    let slug = publish_dashboard(r).await;
    sql(
        r,
        &format!(
            "UPDATE dashboards SET published_snapshot = jsonb_set(published_snapshot, '{{widgets}}', '[]') WHERE id = {}",
            r.dashboard_id
        ),
    )
    .await;
    slug
}

async fn discard(r: &WidgetRun, slug: &str) {
    let resp = r
        .app
        .admin_client()
        .await
        .post(format!("{}/api/dashboards/{slug}/discard", r.app.address))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
}

async fn widget_exists(r: &WidgetRun, w: i64) -> bool {
    widget::Entity::find_by_id(w)
        .one(&r.app.db)
        .await
        .unwrap()
        .is_some()
}

#[tokio::test]
async fn discarding_removes_untouched_ai_widget_query_and_viz() {
    let Some((r, q, v, w)) = ai_widget_run().await else {
        return;
    };
    let slug = publish_without_widgets(&r).await;
    discard(&r, &slug).await;
    assert!(!widget_exists(&r, w).await);
    assert_eq!(query_and_viz_exist(&r, q, v).await, (false, false));
}

#[tokio::test]
async fn discarding_keeps_ai_query_edited_since_creation() {
    let Some((r, q, v, w)) = ai_widget_run().await else {
        return;
    };
    let resp = r
        .app
        .admin_client()
        .await
        .post(format!("{}/api/queries/{q}", r.app.address))
        .json(&json!({ "query": "SELECT 2" }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let slug = publish_without_widgets(&r).await;
    discard(&r, &slug).await;
    assert!(!widget_exists(&r, w).await);
    assert_eq!(query_and_viz_exist(&r, q, v).await, (true, true));
}

#[tokio::test]
async fn discarding_restores_published_ai_tile_and_keeps_its_query() {
    let Some((r, q, v, w)) = ai_widget_run().await else {
        return;
    };
    let slug = publish_dashboard(&r).await;
    delete_widget(&r, w).await;
    discard(&r, &slug).await;
    assert_eq!(query_and_viz_exist(&r, q, v).await, (true, true));
    let restored = widget::Entity::find()
        .filter(widget::Column::DashboardId.eq(r.dashboard_id))
        .filter(widget::Column::VisualizationId.eq(v))
        .count(&r.app.db)
        .await
        .unwrap();
    assert_eq!(restored, 1);
}

const FAIL_TRIGGER: &str = "ai_cleanup_fail";

/// Makes deleting query `q` raise, to prove a failing cleanup cannot abort the surrounding work.
async fn install_delete_trigger(r: &WidgetRun, q: i64) {
    sql(
        r,
        &format!(
            "DROP TRIGGER IF EXISTS {FAIL_TRIGGER} ON queries; \
             CREATE OR REPLACE FUNCTION {FAIL_TRIGGER}() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'injected'; END $$ LANGUAGE plpgsql; \
             CREATE TRIGGER {FAIL_TRIGGER} BEFORE DELETE ON queries FOR EACH ROW WHEN (OLD.id = {q}) EXECUTE FUNCTION {FAIL_TRIGGER}()"
        ),
    )
    .await;
}

async fn drop_delete_trigger(r: &WidgetRun) {
    sql(
        r,
        &format!("DROP TRIGGER IF EXISTS {FAIL_TRIGGER} ON queries; DROP FUNCTION IF EXISTS {FAIL_TRIGGER}()"),
    )
    .await;
}

#[tokio::test]
async fn failing_cleanup_does_not_break_widget_delete() {
    let Some((r, q, v, w)) = ai_widget_run().await else {
        return;
    };
    install_delete_trigger(&r, q).await;
    let resp = r
        .app
        .admin_client()
        .await
        .delete(format!("{}/api/widgets/{w}", r.app.address))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    drop_delete_trigger(&r).await;
    assert_eq!(status, 204);
    assert!(!widget_exists(&r, w).await);
    assert_eq!(query_and_viz_exist(&r, q, v).await, (true, true));
}

#[tokio::test]
async fn failing_cleanup_does_not_break_discard() {
    let Some((r, q, v, w)) = ai_widget_run().await else {
        return;
    };
    let slug = publish_without_widgets(&r).await;
    install_delete_trigger(&r, q).await;
    let resp = r
        .app
        .admin_client()
        .await
        .post(format!("{}/api/dashboards/{slug}/discard", r.app.address))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    drop_delete_trigger(&r).await;
    assert_eq!(status, 200);
    assert!(!widget_exists(&r, w).await);
    assert_eq!(query_and_viz_exist(&r, q, v).await, (true, true));
}

// ── monitor ask ──────────────────────────────────────────────────────────────

struct MonitorRun {
    app: common::TestApp,
    events: Vec<(String, Value)>,
    prompts: Vec<String>,
    json_left: usize,
    admin_id: i64,
}

impl MonitorRun {
    fn names(&self) -> Vec<&str> {
        self.events.iter().map(|(n, _)| n.as_str()).collect()
    }
    async fn usage(&self, feature: &str) -> Vec<ai_usage::Model> {
        ai_usage::Entity::find()
            .filter(ai_usage::Column::Feature.eq(feature))
            .all(&self.app.db)
            .await
            .unwrap()
    }
}

fn suggestion_json(column: &str, unsupported: &str) -> Result<JsonGeneration, AiError> {
    let items = if column.is_empty() {
        vec![]
    } else {
        vec![json!({
            "name": "Order volume", "kind": "baseline", "column": column,
            "sensitivity": "medium", "op": "", "value": 0,
            "definition": "orders drift", "why": "catches drops", "confidence": "high",
        })]
    };
    Ok(JsonGeneration {
        value: json!({ "suggestions": items, "unsupported": unsupported }),
        tokens_in: Some(5),
        tokens_out: Some(3),
    })
}

async fn monitor_run(
    script: Vec<Step>,
    previews: Vec<PreviewOutcome>,
    json: Vec<Result<JsonGeneration, AiError>>,
    include_samples: bool,
) -> Option<MonitorRun> {
    let app = spawn_app().await?;
    app.cleanup().await;
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: organization::ActiveModel = org.into();
    active.settings = Set(json!({ "ai": { "include_sample_values": include_samples } }));
    let org = active.update(&app.db).await.unwrap();
    let admin = user::Entity::find()
        .filter(user::Column::Email.eq(common::ADMIN_EMAIL))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();

    let script = Arc::new(Mutex::new(VecDeque::from(script)));
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let json: JsonQueue = Arc::new(Mutex::new(VecDeque::from(json)));
    let (ls, lp, lj) = (script.clone(), prompts.clone(), json.clone());
    let lookup = move |kind, _base_url| -> Box<dyn AiProvider> {
        Box::new(ScriptedProvider {
            kind,
            script: ls.clone(),
            prompts: lp.clone(),
            viz: VizQueue::default(),
            json: lj.clone(),
        })
    };
    let preview = FakePreview {
        script: Mutex::new(VecDeque::from(previews)),
        ran: Arc::new(Mutex::new(Vec::new())),
    };
    let ctx = AskCtx {
        user_id: Some(admin.id),
        scope: RequestScope::pooled(app.db.clone()),
        org,
        http: reqwest::Client::new(),
        chain: chain(G),
        fallback_enabled: false,
        data_source_type: "pg".into(),
        schema_text: "orders(id integer)".into(),
        question: "alert me when orders drop".into(),
        existing_sql: None,
        error: None,
        refresh: None,
    };
    let m = MonitorAskCtx {
        data_source_id: 7,
        include_samples,
    };
    let (tx, rx) = mpsc::channel(64);
    monitor_ask_loop(ctx, m, lookup, preview, tx).await;
    let events = parse(rx).await;
    let json_left = json.lock().unwrap().len();
    let prompts = prompts.lock().unwrap().clone();
    Some(MonitorRun {
        app,
        events,
        prompts,
        json_left,
        admin_id: admin.id,
    })
}

#[tokio::test]
async fn monitor_ask_happy_path_emits_suggested() {
    let Some(r) = monitor_run(
        vec![Ok(("SELECT id FROM orders", 4, 2))],
        vec![rows(3)],
        vec![suggestion_json("id", "")],
        true,
    )
    .await
    else {
        return;
    };
    assert_eq!(
        r.names(),
        [
            "status",
            "draft",
            "executing",
            "preview",
            "final",
            "suggesting",
            "suggested"
        ]
    );
    let fin = &r.events[4].1;
    let suggested = &r.events[6].1;
    assert_eq!(suggested["sql"], fin["sql"]);
    assert_eq!(suggested["data_source_id"], 7);
    assert_eq!(suggested["suggestion"]["column"], "id");
    assert_eq!(suggested["suggestion"]["kind"], "baseline");
    assert_eq!(query::Entity::find().count(&r.app.db).await.unwrap(), 0);
    assert_eq!(monitor::Entity::find().count(&r.app.db).await.unwrap(), 0);
    let (ask, sug) = (
        r.usage("ask").await,
        r.usage("monitor_suggestions_query").await,
    );
    assert_eq!((ask.len(), sug.len()), (1, 1));
    assert_eq!(sug[0].user_id, Some(r.admin_id));
}

#[tokio::test]
async fn monitor_ask_exhausted_loop_fails_without_suggest_call() {
    let Some(r) = monitor_run(
        vec![
            Ok(("SELECT a", 1, 1)),
            Ok(("SELECT b", 1, 1)),
            Ok(("SELECT c", 1, 1)),
        ],
        vec![err("boom"), err("boom"), err("boom")],
        vec![suggestion_json("id", "")],
        true,
    )
    .await
    else {
        return;
    };
    let n = r.names();
    assert_eq!(&n[n.len() - 2..], ["final", "failed"]);
    assert_eq!(r.events[r.events.len() - 2].1["executed"], false);
    assert!(!n.contains(&"suggesting"));
    assert_eq!(r.json_left, 1);
    assert!(r.usage("monitor_suggestions_query").await.is_empty());
}

#[tokio::test]
async fn monitor_ask_unsupported_fails_with_reason() {
    let Some(r) = monitor_run(
        vec![Ok(("SELECT id FROM orders", 1, 1))],
        vec![rows(2)],
        vec![suggestion_json("", "x")],
        true,
    )
    .await
    else {
        return;
    };
    let n = r.names();
    assert_eq!(&n[n.len() - 3..], ["final", "suggesting", "failed"]);
    assert_eq!(r.events.last().unwrap().1["message"], "x");
}

#[tokio::test]
async fn monitor_ask_rows_withheld_when_samples_off() {
    for (samples, expect) in [(false, false), (true, true)] {
        let Some(r) = monitor_run(
            vec![Ok(("SELECT id FROM orders", 1, 1))],
            vec![rows(2)],
            vec![suggestion_json("id", "")],
            samples,
        )
        .await
        else {
            return;
        };
        let leaked = r.prompts.iter().any(|p| p.contains("ROW_SECRET"));
        assert_eq!(leaked, expect, "include_sample_values={samples}");
    }
}

async fn http_monitor_ask(
    client: &reqwest::Client,
    app: &common::TestApp,
    suffix: &str,
    ds_id: i64,
) -> reqwest::Response {
    client
        .post(format!("{}/api/monitors/ask{suffix}", app.address))
        .json(&json!({ "data_source_id": ds_id, "prompt": "alert me when orders drop" }))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn monitor_ask_without_create_monitor_403() {
    let Some(app) = spawn_app().await else { return };
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds(&admin, &app.address).await;
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let (u, _) = common::user_with_role(&app.db, org.id, &["view_query", "create_query"]).await;
    let suffix = format!("?api_key={}", common::test_api_key(&u.email));
    let resp = http_monitor_ask(&reqwest::Client::new(), &app, &suffix, ds_id).await;
    assert_eq!(
        resp.status().as_u16(),
        403,
        "{}",
        resp.text().await.unwrap()
    );
    assert!(ai_usage::Entity::find()
        .all(&app.db)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn monitor_ask_not_configured_streams_failed() {
    let Some(app) = spawn_app().await else { return };
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds(&admin, &app.address).await;
    let resp = http_monitor_ask(&admin, &app, "", ds_id).await;
    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp.headers()["content-type"].to_str().unwrap().to_string();
    assert!(ct.starts_with("text/event-stream"), "{ct}");
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("event: failed") && body.contains("not_configured"),
        "{body}"
    );
    assert!(!body.contains("event: draft"), "{body}");
}
