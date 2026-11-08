//! `POST /api/queries/suggest_visualization` — "Auto Visualize".
//!
//! One click after a query returns results, ask the org's active AI provider to pick the best of
//! the 4 visualization types (Table/Chart/Counter/Funnel) and its full config, in one shot.
//!
//! Mirrors `ai_query.rs`'s `generate_sql` handler shape (auth/org/rate-limit/provider-resolution
//! sequence, fallback chain, tagged response), but needs no `data_source_id` — the caller already
//! has the result set in hand and sends only its column metadata (name + `friendly_type`) and row
//! count, never row values, so this endpoint never touches the connector or live data at all.

use crate::extract::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::api::handlers::ai::viz::{
    build_viz_prompt, sanitize_chart_suggestion, ColumnMeta, VizSuggestion,
};
use crate::api::handlers::ai::{provider_for, AiError, AiProvider, AiProviderKind};
use crate::api::handlers::ai_ask::PreviewColumn;
use crate::api::handlers::ai_query::{ai_failure, check_rate_limit, run_fallback_chain, AiFailure};
use crate::api::handlers::ai_settings::{
    effective_ai_config, fallback_enabled, resolve_provider_chain, ResolvedProvider,
};
use crate::auth::{CurrentOrg, CurrentUser};
use crate::db::{Db, RequestScope};
use crate::error::ApiError;
use crate::state::AppState;
use arcodash_core::entities::organization;

/// A result set legitimately has at most a few dozen columns; this only exists to bound the
/// prompt built from them.
const MAX_COLUMNS_PER_REQUEST: usize = 200;
/// Longer than any real column identifier — a name past this is not a name the prompt needs.
const MAX_COLUMN_NAME_CHARS: usize = 200;

#[derive(Debug, Deserialize)]
pub struct SuggestVisualizationRequest {
    pub columns: Vec<ColumnMeta>,
    pub row_count: usize,
}

/// Tagged response, shaped like `GenerateSqlResponse`.
///
/// Same reason as there — the frontend needs to distinguish "here's your suggestion" from every
/// recoverable failure mode without parsing HTTP status + message heuristically.
#[derive(Debug, Serialize)]
#[serde(tag = "status")]
pub enum SuggestVisualizationResponse {
    #[serde(rename = "ok")]
    Ok {
        title: String,
        #[serde(rename = "type")]
        viz_type: &'static str,
        options: serde_json::Value,
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

const NOT_CONFIGURED_MESSAGE: &str =
    "AI visualization suggestions aren't configured. An admin can add a provider API key in AI Settings.";

/// Rejects an oversized or malformed column list before it reaches [`build_viz_prompt`], which
/// otherwise iterates every element twice with no cap.
pub(crate) fn check_column_bounds(columns: &[ColumnMeta]) -> Result<(), ApiError> {
    if columns.len() > MAX_COLUMNS_PER_REQUEST {
        return Err(ApiError::bad_request(format!(
            "columns must not exceed {MAX_COLUMNS_PER_REQUEST} entries"
        )));
    }
    if columns
        .iter()
        .any(|c| c.name.chars().count() > MAX_COLUMN_NAME_CHARS)
    {
        return Err(ApiError::bad_request(
            "a column name exceeds the maximum allowed length",
        ));
    }
    Ok(())
}

/// `POST /api/queries/suggest_visualization` — any authenticated user may call this (configuring
/// the provider key is admin-only, using the feature is not), same as `generate_sql`.
pub async fn suggest_visualization(
    State(state): State<AppState>,
    db: Db,
    scope: RequestScope,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<SuggestVisualizationRequest>,
) -> Result<Json<SuggestVisualizationResponse>, ApiError> {
    // Entitlement gate — defence-in-depth alongside `ai_query`/`ai_monitors`.
    if !state.entitlements.ai_features(&org) {
        return Err(ApiError::forbidden(
            "AI visualization suggestions aren't enabled for this organization",
        ));
    }

    // Cheap request-shape bounds, before any I/O — the prompt iterates every column twice with
    // no cap otherwise.
    check_column_bounds(&payload.columns)?;

    let effective = effective_ai_config(&org, &state.config)?;

    // Nothing configured: bail before spending a rate-limit slot — a rejected request must
    // never cost the caller quota.
    if effective.provider.is_none() {
        return Ok(Json(SuggestVisualizationResponse::NotConfigured {
            message: NOT_CONFIGURED_MESSAGE.to_string(),
        }));
    }

    // Rate limit (per user, per minute) — before any provider call, charged exactly once
    // regardless of how many providers the fallback chain below ends up attempting internally.
    if let Some(rate_limited) = check_rate_limit(
        &mut state.redis.clone(),
        org.id,
        user.id,
        effective.rate_limit_per_min,
    )
    .await?
    {
        return Ok(Json(SuggestVisualizationResponse::RateLimited {
            message: rate_limited.message,
            retry_after_secs: rate_limited.retry_after_secs,
        }));
    }

    let chain = resolve_provider_chain(&org, &state.config)?;
    if chain.is_empty() {
        return Ok(Json(SuggestVisualizationResponse::NotConfigured {
            message: NOT_CONFIGURED_MESSAGE.to_string(),
        }));
    }

    // No connector call on this path (pure LLM) — release the per-request transaction right
    // before the fallback chain's provider calls, the only slow step here.
    scope.detach(db).await?;

    let (served_kind, outcome) = suggest_for_columns(
        &scope,
        &org,
        Some(user.id),
        &state.http,
        &chain,
        fallback_enabled(&org.settings),
        &payload.columns,
        payload.row_count,
        provider_for,
    )
    .await;

    Ok(Json(match outcome {
        Ok(pick) => SuggestVisualizationResponse::Ok {
            title: pick.title,
            viz_type: pick.viz_type,
            options: pick.options,
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

pub(crate) struct VizPick {
    pub title: String,
    pub viz_type: &'static str,
    pub options: serde_json::Value,
}

impl From<&PreviewColumn> for ColumnMeta {
    fn from(c: &PreviewColumn) -> Self {
        Self {
            name: c.name.clone(),
            friendly_type: c.friendly_type.clone(),
        }
    }
}

/// Prompt, fallback chain (writes the `ai_usage` row, enforces the budget cap) and output
/// sanitizing for one visualization pick.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn suggest_for_columns<F>(
    scope: &RequestScope,
    org: &organization::Model,
    user_id: Option<i64>,
    http: &reqwest::Client,
    chain: &[ResolvedProvider],
    fallback: bool,
    columns: &[ColumnMeta],
    row_count: usize,
    provider_lookup: F,
) -> (AiProviderKind, Result<VizPick, AiError>)
where
    F: Fn(AiProviderKind, Option<String>) -> Box<dyn AiProvider>,
{
    let prompt = build_viz_prompt(columns, row_count);
    let (kind, outcome) = run_fallback_chain(
        scope,
        org,
        user_id,
        http,
        chain,
        fallback,
        "auto_visualize",
        &prompt,
        provider_lookup,
        |provider, http, api_key, model, prompt| {
            provider.suggest_visualization(http, api_key, model, prompt)
        },
        |gen| (gen.tokens_in, gen.tokens_out),
    )
    .await;
    let pick = outcome.map(|mut gen| {
        // Only Chart can name a column the model got wrong; the other types are validated by
        // their own deserialize.
        if let VizSuggestion::Chart(opts) = gen.suggestion {
            gen.suggestion = VizSuggestion::Chart(sanitize_chart_suggestion(opts, columns));
        }
        VizPick {
            // Never persist an empty title, even if the model omitted or blanked it.
            title: gen
                .title
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| "AI visualization".to_string()),
            viz_type: gen.suggestion.type_str(),
            options: gen.suggestion.options_json(),
        }
    });
    (kind, pick)
}

impl From<AiFailure> for SuggestVisualizationResponse {
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

fn map_generation_error(err: AiError, kind: AiProviderKind) -> SuggestVisualizationResponse {
    ai_failure(
        err,
        kind,
        "Couldn't suggest a visualization for this result.",
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::handlers::ai::AI_BUSY_MESSAGE;

    fn column(name: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.to_string(),
            friendly_type: "string".to_string(),
        }
    }

    #[test]
    fn check_column_bounds_rejects_too_many_columns() {
        let columns: Vec<ColumnMeta> = (0..=MAX_COLUMNS_PER_REQUEST)
            .map(|i| column(&format!("col_{i}")))
            .collect();
        assert!(check_column_bounds(&columns).is_err());
    }

    #[test]
    fn check_column_bounds_rejects_an_oversized_column_name() {
        let columns = vec![column(&"x".repeat(MAX_COLUMN_NAME_CHARS + 1))];
        assert!(check_column_bounds(&columns).is_err());
    }

    #[test]
    fn check_column_bounds_accepts_the_boundary() {
        let mut columns: Vec<ColumnMeta> = (0..MAX_COLUMNS_PER_REQUEST - 1)
            .map(|i| column(&format!("col_{i}")))
            .collect();
        columns.push(column(&"x".repeat(MAX_COLUMN_NAME_CHARS)));
        assert_eq!(columns.len(), MAX_COLUMNS_PER_REQUEST);
        assert!(check_column_bounds(&columns).is_ok());
    }

    #[test]
    fn map_generation_error_wire_contract() {
        match map_generation_error(AiError::InvalidKey, AiProviderKind::Gemini) {
            SuggestVisualizationResponse::InvalidKey { message } => assert_eq!(
                message,
                "The configured Gemini key was rejected. An admin should check AI Settings."
            ),
            other => panic!("expected InvalidKey, got {other:?}"),
        }

        match map_generation_error(AiError::QuotaExceeded { free_tier: false }, AiProviderKind::OpenAi) {
            SuggestVisualizationResponse::QuotaExceeded { message } => assert_eq!(
                message,
                "OpenAI quota exhausted for the configured key. Try again later or ask an admin to check the plan."
            ),
            other => panic!("expected QuotaExceeded, got {other:?}"),
        }

        match map_generation_error(
            AiError::GenerationFailed("bad json".to_string()),
            AiProviderKind::Anthropic,
        ) {
            SuggestVisualizationResponse::GenerationFailed { message } => {
                assert_eq!(message, "Couldn't suggest a visualization for this result.")
            }
            other => panic!("expected GenerationFailed, got {other:?}"),
        }

        match map_generation_error(
            AiError::Network("connection reset".to_string()),
            AiProviderKind::Gemini,
        ) {
            SuggestVisualizationResponse::GenerationFailed { message } => {
                assert_eq!(message, AI_BUSY_MESSAGE)
            }
            other => panic!("expected GenerationFailed, got {other:?}"),
        }

        match map_generation_error(
            AiError::Unavailable("503".to_string()),
            AiProviderKind::OpenAi,
        ) {
            SuggestVisualizationResponse::GenerationFailed { message } => {
                assert_eq!(message, AI_BUSY_MESSAGE)
            }
            other => panic!("expected GenerationFailed, got {other:?}"),
        }

        match map_generation_error(
            AiError::RateLimited {
                retry_after_secs: 30,
                free_tier: false,
            },
            AiProviderKind::Anthropic,
        ) {
            SuggestVisualizationResponse::RateLimited {
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
    fn gemini_free_tier_quota_gets_billing_copy() {
        match map_generation_error(
            AiError::QuotaExceeded { free_tier: true },
            AiProviderKind::Gemini,
        ) {
            SuggestVisualizationResponse::QuotaExceeded { message } => assert_eq!(
                message,
                "Gemini's free-tier daily limit is used up for this key. Try again tomorrow, or ask an admin to enable billing on the key's Google Cloud project."
            ),
            other => panic!("expected QuotaExceeded, got {other:?}"),
        }
    }
}
