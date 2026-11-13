//! Agentic "Ask with AI" loop: generate SQL, gate it, preview it, and repair it from the execution
//! error, streaming each step as a named SSE event.
//!
//! Generic over the provider lookup and the preview executor so it is driven by fakes in tests.
//!
//! Invariants: only `is_read_only_sql` drafts ever reach the [`PreviewRunner`]; preview rows
//! never enter a prompt (only the error string); prompts, samples and rows are never logged.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use arcodash_core::entities::{organization, user};
use arcodash_core::models::{
    ai_profile_refresh_cooldown_key, ProfileData, AI_PROFILE_REFRESH_COOLDOWN_SECS,
};
use arcodash_core::queue::{self, enqueue_profile, JobStatus, Queue};
use redis::aio::ConnectionManager;

use crate::api::handlers::ai::context::{build_context, load_recent_queries, load_usable_profile};
use crate::api::handlers::ai::{
    is_read_only_sql, provider_for, schema_for_ai, AiProvider, AiProviderKind, SqlGeneration,
};
use crate::api::handlers::ai_query::{
    build_prompt, check_rate_limit, map_generation_error, not_configured, run_fallback_chain,
    GenerateSqlResponse, MAX_QUESTION_CHARS,
};
use crate::api::handlers::ai_settings::{
    effective_ai_config, fallback_enabled, include_sample_values, resolve_provider_chain,
    ResolvedProvider,
};
use crate::auth::{CurrentOrg, CurrentUser};
use crate::db::{Db, RequestScope};
use crate::error::ApiError;
use crate::extract::Json;
use crate::services::{data_source_service, job_service, query_service, result_service};
use crate::state::AppState;

/// Total generation attempts per request.
const MAX_ATTEMPTS: u32 = 3;
/// Cap on client- and database-supplied error text that reaches a prompt or an event.
const MAX_ERROR_CHARS: usize = 2000;
const NOT_READ_ONLY: &str = "the query must be a single SELECT (or WITH ... SELECT) statement";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewColumn {
    pub name: String,
    pub friendly_type: String,
}

pub enum PreviewOutcome {
    Ok {
        columns: Vec<PreviewColumn>,
        /// At most 5 rows.
        rows: Vec<Value>,
        row_count: u64,
        truncated: bool,
    },
    /// A SQL/execution error the model can repair.
    Err(String),
    /// Not a SQL problem (permission, paused source, no inline result, infra): finalize with the
    /// message, never spend a repair call on it.
    Blocked(String),
    TimedOut,
}

#[async_trait::async_trait]
pub trait PreviewRunner: Send + Sync {
    async fn run(&self, sql: &str) -> PreviewOutcome;
}

/// Lets the loop queue a profile refresh when a draft references objects the cached profile lacks.
pub struct ProfileRefresh {
    pub redis: ConnectionManager,
    pub queue: Arc<dyn Queue>,
    pub org_id: i64,
    pub data_source_id: i64,
}

const MISSING_OBJECT_MARKERS: &[&str] = &[
    "does not exist",
    "doesn't exist",
    "no such table",
    "no such column",
    "unknown column",
    "invalid object name",
    "invalid column name",
    "invalid identifier",
    "not found: table",
    "unrecognized name",
    "cannot be resolved",
    "unknown_table",
    "missing columns",
    "unknown identifier",
];

/// "does not exist" errors about objects a schema refresh cannot fix.
const NON_RELATION_MISSING: &[&str] = &[
    "operator does not exist",
    "function ",
    "procedure ",
    "type \"",
    "role \"",
    "database \"",
];

/// True when a database error says a table/column/relation is missing.
#[must_use]
pub fn is_missing_object_error(msg: &str) -> bool {
    let msg = msg.to_lowercase();
    if NON_RELATION_MISSING.iter().any(|m| msg.contains(m)) {
        return false;
    }
    MISSING_OBJECT_MARKERS.iter().any(|m| msg.contains(m))
}

/// Claims the per-source cooldown and queues the refresh; false when the cooldown is held or
/// queueing fails, so the caller falls back to a normal repair.
async fn start_profile_refresh(r: &ProfileRefresh) -> bool {
    let mut redis = r.redis.clone();
    let claimed: redis::RedisResult<Option<String>> = redis::cmd("SET")
        .arg(ai_profile_refresh_cooldown_key(r.data_source_id))
        .arg(1)
        .arg("NX")
        .arg("EX")
        .arg(AI_PROFILE_REFRESH_COOLDOWN_SECS)
        .query_async(&mut redis)
        .await;
    if !matches!(claimed, Ok(Some(_))) {
        return false;
    }
    let queued = enqueue_profile(
        &mut redis,
        r.queue.as_ref(),
        r.org_id,
        r.data_source_id,
        ProfileData::Changed,
    )
    .await
    .is_ok();
    if !queued {
        let _: redis::RedisResult<()> = redis::cmd("DEL")
            .arg(ai_profile_refresh_cooldown_key(r.data_source_id))
            .query_async(&mut redis)
            .await;
    }
    queued
}

pub struct AskCtx {
    pub scope: RequestScope,
    pub org: organization::Model,
    pub http: reqwest::Client,
    pub chain: Vec<ResolvedProvider>,
    pub fallback_enabled: bool,
    pub data_source_type: String,
    pub schema_text: String,
    pub question: String,
    pub existing_sql: Option<String>,
    /// Execution error from the editor ("Fix with AI"); makes the first prompt a repair prompt.
    pub error: Option<String>,
    pub user_id: Option<i64>,
    pub refresh: Option<ProfileRefresh>,
}

pub(crate) fn event(name: &str, payload: &Value) -> Event {
    Event::default().event(name).data(payload.to_string())
}

/// `None` once the client has gone away.
pub(crate) async fn emit(tx: &mpsc::Sender<Event>, name: &str, payload: Value) -> Option<()> {
    tx.send(event(name, &payload)).await.ok()
}

fn cap_error(s: &str) -> String {
    s.chars().take(MAX_ERROR_CHARS).collect()
}

async fn emit_final(
    tx: &mpsc::Sender<Event>,
    sql: &str,
    explanation: Option<&str>,
    served_by: &str,
    attempts: u32,
    tokens: (i64, i64),
    executed: bool,
    error: Option<&str>,
) -> Option<()> {
    emit(
        tx,
        "final",
        json!({
            "sql": sql,
            "explanation": explanation,
            "served_by": served_by,
            "attempts": attempts,
            "tokens_in": tokens.0,
            "tokens_out": tokens.1,
            "executed": executed,
            "error": error,
        }),
    )
    .await
}

fn repair_prompt(base: &str, previous_sql: Option<&str>, problem: &str) -> String {
    match previous_sql {
        Some(sql) if !sql.trim().is_empty() => {
            format!("{base}\n\nPrevious SQL:\n{sql}\nIt {problem}\nReturn a corrected query.")
        }
        _ => format!("{base}\n\nThe previous attempt {problem}\nReturn a corrected query."),
    }
}

/// What the loop ended with; `columns`/`row_count` come from the successful preview, else empty.
#[derive(Debug, Clone)]
pub struct AskOutcome {
    pub sql: String,
    pub executed: bool,
    pub error: Option<String>,
    pub columns: Vec<PreviewColumn>,
    pub rows: Vec<Value>,
    pub row_count: u64,
}

impl AskOutcome {
    const fn unexecuted(sql: String, error: Option<String>) -> Self {
        Self {
            sql,
            executed: false,
            error,
            columns: Vec::new(),
            rows: Vec::new(),
            row_count: 0,
        }
    }
}

pub async fn ask_loop<F, P>(ctx: AskCtx, provider_lookup: F, preview: P, tx: mpsc::Sender<Event>)
where
    F: Fn(AiProviderKind, Option<String>) -> Box<dyn AiProvider> + Send + Sync,
    P: PreviewRunner,
{
    let _ = ask_loop_outcome(ctx, provider_lookup, preview, &tx).await;
}

/// `None` when the client is gone or a `failed` event was already sent.
pub async fn ask_loop_outcome<F, P>(
    ctx: AskCtx,
    provider_lookup: F,
    preview: P,
    tx: &mpsc::Sender<Event>,
) -> Option<AskOutcome>
where
    F: Fn(AiProviderKind, Option<String>) -> Box<dyn AiProvider> + Send + Sync,
    P: PreviewRunner,
{
    emit(tx, "status", json!({ "text": "Generating SQL" })).await?;

    if let (Some(refresh), Some(err)) = (&ctx.refresh, &ctx.error) {
        if is_missing_object_error(err) && start_profile_refresh(refresh).await {
            emit(
                tx,
                "profile_refreshing",
                json!({ "data_source_id": refresh.data_source_id }),
            )
            .await?;
            let sql = ctx.existing_sql.as_deref().unwrap_or_default();
            emit_final(tx, sql, None, "", 0, (0, 0), false, Some(err)).await?;
            return Some(AskOutcome::unexecuted(sql.to_string(), Some(err.clone())));
        }
    }

    let repairing_initial = ctx.error.is_some();
    let base = build_prompt(
        &ctx.data_source_type,
        &ctx.schema_text,
        &ctx.question,
        if repairing_initial {
            None
        } else {
            ctx.existing_sql.as_deref()
        },
    );
    // (previous sql, problem) driving the next prompt; `None` = plain first prompt.
    let mut pending: Option<(Option<String>, String)> = ctx.error.as_ref().map(|e| {
        (
            ctx.existing_sql.clone(),
            format!("failed with: {}", cap_error(e)),
        )
    });

    let mut org = ctx.org.clone();
    let mut attempt = 0u32;
    let (mut tokens_in, mut tokens_out) = (0i64, 0i64);
    // Provider that generated `last_draft`; a failed later attempt never overwrites it.
    let mut served_by = String::new();
    let mut executed = false;
    let mut last_draft: Option<(String, Option<String>)> = None;
    let mut last_error: Option<String>;
    let mut last_preview: Option<(Vec<PreviewColumn>, Vec<Value>, u64)> = None;

    loop {
        attempt += 1;
        let prompt = match &pending {
            Some((prev, problem)) => repair_prompt(&base, prev.as_deref(), problem),
            None => base.clone(),
        };

        // Each provider call merges its status into `organizations.settings.ai`; start every
        // attempt from the persisted row so a stale snapshot can't overwrite an earlier one.
        if attempt > 1 {
            if let Ok(scoped) = ctx.scope.begin(&org).await {
                use sea_orm::EntityTrait;
                if let Ok(Some(fresh)) = organization::Entity::find_by_id(org.id)
                    .one(scoped.db())
                    .await
                {
                    org = fresh;
                }
                let _ = scoped.commit().await;
            }
        }

        let (kind, outcome) = run_fallback_chain(
            &ctx.scope,
            &org,
            ctx.user_id,
            &ctx.http,
            &ctx.chain,
            ctx.fallback_enabled,
            "ask",
            &prompt,
            &provider_lookup,
            |provider, http, api_key, model, prompt| {
                provider.generate_sql(http, api_key, model, prompt)
            },
            |gen: &SqlGeneration| (gen.tokens_in, gen.tokens_out),
        )
        .await;
        let gen = match outcome {
            Ok(gen) => gen,
            Err(err) => {
                let failed = serde_json::to_value(map_generation_error(err, kind)).ok()?;
                // A later attempt failing must not discard a usable earlier draft.
                if attempt > 1 {
                    if let Some((sql, expl)) = last_draft.take() {
                        let msg = failed.get("message").and_then(Value::as_str);
                        emit_final(
                            tx,
                            &sql,
                            expl.as_deref(),
                            &served_by,
                            attempt,
                            (tokens_in, tokens_out),
                            false,
                            msg,
                        )
                        .await?;
                        return Some(AskOutcome::unexecuted(sql, msg.map(str::to_string)));
                    }
                }
                emit(tx, "failed", failed).await?;
                return None;
            }
        };
        served_by = kind.as_str().to_string();
        tokens_in += gen.tokens_in.unwrap_or(0);
        tokens_out += gen.tokens_out.unwrap_or(0);
        let sql = gen.sql;
        let explanation = gen.explanation;
        last_draft = Some((sql.clone(), explanation.clone()));
        emit(
            tx,
            "draft",
            json!({ "attempt": attempt, "sql": sql, "explanation": explanation }),
        )
        .await?;

        let outcome = if is_read_only_sql(&sql) {
            emit(tx, "executing", json!({ "attempt": attempt })).await?;
            preview.run(&sql).await
        } else {
            PreviewOutcome::Err(NOT_READ_ONLY.to_string())
        };

        let problem = match outcome {
            PreviewOutcome::Ok {
                columns,
                rows,
                row_count,
                truncated,
            } => {
                let shown = rows.clone();
                emit(
                    tx,
                    "preview",
                    json!({
                        "attempt": attempt,
                        "columns": columns,
                        "rows": rows,
                        "row_count": row_count,
                        "truncated": truncated,
                    }),
                )
                .await?;
                executed = true;
                last_preview = Some((columns, shown, row_count));
                last_error = None;
                break;
            }
            PreviewOutcome::TimedOut => {
                emit(
                    tx,
                    "repair",
                    json!({ "attempt": attempt, "error": "preview timed out" }),
                )
                .await?;
                last_error = Some("preview timed out".to_string());
                break;
            }
            PreviewOutcome::Blocked(msg) => {
                last_error = Some(cap_error(&msg));
                break;
            }
            PreviewOutcome::Err(msg) => {
                let msg = cap_error(&msg);
                last_error = Some(msg.clone());
                if attempt >= MAX_ATTEMPTS {
                    break;
                }
                if let Some(refresh) = &ctx.refresh {
                    if is_missing_object_error(&msg) && start_profile_refresh(refresh).await {
                        emit(
                            tx,
                            "profile_refreshing",
                            json!({ "data_source_id": refresh.data_source_id }),
                        )
                        .await?;
                        break;
                    }
                }
                msg
            }
        };

        emit(
            tx,
            "repair",
            json!({ "attempt": attempt, "error": problem }),
        )
        .await?;
        pending = Some((Some(sql), format!("failed with: {problem}")));
    }

    let (sql, explanation) = last_draft?;
    emit_final(
        tx,
        &sql,
        explanation.as_deref(),
        &served_by,
        attempt,
        (tokens_in, tokens_out),
        executed,
        last_error.as_deref(),
    )
    .await?;
    let (columns, rows, row_count) = last_preview.unwrap_or_default();
    Some(AskOutcome {
        sql,
        executed,
        error: last_error,
        columns,
        rows,
        row_count,
    })
}

/// Rows requested from the preview run and rows echoed to the client.
const PREVIEW_MAX_ROWS: u32 = 20;
const PREVIEW_SHOWN_ROWS: usize = 5;
pub(crate) const PREVIEW_POLL: Duration = Duration::from_millis(500);
const PREVIEW_CAP: Duration = Duration::from_secs(25);

#[derive(Debug, Deserialize)]
pub struct AskRequest {
    pub data_source_id: i64,
    pub question: String,
    pub existing_sql: Option<String>,
    pub error: Option<String>,
}

pub(crate) fn sse(rx: mpsc::Receiver<Event>) -> impl IntoResponse {
    Sse::new(ReceiverStream::new(rx).map(Ok::<_, Infallible>))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// A stream holding a single `failed` event.
pub(crate) fn failed_stream(resp: GenerateSqlResponse) -> Response {
    let (tx, rx) = mpsc::channel(1);
    if let Ok(payload) = serde_json::to_value(resp) {
        let _ = tx.try_send(event("failed", &payload));
    }
    sse(rx).into_response()
}

/// Everything `ask` does before the provider is called, shared with other handlers that drive
/// the same loop.
pub(crate) enum AskStart {
    Ready {
        ctx: Box<AskCtx>,
        preview: Box<DbPreview>,
        tx: mpsc::Sender<Event>,
        rx: mpsc::Receiver<Event>,
    },
    /// A refusal already shaped as a `failed` stream.
    Early(Response),
}

impl AskStart {
    /// The SSE response: `Early` as-is, else `run` spawned to feed the stream.
    pub(crate) fn stream<Fut>(
        self,
        run: impl FnOnce(AskCtx, DbPreview, mpsc::Sender<Event>) -> Fut,
    ) -> Response
    where
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        match self {
            Self::Early(resp) => resp,
            Self::Ready {
                ctx,
                preview,
                tx,
                rx,
            } => {
                tokio::spawn(run(*ctx, *preview, tx));
                sse(rx).into_response()
            }
        }
    }
}

/// Consumes `db` (the request transaction is detached before the schema fetch), so callers run
/// their own DB checks first.
pub(crate) async fn prepare_ask(
    state: &AppState,
    db: Db,
    scope: &RequestScope,
    org: organization::Model,
    user: user::Model,
    payload: AskRequest,
) -> Result<AskStart, ApiError> {
    let ds =
        data_source_service::get_for_user(&db, user.org_id, &user, payload.data_source_id).await?;

    let groups = crate::authz::permissions::load_effective_groups(&db, &user).await?;
    query_service::require_data_source_execute_access(&db, &user, &groups, ds.id).await?;

    if !state.entitlements.ai_features(&org) {
        return Err(ApiError::forbidden(
            "AI SQL generation isn't enabled for this organization",
        ));
    }
    if payload.question.chars().count() > MAX_QUESTION_CHARS {
        return Err(ApiError::bad_request(format!(
            "question must not exceed {MAX_QUESTION_CHARS} characters"
        )));
    }

    let effective = effective_ai_config(&org, &state.config)?;
    let not_configured = || failed_stream(not_configured());
    if effective.provider.is_none() {
        return Ok(AskStart::Early(not_configured()));
    }

    if let Err(arcodash_core::ai::AiError::BudgetExceeded { cap, resets_at, .. }) =
        arcodash_core::ai::budget::check_budget(&db, org.id).await
    {
        return Err(ApiError::too_many_requests_with_code(
            arcodash_core::ai::budget_message(cap, resets_at),
            "ai_budget_exceeded",
        ));
    }

    // One charge per request, however many attempts the loop makes.
    if let Some(limited) = check_rate_limit(
        &mut state.redis.clone(),
        org.id,
        user.id,
        effective.rate_limit_per_min,
    )
    .await?
    {
        return Ok(AskStart::Early(failed_stream(
            GenerateSqlResponse::RateLimited {
                message: limited.message,
                retry_after_secs: limited.retry_after_secs,
            },
        )));
    }

    let profile = load_usable_profile(state, user.org_id, &ds).await?;
    let recent = load_recent_queries(&db, user.org_id, user.id, ds.id).await?;
    let include_samples = include_sample_values(&org.settings);
    scope.detach(db).await?;

    let tables = schema_for_ai(state, &ds, user.id).await?;
    let schema_text = build_context(
        &tables,
        profile.as_ref(),
        &payload.question,
        payload.existing_sql.as_deref(),
        &recent,
        include_samples,
    );

    let chain = resolve_provider_chain(&org, &state.config)?;
    if chain.is_empty() {
        return Ok(AskStart::Early(not_configured()));
    }

    crate::api::handlers::ai::describe::spawn_for_request(
        state,
        scope,
        &org,
        &ds,
        profile.as_ref(),
        &tables,
        &chain,
        include_samples,
        user.id,
    );

    let (tx, rx) = mpsc::channel(16);
    let preview = DbPreview {
        state: state.clone(),
        scope: scope.clone(),
        org: org.clone(),
        user: user.clone(),
        data_source_id: ds.id,
        tx: tx.clone(),
    };
    let ctx = AskCtx {
        scope: scope.clone(),
        fallback_enabled: fallback_enabled(&org.settings),
        org,
        http: state.http.clone(),
        chain,
        data_source_type: ds.r#type.clone(),
        schema_text,
        question: payload.question,
        existing_sql: payload.existing_sql,
        error: payload.error.as_deref().map(cap_error),
        user_id: Some(user.id),
        refresh: Some(ProfileRefresh {
            redis: state.redis.clone(),
            queue: state.queue.clone(),
            org_id: user.org_id,
            data_source_id: ds.id,
        }),
    };
    Ok(AskStart::Ready {
        ctx: Box::new(ctx),
        preview: Box::new(preview),
        tx,
        rx,
    })
}

/// `POST /api/queries/ask` — the agentic counterpart of `generate_sql`: streams
/// `status|draft|executing|preview|repair|final|failed` events.
///
/// Only `is_read_only_sql` drafts are ever executed, through the caller's own ad-hoc execution
/// path.
pub async fn ask(
    State(state): State<AppState>,
    db: Db,
    scope: RequestScope,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<AskRequest>,
) -> Result<Response, ApiError> {
    Ok(prepare_ask(&state, db, &scope, org, user, payload)
        .await?
        .stream(|ctx, preview, tx| ask_loop(ctx, provider_for, preview, tx)))
}

/// Runs the draft through the caller's own ad-hoc execution path (permission + org scoped) with a
/// 20-row cap and waits for the worker.
pub(crate) struct DbPreview {
    state: AppState,
    scope: RequestScope,
    org: organization::Model,
    user: user::Model,
    data_source_id: i64,
    tx: mpsc::Sender<Event>,
}

impl DbPreview {
    async fn cancel(&self, job_id: &str) {
        if let Ok(scoped) = self.scope.begin(&self.org).await {
            let _ = job_service::cancel(scoped.db(), &self.state.redis, job_id, &self.user).await;
            let _ = scoped.commit().await;
        }
    }

    async fn submit(&self, sql: &str) -> Result<query_service::ExecuteOutcome, ApiError> {
        let scoped = self.scope.begin(&self.org).await?;
        let outcome = query_service::execute(
            scoped.db(),
            self.state.queue.as_ref(),
            self.state.conveyor.as_deref(),
            self.state.result_reader(),
            &self.state.redis,
            self.user.org_id,
            &self.user,
            self.data_source_id,
            sql.to_string(),
            Value::Null,
            Value::Null,
            None,
            None,
            None,
            false,
            PREVIEW_MAX_ROWS,
        )
        .await?;
        scoped.commit().await?;
        Ok(outcome)
    }

    async fn fetch_result(&self, result_id: i64) -> PreviewOutcome {
        let scoped = match self.scope.begin(&self.org).await {
            Ok(s) => s,
            Err(e) => return PreviewOutcome::Err(e.message),
        };
        let res = result_service::get_with_data(
            scoped.db(),
            self.state.result_reader(),
            self.user.org_id,
            &self.user,
            result_id,
        )
        .await;
        let _ = scoped.commit().await;
        match res {
            Ok(r) => parse_preview(r.data.as_ref()),
            Err(e) => PreviewOutcome::Err(e.message),
        }
    }
}

/// `data` = `{columns:[{name,friendly_type}], rows:[...], truncated}` as the worker persists it.
/// A result with no inline data (stored externally) gives no preview rather than a repair.
fn parse_preview(data: Option<&Value>) -> PreviewOutcome {
    let Some(data) = data else {
        return PreviewOutcome::Blocked("the result is too large to preview".into());
    };
    let columns = data
        .get("columns")
        .cloned()
        .and_then(|c| serde_json::from_value(c).ok())
        .unwrap_or_default();
    let all = data.get("rows").and_then(Value::as_array);
    PreviewOutcome::Ok {
        columns,
        row_count: all.map_or(0, |r| r.len() as u64),
        rows: all.map_or_else(Vec::new, |r| {
            r.iter().take(PREVIEW_SHOWN_ROWS).cloned().collect()
        }),
        truncated: data
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// Access, missing-source and paused-source refusals are not SQL defects.
fn classify_submit_error(e: ApiError) -> PreviewOutcome {
    use axum::http::StatusCode;
    match e.status {
        StatusCode::UNAUTHORIZED
        | StatusCode::FORBIDDEN
        | StatusCode::NOT_FOUND
        | StatusCode::CONFLICT => PreviewOutcome::Blocked(e.message),
        _ => PreviewOutcome::Err(e.message),
    }
}

#[async_trait::async_trait]
impl PreviewRunner for DbPreview {
    async fn run(&self, sql: &str) -> PreviewOutcome {
        let job_id = match self.submit(sql).await {
            Ok(query_service::ExecuteOutcome::Cached(r)) => return parse_preview(r.data.as_ref()),
            Ok(
                query_service::ExecuteOutcome::Enqueued { job_id }
                | query_service::ExecuteOutcome::Parked { job_id },
            ) => job_id,
            Err(e) => return classify_submit_error(e),
        };

        let started = Instant::now();
        loop {
            // Client gone: nobody will read the preview, so free the worker slot.
            if self.tx.is_closed() || started.elapsed() >= PREVIEW_CAP {
                self.cancel(&job_id).await;
                return PreviewOutcome::TimedOut;
            }
            match queue::get_job_status(&self.state.redis, &job_id).await {
                Ok(Some(rec)) => match rec.status {
                    JobStatus::Success => {
                        return match rec.query_result_id {
                            Some(id) => self.fetch_result(id).await,
                            None => PreviewOutcome::Err("query produced no result".into()),
                        }
                    }
                    JobStatus::Failure => {
                        return PreviewOutcome::Err(
                            rec.error.unwrap_or_else(|| "query failed".into()),
                        )
                    }
                    JobStatus::Cancelled => {
                        return PreviewOutcome::Err("query was cancelled".into())
                    }
                    JobStatus::Pending | JobStatus::Started => {}
                },
                Ok(None) => return PreviewOutcome::Err("preview job expired".into()),
                Err(_) => {
                    self.cancel(&job_id).await;
                    return PreviewOutcome::Blocked("couldn't read the preview status".into());
                }
            }
            tokio::time::sleep(PREVIEW_POLL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_object_classifier() {
        for yes in [
            r#"relation "orders" does not exist"#,
            "Table 'shop.orders' doesn't exist",
            "Unknown column 'x' in 'field list'",
            "Invalid object name 'orders'.",
            "Invalid column name 'x'.",
            "no such table: orders",
            "SQL compilation error: Object 'ORDERS' does not exist or not authorized.",
            "SQL compilation error: invalid identifier 'X'",
            "Not found: Table proj:ds.orders was not found in location US",
            "Unrecognized name: foo at [1:8]",
            "line 1:15: Table 'awsdatacatalog.db.orders' does not exist",
            "COLUMN_NOT_FOUND: Column 'x' cannot be resolved",
            "Code: 60. DB::Exception: UNKNOWN_TABLE (Table db.orders doesn't exist)",
            "Missing columns: 'x' while processing query",
            "Unknown identifier: x",
        ] {
            assert!(is_missing_object_error(yes), "{yes}");
        }
        for no in [
            r#"syntax error at or near "FORM""#,
            "permission denied for table orders",
            "canceling statement due to statement timeout",
            "division by zero",
            r#"column reference "id" is ambiguous"#,
            "Ambiguous column name 'id'.",
            "operator does not exist: integer = text",
            "function date_trunc(unknown, integer) does not exist",
            r#"type "x" does not exist"#,
            r#"role "x" does not exist"#,
            r#"database "x" does not exist"#,
            "FUNCTION db.foo does not exist",
        ] {
            assert!(!is_missing_object_error(no), "{no}");
        }
    }

    #[test]
    fn no_inline_data_is_blocked_not_repairable() {
        assert!(matches!(parse_preview(None), PreviewOutcome::Blocked(_)));
    }

    #[test]
    fn cap_error_truncates_on_char_boundary() {
        let s = "é".repeat(MAX_ERROR_CHARS + 5);
        assert_eq!(cap_error(&s).chars().count(), MAX_ERROR_CHARS);
    }

    #[test]
    fn refusals_are_blocked_and_bad_sql_is_not() {
        assert!(matches!(
            classify_submit_error(ApiError::forbidden("no")),
            PreviewOutcome::Blocked(_)
        ));
        assert!(matches!(
            classify_submit_error(ApiError::conflict("paused")),
            PreviewOutcome::Blocked(_)
        ));
        assert!(matches!(
            classify_submit_error(ApiError::bad_request("x")),
            PreviewOutcome::Err(_)
        ));
    }
}
