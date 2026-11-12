//! `POST /api/data_sources/{id}/suggest_monitors` — "AI data-quality monitor suggestions".
//!
//! Modelled line-for-line on `ai_query.rs`'s `generate_sql`: same access-check-before-rate-limit-
//! before-provider-call ordering, same tagged-response shape, same fallback chain. This handler
//! is read-only and adds no write endpoint — accepting a suggestion is four *existing*
//! calls (`POST /api/queries`, `.../publish`, `POST /api/monitors`, `.../subscriptions`) chained
//! client-side.

use crate::extract::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use crate::api::handlers::ai::context::{build_context, load_recent_queries, usable_profile};
use crate::api::handlers::ai::monitor_suggestions::{
    build_response, build_suggest_prompt_from_text, monitor_facts, CoveredTable,
    ExistingMonitorFacts, MonitorSuggestion,
};
use crate::api::handlers::ai::monitor_suggestions::{
    suggest_for_query, CoveredColumn, ExistingQueryMonitor, QueryMonitorSuggestion,
    QuerySuggestInput,
};
use crate::api::handlers::ai::viz::ColumnMeta;
use crate::api::handlers::ai::{
    cached_schema, dialect_name, live_schema_for_ai, provider_for, schema_for_ai, AiError,
    AiProviderKind,
};
use crate::api::handlers::ai_query::{
    ai_failure, check_rate_limit, run_fallback_chain, AiFailure, ScopeSink, MAX_QUESTION_CHARS,
};
use crate::api::handlers::ai_settings::{
    effective_ai_config, fallback_enabled, include_sample_values, resolve_provider_chain,
};
use crate::auth::{CurrentOrg, CurrentUser};
use crate::authz::authorize::{self, Action};
use crate::db::{Db, RequestScope};
use crate::error::ApiError;
use crate::services::{data_source_service, monitor_service, query_service, result_service};
use crate::state::AppState;

const MAX_TABLES_PER_RUN: usize = 20;

#[derive(Debug, Deserialize)]
pub struct SuggestMonitorsRequest {
    pub tables: Vec<String>,
}

/// Tagged response, shaped like `GenerateSqlResponse`.
///
/// Same reason as there — the frontend's shared `aiErrorMessage` helper needs an identical shape
/// (same variant names, same `message`/`retry_after_secs` fields) to work unchanged across every
/// AI-backed endpoint.
///
/// `Ok` flattens its payload struct next to the `status` tag.
#[derive(Debug, Serialize)]
#[serde(tag = "status")]
#[allow(clippy::large_enum_variant)]
pub enum AiJsonResponse<T> {
    #[serde(rename = "ok")]
    Ok(T),
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

#[derive(Debug, Serialize)]
pub struct DataSourceSuggestions {
    suggestions: Vec<MonitorSuggestion>,
    covered: Vec<CoveredTable>,
    scanned_tables: usize,
    served_by: String,
}

pub type SuggestMonitorsResponse = AiJsonResponse<DataSourceSuggestions>;

const NOT_CONFIGURED_MESSAGE: &str =
    "AI isn't set up for your organization yet. An admin can add a provider API key in AI Settings.";

/// `POST /api/data_sources/{id}/suggest_monitors` — body `{ "tables": [...] }`, 1..=20 entries,
/// every one an existing table on this data source.
pub async fn suggest_monitors(
    State(state): State<AppState>,
    db: Db,
    scope: RequestScope,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<SuggestMonitorsRequest>,
) -> Result<Json<SuggestMonitorsResponse>, ApiError> {
    // 1. Access-check the data source first — fail fast, same rationale as `generate_sql`: an
    // unauthorized caller should never burn a rate-limit slot or reach a provider call. 404s
    // rather than 403s on no access, same convention as every other data-source read.
    let ds = data_source_service::get_for_user(&db, user.org_id, &user, id).await?;

    // 2. The card only exists for a user holding both `create_query` and `create_monitor` — the backend is the authoritative check, and a suggestion the caller could
    // never accept should not be generated. Denial maps to the same 404 the access check above
    // uses (rather than the 403 `authorize` would otherwise return for a create action), so a
    // caller without these permissions learns nothing about the endpoint's existence beyond what
    // the access check already reveals.
    authorize::authorize(&db, &user, Action::QueryCreate)
        .await
        .map_err(|_| ApiError::not_found("data source not found"))?;
    authorize::authorize(&db, &user, Action::MonitorCreate)
        .await
        .map_err(|_| ApiError::not_found("data source not found"))?;

    // 3. Entitlement gate — one of three call sites, alongside `ai_query`/`ai_viz`.
    if !state.entitlements.ai_features(&org) {
        return Err(ApiError::forbidden(
            "AI data-quality suggestions aren't enabled for this organization",
        ));
    }

    // 4. Cheap request-shape bounds, before any I/O.
    if payload.tables.is_empty() {
        return Err(ApiError::bad_request("tables must not be empty"));
    }
    if payload.tables.len() > MAX_TABLES_PER_RUN {
        return Err(ApiError::bad_request(format!(
            "tables must not exceed {MAX_TABLES_PER_RUN} entries"
        )));
    }

    let effective = effective_ai_config(&org, &state.config)?;

    // 5. Nothing configured: bail before spending a rate-limit slot — a rejected request must
    // never cost the caller quota.
    if effective.provider.is_none() {
        return Ok(Json(SuggestMonitorsResponse::NotConfigured {
            message: NOT_CONFIGURED_MESSAGE.to_string(),
        }));
    }

    // 6. Rate limit (shared bucket with Ask/Fix, per user per minute) — before any provider call.
    if let Some(rate_limited) = check_rate_limit(
        &mut state.redis.clone(),
        org.id,
        user.id,
        effective.rate_limit_per_min,
    )
    .await?
    {
        return Ok(Json(SuggestMonitorsResponse::RateLimited {
            message: rate_limited.message,
            retry_after_secs: rate_limited.retry_after_secs,
        }));
    }

    // 7. Schema (cached when available), then validate every requested table actually exists on
    // it — reject rather than silently drop an unknown entry. The per-request transaction is
    // released first: the fallback path can hit the customer's warehouse and the LLM chain below
    // is slow.
    let profile = usable_profile(&ds);
    let recent = load_recent_queries(&db, user.org_id, user.id, ds.id).await?;
    let include_samples = include_sample_values(&org.settings);
    scope.detach(db).await?;

    let mut all_tables = schema_for_ai(&state, &ds, user.id).await?;
    let missing = |tables: &[arcodash_core::connectors::TableSchema]| {
        payload
            .tables
            .iter()
            .any(|r| !tables.iter().any(|t| &t.name == r))
    };
    // A table added since the last cache refresh is not an error: re-check against the warehouse.
    if missing(&all_tables) && cached_schema(&ds).is_some() {
        all_tables = live_schema_for_ai(&state, &ds, user.id).await?;
    }
    for requested in &payload.tables {
        if !all_tables.iter().any(|t| &t.name == requested) {
            return Err(ApiError::bad_request(format!(
                "table '{requested}' not found in this data source's schema"
            )));
        }
    }
    // Preserve schema order (not the request's order) — tables come back in schema order.
    let scoped: Vec<_> = all_tables
        .into_iter()
        .filter(|t| payload.tables.contains(&t.name))
        .collect();

    // 8. Build + dispatch.
    let schema_text = build_context(
        &scoped,
        profile.as_ref(),
        "",
        None,
        &recent,
        include_samples,
    );
    let prompt = build_suggest_prompt_from_text(&ds.r#type, &schema_text);

    let chain = resolve_provider_chain(&org, &state.config)?;
    if chain.is_empty() {
        return Ok(Json(SuggestMonitorsResponse::NotConfigured {
            message: NOT_CONFIGURED_MESSAGE.to_string(),
        }));
    }
    let (served_kind, outcome) = run_fallback_chain(
        &scope,
        &org,
        Some(user.id),
        &state.http,
        &chain,
        fallback_enabled(&org.settings),
        "monitor_suggestions",
        &prompt,
        provider_for,
        |provider, http, api_key, model, prompt| {
            provider.suggest_monitors(http, api_key, model, prompt)
        },
        |gen| (gen.tokens_in, gen.tokens_out),
    )
    .await;

    let generation = match outcome {
        Ok(gen) => gen,
        Err(AiError::BudgetExceeded { cap, resets_at, .. }) => {
            return Err(ApiError::too_many_requests_with_code(
                arcodash_core::ai::budget_message(cap, resets_at),
                "ai_budget_exceeded",
            ))
        }
        Err(err) => {
            return Ok(Json(map_generation_error(
                err,
                served_kind,
                "Couldn't suggest checks for this data source.",
            )))
        }
    };

    // 9. Cross-run dedup lookup — runs under the caller's own identity, so a monitor they
    // can't see never leaks into a "covered" row. `list_for_user` already resolves each
    // monitor's query row, so filtering by data source here costs nothing extra. Its own short
    // scoped transaction, opened fresh since the per-request one was released above.
    let dedup_txn = scope.begin(&org).await?;
    let existing = monitor_service::list_for_user(
        dedup_txn.db(),
        org.id,
        &user,
        None,
        None,
        Some(ds.id),
        None,
    )
    .await?;
    dedup_txn.commit().await?;
    let existing_facts: Vec<ExistingMonitorFacts> = existing
        .iter()
        .map(|(m, q)| {
            let column = m.options.get("column").and_then(serde_json::Value::as_str);
            monitor_facts(m.id, m.name.clone(), column, &q.query, &scoped)
        })
        .collect();

    let (suggestions, covered) = build_response(&scoped, &generation.suggestions, &existing_facts);

    Ok(Json(SuggestMonitorsResponse::Ok(DataSourceSuggestions {
        suggestions,
        covered,
        scanned_tables: scoped.len(),
        served_by: served_kind.as_str().to_string(),
    })))
}

const SAMPLE_ROWS: usize = 5;

#[derive(Debug, Deserialize, Default)]
pub struct SuggestQueryMonitorsRequest {
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct QuerySuggestionsOk {
    suggestions: Vec<QueryMonitorSuggestion>,
    already_covered: Vec<CoveredColumn>,
    unsupported: Option<String>,
    served_by: String,
}

fn no_result() -> ApiError {
    ApiError::conflict_with_code(
        "Run this query once so AI can see its columns.",
        "no_result",
    )
}

/// `POST /api/queries/{id}/suggest_monitors` — body `{ "prompt"?: string }`.
///
/// Suggests monitors on the columns of the query's published result. Every permission, entitlement
/// and result check runs before the rate-limit charge and any provider call.
pub async fn suggest_query_monitors(
    State(state): State<AppState>,
    db: Db,
    scope: RequestScope,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(body): Json<SuggestQueryMonitorsRequest>,
) -> Result<Json<AiJsonResponse<QuerySuggestionsOk>>, ApiError> {
    let q = query_service::get_for_user(&db, org.id, &user, id).await?;
    if !query_service::is_published(&q) {
        return Err(ApiError::conflict_with_code(
            "Publish this query before adding monitors.",
            "query_unpublished",
        ));
    }

    let served = query_service::resolve_serving_content(&q);
    let ds_id = q.published_data_source_id.unwrap_or(q.data_source_id);
    let ds = data_source_service::get_for_user(&db, org.id, &user, ds_id).await?;
    let groups = crate::authz::permissions::load_effective_groups(&db, &user).await?;
    query_service::require_data_source_execute_access(&db, &user, &groups, ds.id).await?;
    if !crate::authz::permissions::user_has_permission(&user, &groups, "create_monitor") {
        return Err(ApiError::forbidden("missing the create_monitor permission"));
    }
    if !state.entitlements.ai_features(&org) {
        return Err(ApiError::forbidden(
            "AI data-quality suggestions aren't enabled for this organization",
        ));
    }

    let prompt = body
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    if prompt
        .as_ref()
        .is_some_and(|p| p.chars().count() > MAX_QUESTION_CHARS)
    {
        return Err(ApiError::bad_request(format!(
            "prompt must not exceed {MAX_QUESTION_CHARS} characters"
        )));
    }

    let result_id = served.latest_query_data_id.ok_or_else(no_result)?;
    let result =
        result_service::get_with_data(&db, state.result_reader(), org.id, &user, result_id)
            .await
            .map_err(|e| {
                if e.status == axum::http::StatusCode::NOT_FOUND {
                    no_result()
                } else {
                    e
                }
            })?;
    let data = result.data.as_ref().ok_or_else(no_result)?;
    let columns: Vec<ColumnMeta> = data
        .get("columns")
        .and_then(serde_json::Value::as_array)
        .map(|cols| {
            cols.iter()
                .filter_map(|c| {
                    Some(ColumnMeta {
                        name: c.get("name")?.as_str()?.to_string(),
                        friendly_type: c
                            .get("friendly_type")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let effective = effective_ai_config(&org, &state.config)?;
    if effective.provider.is_none() {
        return Ok(Json(AiJsonResponse::NotConfigured {
            message: NOT_CONFIGURED_MESSAGE.to_string(),
        }));
    }
    if let Some(rate_limited) = check_rate_limit(
        &mut state.redis.clone(),
        org.id,
        user.id,
        effective.rate_limit_per_min,
    )
    .await?
    {
        return Ok(Json(AiJsonResponse::RateLimited {
            message: rate_limited.message,
            retry_after_secs: rate_limited.retry_after_secs,
        }));
    }

    let include_samples = include_sample_values(&org.settings);
    let sample_rows = include_samples.then(|| {
        data.get("rows")
            .and_then(serde_json::Value::as_array)
            .map(|r| r.iter().take(SAMPLE_ROWS).cloned().collect())
            .unwrap_or_default()
    });
    let profile_context = cached_schema(&ds).map(|tables| {
        build_context(
            &tables,
            usable_profile(&ds).as_ref(),
            "",
            Some(&served.query),
            &[],
            include_samples,
        )
    });
    let existing: Vec<ExistingQueryMonitor> =
        monitor_service::list_for_user(&db, org.id, &user, None, Some(q.id), None, None)
            .await?
            .into_iter()
            .filter_map(|(m, _)| {
                let opts = arcodash_core::models::parse_monitor_options(m.id, &m.options);
                Some(ExistingQueryMonitor {
                    monitor_id: m.id,
                    monitor_name: m.name,
                    kind: opts.kind.unwrap_or_default(),
                    column: opts.column?,
                })
            })
            .collect();
    let input = QuerySuggestInput {
        sql: served.query.clone(),
        dialect: dialect_name(&ds.r#type).to_string(),
        columns,
        sample_rows,
        profile_context,
        existing,
        prompt,
        query_name: served.name.clone(),
    };
    scope.detach(db).await?;

    let chain = resolve_provider_chain(&org, &state.config)?;
    if chain.is_empty() {
        return Ok(Json(AiJsonResponse::NotConfigured {
            message: NOT_CONFIGURED_MESSAGE.to_string(),
        }));
    }
    let sink = ScopeSink::new(&scope, &org, Some(user.id));
    let (kind, outcome) = suggest_for_query(
        &input,
        &state.http,
        &chain,
        fallback_enabled(&org.settings),
        provider_for,
        &sink,
    )
    .await;

    match outcome {
        Ok(s) => Ok(Json(AiJsonResponse::Ok(QuerySuggestionsOk {
            suggestions: s.suggestions,
            already_covered: s.already_covered,
            unsupported: s.unsupported,
            served_by: kind.as_str().to_string(),
        }))),
        Err(AiError::BudgetExceeded { cap, resets_at, .. }) => {
            Err(ApiError::too_many_requests_with_code(
                arcodash_core::ai::budget_message(cap, resets_at),
                "ai_budget_exceeded",
            ))
        }
        Err(err) => Ok(Json(map_generation_error(
            err,
            kind,
            "Couldn't suggest monitors for this query.",
        ))),
    }
}

impl<T: Serialize> From<AiFailure> for AiJsonResponse<T> {
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

fn map_generation_error<T: Serialize>(
    err: AiError,
    kind: AiProviderKind,
    failed: &str,
) -> AiJsonResponse<T> {
    ai_failure(err, kind, failed).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::handlers::ai::AI_BUSY_MESSAGE;

    fn map(err: AiError, kind: AiProviderKind) -> SuggestMonitorsResponse {
        map_generation_error(err, kind, "Couldn't suggest checks for this data source.")
    }

    #[test]
    fn map_generation_error_wire_contract() {
        match map(AiError::InvalidKey, AiProviderKind::Gemini) {
            SuggestMonitorsResponse::InvalidKey { message } => assert_eq!(
                message,
                "The configured Gemini key was rejected. An admin should check AI Settings."
            ),
            other => panic!("expected InvalidKey, got {other:?}"),
        }

        match map(AiError::QuotaExceeded { free_tier: false }, AiProviderKind::OpenAi) {
            SuggestMonitorsResponse::QuotaExceeded { message } => assert_eq!(
                message,
                "OpenAI quota exhausted for the configured key. Try again later or ask an admin to check the plan."
            ),
            other => panic!("expected QuotaExceeded, got {other:?}"),
        }

        match map(
            AiError::GenerationFailed("bad json".to_string()),
            AiProviderKind::Anthropic,
        ) {
            SuggestMonitorsResponse::GenerationFailed { message } => {
                assert_eq!(message, "Couldn't suggest checks for this data source.")
            }
            other => panic!("expected GenerationFailed, got {other:?}"),
        }

        match map(
            AiError::Network("connection reset".to_string()),
            AiProviderKind::Gemini,
        ) {
            SuggestMonitorsResponse::GenerationFailed { message } => {
                assert_eq!(message, AI_BUSY_MESSAGE)
            }
            other => panic!("expected GenerationFailed, got {other:?}"),
        }

        match map(
            AiError::Unavailable("503".to_string()),
            AiProviderKind::OpenAi,
        ) {
            SuggestMonitorsResponse::GenerationFailed { message } => {
                assert_eq!(message, AI_BUSY_MESSAGE)
            }
            other => panic!("expected GenerationFailed, got {other:?}"),
        }

        match map(
            AiError::RateLimited {
                retry_after_secs: 30,
                free_tier: false,
            },
            AiProviderKind::Anthropic,
        ) {
            SuggestMonitorsResponse::RateLimited {
                message,
                retry_after_secs,
            } => {
                assert_eq!(
                    message,
                    "Anthropic is rate-limiting the configured key right now."
                );
                assert_eq!(retry_after_secs, 30);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn ok_variant_serializes_flat() {
        let v = serde_json::to_value(AiJsonResponse::Ok(QuerySuggestionsOk {
            suggestions: vec![],
            already_covered: vec![],
            unsupported: None,
            served_by: "openai".into(),
        }))
        .unwrap();
        assert_eq!(v["status"], "ok");
        assert!(v["suggestions"].is_array());
        assert_eq!(v["served_by"], "openai");
    }

    #[test]
    fn gemini_free_tier_quota_gets_billing_copy() {
        match map(
            AiError::QuotaExceeded { free_tier: true },
            AiProviderKind::Gemini,
        ) {
            SuggestMonitorsResponse::QuotaExceeded { message } => assert_eq!(
                message,
                "Gemini's free-tier daily limit is used up for this key. Try again tomorrow, or ask an admin to enable billing on the key's Google Cloud project."
            ),
            other => panic!("expected QuotaExceeded, got {other:?}"),
        }
    }
}
