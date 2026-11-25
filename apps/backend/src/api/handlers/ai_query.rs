//! `POST /api/queries/generate_sql`: "Ask with AI" text-to-SQL. Any authenticated user with access
//! to the data source may call it (only configuring the key is admin-only).

use std::fmt::Write as _;

use crate::extract::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use arcodash_core::entities::organization;

use crate::api::handlers::ai::context::{build_context, load_recent_queries, load_usable_profile};
use crate::api::handlers::ai::{
    provider_for, schema_for_ai, AiError, AiProvider, AiProviderKind, SqlGeneration,
    AI_BUSY_MESSAGE,
};
use crate::api::handlers::ai_settings::{
    effective_ai_config, fallback_enabled, include_sample_values, record_provider_status,
    resolve_provider_chain, ResolvedProvider,
};
use crate::auth::{CurrentOrg, CurrentUser};
use crate::db::{Db, RequestScope};
use crate::error::ApiError;
use crate::services::data_source_service;
use crate::state::AppState;
use arcodash_core::ai::fallback::{
    run_fallback_chain as core_run_fallback_chain, Attempt, AttemptSink, GenFuture,
};
use arcodash_core::ai::usage::record_usage;

/// Upper bound on the question length, applied before it reaches the LLM prompt.
pub(crate) const MAX_QUESTION_CHARS: usize = 4000;

#[derive(Debug, Deserialize)]
pub struct GenerateSqlRequest {
    pub data_source_id: i64,
    pub question: String,
    #[serde(default)]
    pub existing_sql: Option<String>,
}

/// Tagged so the frontend can tell "here's your SQL" from each recoverable failure without parsing
/// HTTP status; always 200, with `status` carrying the outcome.
#[derive(Debug, Serialize)]
#[serde(tag = "status")]
pub enum GenerateSqlResponse {
    #[serde(rename = "ok")]
    Ok {
        sql: String,
        explanation: Option<String>,
        /// Which provider actually served the request (`as_str()` of its [`AiProviderKind`]);
        /// always populated, additive.
        served_by: String,
    },
    #[serde(rename = "not_configured")]
    NotConfigured { message: String },
    #[serde(rename = "invalid_key")]
    InvalidKey { message: String },
    #[serde(rename = "quota_exceeded")]
    QuotaExceeded { message: String },
    #[serde(rename = "rate_limited")]
    RateLimited {
        message: String,
        retry_after_secs: u64,
    },
    #[serde(rename = "generation_failed")]
    GenerationFailed { message: String },
}

pub(crate) fn not_configured() -> GenerateSqlResponse {
    GenerateSqlResponse::NotConfigured {
        message: "AI SQL generation isn't configured. An admin can add a provider API key in AI \
                  Settings."
            .to_string(),
    }
}

/// `POST /api/queries/generate_sql` — turn a natural-language `question` into SQL text via the
/// org's active AI provider.
///
/// **Invariant: this handler only returns generated SQL as text; it never executes the query.**
pub async fn generate_sql(
    State(state): State<AppState>,
    db: Db,
    scope: RequestScope,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<GenerateSqlRequest>,
) -> Result<Json<GenerateSqlResponse>, ApiError> {
    // 1. Access-check the data source FIRST so an unauthorized caller never burns a rate-limit slot or API call; 404 on no-access like other data-source reads.
    let ds =
        data_source_service::get_for_user(&db, user.org_id, &user, payload.data_source_id).await?;

    // 2. Entitlement gate — defence-in-depth alongside `ai_viz`/`ai_monitors`.
    if !state.entitlements.ai_features(&org) {
        return Err(ApiError::forbidden(
            "AI SQL generation isn't enabled for this organization",
        ));
    }

    // 3. Cheap request-shape bound, before any I/O.
    if payload.question.chars().count() > MAX_QUESTION_CHARS {
        return Err(ApiError::bad_request(format!(
            "question must not exceed {MAX_QUESTION_CHARS} characters"
        )));
    }

    let effective = effective_ai_config(&org, &state.config)?;

    // 4. Nothing configured: bail before spending a rate-limit slot or touching the data source.
    if effective.provider.is_none() {
        return Ok(Json(not_configured()));
    }

    // 5. Rate limit (per user, per minute), charged once regardless of how many providers the fallback chain attempts.
    if let Some(rate_limited) = check_rate_limit(
        &mut state.redis.clone(),
        org.id,
        user.id,
        effective.rate_limit_per_min,
    )
    .await?
    {
        return Ok(Json(GenerateSqlResponse::RateLimited {
            message: rate_limited.message,
            retry_after_secs: rate_limited.retry_after_secs,
        }));
    }

    // 6. Schema: cached when available. Release the transaction first, since the schema fallback and LLM chain are slow.
    let profile = load_usable_profile(&state, user.org_id, &ds).await?;
    let recent = load_recent_queries(&db, user.org_id, user.id, ds.id).await?;
    let include_samples = include_sample_values(&org.settings);
    scope.detach(db).await?;

    let tables = schema_for_ai(&state, &ds, user.id).await?;
    let schema_text = build_context(
        &tables,
        profile.as_ref(),
        &payload.question,
        payload.existing_sql.as_deref(),
        &recent,
        include_samples,
    );

    let prompt = build_prompt(
        &ds.r#type,
        &schema_text,
        &payload.question,
        payload.existing_sql.as_deref(),
    );

    // 7. Dispatch with structured output (guaranteed JSON). `run_fallback_chain` tries only the first provider unless fallback is on and the failure means that provider itself won't work (see `should_attempt_fallback`).
    let chain = resolve_provider_chain(&org, &state.config)?;
    if chain.is_empty() {
        return Ok(Json(not_configured()));
    }
    crate::api::handlers::ai::describe::spawn_for_request(
        &state,
        &scope,
        &org,
        &ds,
        profile.as_ref(),
        &tables,
        &chain,
        include_samples,
        user.id,
    );
    let (served_kind, outcome) = run_fallback_chain(
        &scope,
        &org,
        Some(user.id),
        &state.http,
        &chain,
        fallback_enabled(&org.settings),
        "sql_generation",
        &prompt,
        provider_for,
        |provider, http, api_key, model, prompt| {
            provider.generate_sql(http, api_key, model, prompt)
        },
        |gen: &SqlGeneration| (gen.tokens_in, gen.tokens_out),
    )
    .await;

    Ok(Json(match outcome {
        Ok(gen) => GenerateSqlResponse::Ok {
            sql: gen.sql,
            explanation: gen.explanation,
            served_by: served_kind.as_str().to_string(),
        },
        Err(AiError::BudgetExceeded { cap, resets_at, .. }) => {
            return Err(ApiError::too_many_requests_with_code(
                arcodash_core::ai::budget_message(cap, resets_at),
                "ai_budget_exceeded",
            ))
        }
        Err(err) => map_generation_error(err, served_kind),
    }))
}

/// Runs the provider fallback chain ([`arcodash_core::ai::fallback::run_fallback_chain`]).
///
/// Records each attempt's key-health status and usage on its own short [`RequestScope::begin`]
/// transaction so no pooled connection is pinned during a provider call.
///
/// `pub` for integration tests.
#[allow(clippy::too_many_arguments)]
pub async fn run_fallback_chain<T, F, C, TokFn>(
    scope: &RequestScope,
    org: &organization::Model,
    user_id: Option<i64>,
    http: &reqwest::Client,
    chain: &[ResolvedProvider],
    retry_enabled: bool,
    feature: &str,
    prompt: &str,
    provider_lookup: F,
    call: C,
    token_accessor: TokFn,
) -> (AiProviderKind, Result<T, AiError>)
where
    F: Fn(AiProviderKind, Option<String>) -> Box<dyn AiProvider>,
    C: for<'a> Fn(
        &'a dyn AiProvider,
        &'a reqwest::Client,
        &'a str,
        &'a str,
        &'a str,
    ) -> GenFuture<'a, T>,
    TokFn: Fn(&T) -> (Option<i64>, Option<i64>),
{
    let sink = ScopeSink::new(scope, org, user_id);
    core_run_fallback_chain(
        http,
        chain,
        retry_enabled,
        feature,
        prompt,
        provider_lookup,
        call,
        token_accessor,
        &sink,
    )
    .await
}

/// Each attempt's `record_provider_status` returns the updated row, threaded through the mutex so
/// later attempts see the latest settings.
pub(crate) struct ScopeSink<'a> {
    scope: &'a RequestScope,
    org: tokio::sync::Mutex<organization::Model>,
    org_id: i64,
    user_id: Option<i64>,
}

impl<'a> ScopeSink<'a> {
    pub(crate) fn new(
        scope: &'a RequestScope,
        org: &organization::Model,
        user_id: Option<i64>,
    ) -> Self {
        Self {
            scope,
            org: tokio::sync::Mutex::new(org.clone()),
            org_id: org.id,
            user_id,
        }
    }
}

#[async_trait::async_trait]
impl AttemptSink for ScopeSink<'_> {
    async fn check_budget(&self) -> Result<(), AiError> {
        let org = self.org.lock().await.clone();
        // Short scope, released before any provider call; fails open like the check itself.
        let Ok(scoped) = self.scope.begin(&org).await else {
            tracing::warn!(
                org_id = self.org_id,
                "budget check could not open scope; allowing"
            );
            return Ok(());
        };
        let res = arcodash_core::ai::budget::check_budget(scoped.db(), self.org_id).await;
        let _ = scoped.commit().await;
        res.map(|_| ())
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "`current_org` is borrowed by `scope.begin` and `record_provider_status` and updated in place"
    )]
    async fn record(&self, attempt: Attempt<'_>) {
        crate::metrics::record_ai_provider_call(
            attempt.kind.as_str(),
            attempt.feature,
            attempt.status,
        );
        let mut current_org = self.org.lock().await;
        // Metering must never fail a generation: both writes are best-effort.
        if let Ok(scoped) = self.scope.begin(&current_org).await {
            if let Ok(updated) = record_provider_status(
                scoped.db(),
                &current_org,
                attempt.kind,
                attempt.status,
                attempt.free_tier,
            )
            .await
            {
                *current_org = updated;
            }
            let _ = record_usage(
                scoped.db(),
                self.org_id,
                self.user_id,
                attempt.kind,
                attempt.model,
                attempt.feature,
                attempt.status,
                attempt.tokens_in,
                attempt.tokens_out,
            )
            .await;
            let _ = scoped.commit().await;
        } else {
            tracing::warn!(
                org_id = self.org_id,
                provider = attempt.kind.as_str(),
                "failed to open scoped transaction for AI provider status/usage metering; both writes dropped"
            );
        }
    }
}

pub(crate) const fn provider_name(kind: AiProviderKind) -> &'static str {
    match kind {
        AiProviderKind::Gemini => "Gemini",
        AiProviderKind::OpenAi => "OpenAI",
        AiProviderKind::Anthropic => "Anthropic",
    }
}

pub(crate) fn rate_limit_message(kind: AiProviderKind) -> String {
    format!(
        "{} is rate-limiting the configured key right now.",
        provider_name(kind)
    )
}

/// The failure half of every AI endpoint's tagged response (same variant names everywhere, so the
/// frontend's shared error helper works unchanged).
pub(crate) enum AiFailure {
    InvalidKey(String),
    QuotaExceeded(String),
    RateLimited {
        message: String,
        retry_after_secs: u64,
    },
    GenerationFailed(String),
}

/// Maps a provider-agnostic [`AiError`] to its user-facing failure with per-provider wording;
/// `failed` is the endpoint's own copy for an unusable generation.
#[allow(
    clippy::needless_pass_by_value,
    reason = "callers hand off the error they just received; borrowing only moves the lint onto every mapping wrapper"
)]
pub(crate) fn ai_failure(err: AiError, kind: AiProviderKind, failed: &str) -> AiFailure {
    let provider = provider_name(kind);
    match err {
        AiError::BudgetExceeded { cap, resets_at, .. } => {
            AiFailure::GenerationFailed(arcodash_core::ai::budget_message(cap, resets_at))
        }
        AiError::Unavailable(_) | AiError::Network(_) => {
            AiFailure::GenerationFailed(AI_BUSY_MESSAGE.to_string())
        }
        AiError::RateLimited {
            retry_after_secs, ..
        } => AiFailure::RateLimited {
            message: rate_limit_message(kind),
            retry_after_secs,
        },
        AiError::InvalidKey => AiFailure::InvalidKey(format!(
            "The configured {provider} key was rejected. An admin should check AI Settings."
        )),
        AiError::QuotaExceeded { free_tier } => AiFailure::QuotaExceeded(match kind {
            AiProviderKind::Gemini => gemini_quota_message(free_tier),
            _ => format!(
                "{provider} quota exhausted for the configured key. Try again later or ask an \
                 admin to check the plan."
            ),
        }),
        AiError::GenerationFailed(_) => AiFailure::GenerationFailed(failed.to_string()),
    }
}

impl From<AiFailure> for GenerateSqlResponse {
    fn from(f: AiFailure) -> Self {
        match f {
            AiFailure::InvalidKey(message) => Self::InvalidKey { message },
            AiFailure::QuotaExceeded(message) => Self::QuotaExceeded { message },
            AiFailure::RateLimited {
                message,
                retry_after_secs,
            } => Self::RateLimited {
                message,
                retry_after_secs,
            },
            AiFailure::GenerationFailed(message) => Self::GenerationFailed { message },
        }
    }
}

/// Maps a provider-agnostic [`AiError`] to the tagged response the frontend expects, with
/// per-provider wording.
pub(crate) fn map_generation_error(err: AiError, kind: AiProviderKind) -> GenerateSqlResponse {
    ai_failure(
        err,
        kind,
        "Couldn't generate SQL — try rephrasing your question.",
    )
    .into()
}

/// Human dialect name for the prompt. MongoDB isn't SQL: a known v1 limitation, the model just does
/// its best. `pub(crate)` for reuse by `build_suggest_prompt`.
pub(crate) use arcodash_core::ai::dialect_name;

#[must_use]
pub fn build_prompt(
    type_name: &str,
    schema_text: &str,
    question: &str,
    existing_sql: Option<&str>,
) -> String {
    let dialect = dialect_name(type_name);
    let mut prompt =
        format!("You are a SQL assistant generating queries for a {dialect} data source.\n");
    if type_name == "mongodb" {
        prompt.push_str(
            "Note: MongoDB is not a SQL database — express the query as MongoDB's query/aggregation language as best you can.\n",
        );
    }
    prompt.push_str("\nSchema (one table per line: table(column type, ...); parentheses after a column hold sampled-data notes):\n");
    prompt.push_str(schema_text);
    let _ = write!(
        prompt,
        "\n\nRules:\n\
        - Use only tables and columns listed above. Never invent names.\n\
        - Write valid {dialect} SQL. Quote identifiers only when the dialect requires it.\n\
        - Read-only: a single SELECT (or WITH ... SELECT). No DML/DDL, no multiple statements.\n\
        - Prefer explicit JOINs with ON conditions over comma joins.\n\
        - If the question cannot be answered from this schema, return your best attempt and say \
        what is missing in the explanation.\n"
    );
    if let Some(existing) = existing_sql {
        let _ = write!(
            prompt,
            "\nThe user has an existing query. Modify/refine it to satisfy the question below rather than writing a new one from scratch:\n{existing}\n"
        );
    }
    let _ = write!(
        prompt,
        "\nQuestion: {question}\n\nRespond with the query and a short explanation of what it does."
    );
    prompt
}

/// Neutral outcome of a rate-limit check, shared by `generate_sql` and "Auto Visualize" via
/// [`check_rate_limit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitExceeded {
    pub message: String,
    pub retry_after_secs: u64,
}

/// Redis key for the fixed-window AI-request counter: `INCR` creates it and `EXPIRE ... NX` gives
/// it a 60s lifetime, so it self-resets. Shared across replicas via `state.redis`.
fn ai_rate_limit_key(org_id: i64, user_id: i64) -> String {
    format!("arcodash:ai_rate_limit:{org_id}:{user_id}")
}

const AI_RATE_LIMIT_WINDOW_SECS: u64 = 60;

/// Returns `Some(RateLimitExceeded)` if this call is over `limit` for the current 60s window. Takes
/// the bare connection so tests can use a real Redis without an `AppState`; `pub` for integration
/// tests.
pub async fn check_rate_limit(
    redis: &mut redis::aio::ConnectionManager,
    org_id: i64,
    user_id: i64,
    limit: u32,
) -> Result<Option<RateLimitExceeded>, ApiError> {
    use redis::AsyncCommands;

    let key = ai_rate_limit_key(org_id, user_id);
    let count: u64 = redis
        .incr(&key, 1)
        .await
        .map_err(|e| ApiError::internal(format!("redis error incrementing AI rate limit: {e}")))?;

    // `EXPIRE ... NX` only sets a TTL if the key has none, so it runs after every INCR rather than
    // only when `count == 1`: if the process died or the future was dropped between INCR and
    // EXPIRE, a key without a TTL would rate limit that (org, user) forever. Unconditional `NX` is
    // idempotent and self-healing.
    let _: () = redis::cmd("EXPIRE")
        .arg(&key)
        .arg(AI_RATE_LIMIT_WINDOW_SECS)
        .arg("NX")
        .query_async(redis)
        .await
        .map_err(|e| {
            ApiError::internal(format!("redis error setting AI rate limit expiry: {e}"))
        })?;

    if count > u64::from(limit) {
        let ttl: i64 = redis.ttl(&key).await.unwrap_or(-1);
        let retry_after_secs = if ttl > 0 {
            ttl.cast_unsigned()
        } else {
            AI_RATE_LIMIT_WINDOW_SECS
        };
        Ok(Some(RateLimitExceeded {
            message: format!(
                "AI request limit reached ({limit}/min). Wait a moment and try again."
            ),
            retry_after_secs,
        }))
    } else {
        Ok(None)
    }
}

/// Gemini quota copy; a free-tier key gets the actionable "enable billing" variant.
#[must_use]
pub fn gemini_quota_message(free_tier: bool) -> String {
    if free_tier {
        "Gemini's free-tier daily limit is used up for this key. Try again tomorrow, or ask an admin to enable billing on the key's Google Cloud project.".to_string()
    } else {
        "Gemini quota exhausted for the configured key. Try again later or ask an admin to check the plan.".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_prompt_has_rules_dialect_and_typed_columns() {
        let p = build_prompt("pg", "orders(id integer, created_at timestamp)", "q?", None);
        assert!(p.contains("PostgreSQL"));
        assert!(p.contains("created_at timestamp"));
        assert!(p.contains("Rules:"));
        assert!(p.contains("Write valid PostgreSQL SQL."));
        assert!(p.contains("Never invent names."));
        assert!(!p.contains("column types are not available"));
    }

    /// Pins the `question` length bound's exact boundary — at the limit passes, one over fails.
    #[test]
    fn question_length_bound_is_exact() {
        let at_limit = "a".repeat(MAX_QUESTION_CHARS);
        let over_limit = "a".repeat(MAX_QUESTION_CHARS + 1);
        assert!(at_limit.chars().count() <= MAX_QUESTION_CHARS);
        assert!(over_limit.chars().count() > MAX_QUESTION_CHARS);
    }

    /// Locks the `AiError` → `GenerateSqlResponse` wire contract: tag and message text per variant.
    #[test]
    fn map_generation_error_wire_contract_gemini() {
        match map_generation_error(AiError::InvalidKey, AiProviderKind::Gemini) {
            GenerateSqlResponse::InvalidKey { message } => assert_eq!(
                message,
                "The configured Gemini key was rejected. An admin should check AI Settings."
            ),
            other => panic!("expected InvalidKey, got {other:?}"),
        }

        match map_generation_error(AiError::QuotaExceeded { free_tier: false }, AiProviderKind::Gemini) {
            GenerateSqlResponse::QuotaExceeded { message } => assert_eq!(
                message,
                "Gemini quota exhausted for the configured key. Try again later or ask an admin to check the plan."
            ),
            other => panic!("expected QuotaExceeded, got {other:?}"),
        }

        match map_generation_error(
            AiError::Network("connection reset".to_string()),
            AiProviderKind::Gemini,
        ) {
            GenerateSqlResponse::GenerationFailed { message } => {
                assert_eq!(message, AI_BUSY_MESSAGE)
            }
            other => panic!("expected GenerationFailed, got {other:?}"),
        }

        match map_generation_error(
            AiError::Unavailable("503".to_string()),
            AiProviderKind::OpenAi,
        ) {
            GenerateSqlResponse::GenerationFailed { message } => {
                assert_eq!(message, AI_BUSY_MESSAGE)
            }
            other => panic!("expected GenerationFailed, got {other:?}"),
        }

        match map_generation_error(
            AiError::RateLimited {
                retry_after_secs: 30,
                free_tier: false,
            },
            AiProviderKind::Gemini,
        ) {
            GenerateSqlResponse::RateLimited {
                message,
                retry_after_secs,
            } => {
                assert_eq!(
                    message,
                    "Gemini is rate-limiting the configured key right now."
                );
                assert_eq!(retry_after_secs, 30);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }

        match map_generation_error(
            AiError::GenerationFailed("bad json".to_string()),
            AiProviderKind::Gemini,
        ) {
            GenerateSqlResponse::GenerationFailed { message } => assert_eq!(
                message,
                "Couldn't generate SQL — try rephrasing your question."
            ),
            other => panic!("expected GenerationFailed, got {other:?}"),
        }
    }

    #[test]
    fn map_generation_error_wire_contract_openai() {
        match map_generation_error(AiError::InvalidKey, AiProviderKind::OpenAi) {
            GenerateSqlResponse::InvalidKey { message } => assert_eq!(
                message,
                "The configured OpenAI key was rejected. An admin should check AI Settings."
            ),
            other => panic!("expected InvalidKey, got {other:?}"),
        }

        match map_generation_error(AiError::QuotaExceeded { free_tier: false }, AiProviderKind::OpenAi) {
            GenerateSqlResponse::QuotaExceeded { message } => assert_eq!(
                message,
                "OpenAI quota exhausted for the configured key. Try again later or ask an admin to check the plan."
            ),
            other => panic!("expected QuotaExceeded, got {other:?}"),
        }
    }

    #[test]
    fn map_generation_error_wire_contract_anthropic() {
        match map_generation_error(AiError::InvalidKey, AiProviderKind::Anthropic) {
            GenerateSqlResponse::InvalidKey { message } => assert_eq!(
                message,
                "The configured Anthropic key was rejected. An admin should check AI Settings."
            ),
            other => panic!("expected InvalidKey, got {other:?}"),
        }

        match map_generation_error(AiError::QuotaExceeded { free_tier: false }, AiProviderKind::Anthropic) {
            GenerateSqlResponse::QuotaExceeded { message } => assert_eq!(
                message,
                "Anthropic quota exhausted for the configured key. Try again later or ask an admin to check the plan."
            ),
            other => panic!("expected QuotaExceeded, got {other:?}"),
        }

        match map_generation_error(
            AiError::GenerationFailed("bad json".to_string()),
            AiProviderKind::Anthropic,
        ) {
            GenerateSqlResponse::GenerationFailed { message } => assert_eq!(
                message,
                "Couldn't generate SQL — try rephrasing your question."
            ),
            other => panic!("expected GenerationFailed, got {other:?}"),
        }
    }

    #[test]
    fn gemini_free_tier_quota_gets_billing_copy() {
        match map_generation_error(
            AiError::QuotaExceeded { free_tier: true },
            AiProviderKind::Gemini,
        ) {
            GenerateSqlResponse::QuotaExceeded { message } => assert_eq!(
                message,
                "Gemini's free-tier daily limit is used up for this key. Try again tomorrow, or ask an admin to enable billing on the key's Google Cloud project."
            ),
            other => panic!("expected QuotaExceeded, got {other:?}"),
        }
    }
}
