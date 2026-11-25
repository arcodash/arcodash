//! `POST|GET /api/dashboards/{id}/narrative` — plain-language AI summary of a dashboard's cached
//! widget results. The stored digest (the rows sent to the provider) is never returned.

use std::collections::{HashMap, HashSet};

use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::api::handlers::ai::{provider_for, AiError};
use crate::api::handlers::ai_query::{ai_failure, check_rate_limit, AiFailure, ScopeSink};
use crate::api::handlers::ai_settings::{
    effective_ai_config, fallback_enabled, include_sample_values, resolve_provider_chain,
};
use crate::auth::{CurrentOrg, CurrentUser};
use crate::db::{Db, RequestScope};
use crate::error::ApiError;
use crate::extract::Json;
use crate::services::{dashboard_service, query_service, result_service};
use crate::state::AppState;
use arcodash_core::ai::narrative;
use arcodash_core::ai::settings::dashboard_narratives;
use arcodash_core::entities::dashboard_narrative;

const NOT_CONFIGURED_MESSAGE: &str =
    "AI isn't set up for your organization yet. An admin can add a provider API key in AI Settings.";
const DISABLED_MESSAGE: &str = "Dashboard explanations are turned off for this workspace.";
const FAILED_MESSAGE: &str = "Couldn't explain this dashboard.";
const EMPTY_MESSAGE: &str = "This dashboard has no widgets you can explain yet.";

/// Whether dashboard narratives are on: they send result rows, so they also need sample values.
#[must_use]
pub fn narratives_enabled(settings: &serde_json::Value) -> bool {
    dashboard_narratives(settings) && include_sample_values(settings)
}

#[derive(Debug, Serialize)]
pub struct NarrativeResponse {
    pub id: i64,
    pub dashboard_id: i64,
    pub created_at: DateTime<Utc>,
    pub source: String,
    pub summary: String,
    pub highlights: serde_json::Value,
    pub body: Option<String>,
    pub confidence: String,
    pub model: String,
}

impl NarrativeResponse {
    fn new(m: dashboard_narrative::Model, c: narrative::NarrativeContent) -> Self {
        Self {
            id: m.id,
            dashboard_id: m.dashboard_id,
            created_at: m.created_at.with_timezone(&Utc),
            source: m.source,
            summary: c.summary,
            highlights: c.highlights,
            body: c.body,
            confidence: m.confidence,
            model: m.model,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "status")]
pub enum ExplainDashboardResponse {
    #[serde(rename = "ok")]
    Ok { narrative: NarrativeResponse },
    #[serde(rename = "disabled")]
    Disabled { message: String },
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

fn failed(message: &str) -> Json<ExplainDashboardResponse> {
    Json(ExplainDashboardResponse::GenerationFailed {
        message: message.to_string(),
    })
}

/// Reuses the shared per-provider wording; only the generic failure text is feature-specific.
fn map_error(err: AiError, kind: arcodash_core::ai::AiProviderKind) -> ExplainDashboardResponse {
    match ai_failure(err, kind, FAILED_MESSAGE) {
        AiFailure::InvalidKey(message) => ExplainDashboardResponse::InvalidKey { message },
        AiFailure::QuotaExceeded(message) => ExplainDashboardResponse::QuotaExceeded { message },
        AiFailure::RateLimited {
            message,
            retry_after_secs,
        } => ExplainDashboardResponse::RateLimited {
            message,
            retry_after_secs,
        },
        AiFailure::GenerationFailed(message) => {
            ExplainDashboardResponse::GenerationFailed { message }
        }
    }
}

/// `POST /api/dashboards/{id}/narrative` — generate and store a fresh narrative for the caller.
pub async fn explain_dashboard(
    State(state): State<AppState>,
    db: Db,
    scope: RequestScope,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<ExplainDashboardResponse>, ApiError> {
    let dash = dashboard_service::get_by_id(&db, org.id, &user, id).await?;

    if !state.entitlements.ai_features(&org) {
        return Err(ApiError::forbidden(
            "AI dashboard explanations aren't enabled for this organization",
        ));
    }
    if !narratives_enabled(&org.settings) {
        return Ok(Json(ExplainDashboardResponse::Disabled {
            message: DISABLED_MESSAGE.to_string(),
        }));
    }

    let effective = effective_ai_config(&org, &state.config)?;
    let chain = resolve_provider_chain(&org, &state.config)?;
    if effective.provider.is_none() || chain.is_empty() {
        return Ok(Json(ExplainDashboardResponse::NotConfigured {
            message: NOT_CONFIGURED_MESSAGE.to_string(),
        }));
    }

    if let Some(limited) = check_rate_limit(
        &mut state.redis.clone(),
        org.id,
        user.id,
        effective.rate_limit_per_min,
    )
    .await?
    {
        return Ok(Json(ExplainDashboardResponse::RateLimited {
            message: limited.message,
            retry_after_secs: limited.retry_after_secs,
        }));
    }

    let widgets = narrative::dashboard_widgets(&db, &dash).await?;
    // Both gates apply: the query itself and the stored result it serves.
    let mut verdicts: HashMap<i64, bool> = HashMap::new();
    for w in &widgets {
        let qid = w.query.id;
        if verdicts.contains_key(&qid) {
            continue;
        }
        let ok = query_service::get_for_user(&db, org.id, &user, qid)
            .await
            .is_ok()
            && match narrative::serving_result_id(&w.query) {
                Some(rid) => result_service::get(&db, org.id, &user, rid).await.is_ok(),
                None => true,
            };
        verdicts.insert(qid, ok);
    }
    let readable: HashSet<i64> = verdicts
        .into_iter()
        .filter_map(|(q, ok)| ok.then_some(q))
        .collect();

    let storage = state.storage.resolve(&state.db, org.id).await?;
    let digest =
        narrative::build_digest(&db, &state.db, &*state.storage, org.id, &widgets, &readable)
            .await?;
    if digest.widgets.is_empty() {
        return Ok(failed(EMPTY_MESSAGE));
    }
    let previous = narrative::previous_digest(&db, &storage, dash.id, &digest, &readable).await?;
    let prompt = narrative::build_prompt(&dash.name, &digest, previous.as_ref());

    scope.detach(db).await?;

    let sink = ScopeSink::new(&scope, &org, Some(user.id));
    let (kind, model, outcome) = narrative::generate(
        &state.http,
        &chain,
        fallback_enabled(&org.settings),
        provider_for,
        &sink,
        "dashboard_narrative",
        &prompt,
        &digest,
    )
    .await;

    let generated = match outcome {
        Ok(n) => n,
        Err(AiError::BudgetExceeded { cap, resets_at, .. }) => {
            return Err(ApiError::too_many_requests_with_code(
                arcodash_core::ai::budget_message(cap, resets_at),
                "ai_budget_exceeded",
            ))
        }
        Err(err) => return Ok(Json(map_error(err, kind))),
    };

    let txn = scope.begin(&org).await?;
    let (row, content) = match narrative::store(
        txn.db(),
        &storage,
        org.id,
        dash.id,
        Some(user.id),
        "manual",
        &generated,
        &digest,
        &model,
    )
    .await
    {
        Ok(stored) => stored,
        Err(e) => {
            if let narrative::NarrativeError::Storage(se) = &e {
                narrative::report_unreachable(&*state.storage, &state.db, org.id, se).await;
            }
            return Err(e.into());
        }
    };
    txn.commit().await?;

    Ok(Json(ExplainDashboardResponse::Ok {
        narrative: NarrativeResponse::new(row, content),
    }))
}

#[derive(Debug, Serialize)]
pub struct LatestNarrativeResponse {
    pub narrative: NarrativeResponse,
}

/// `GET /api/dashboards/{id}/narrative` — the caller's own newest narrative, or 404.
pub async fn latest_narrative(
    State(state): State<AppState>,
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<LatestNarrativeResponse>, ApiError> {
    let dash = dashboard_service::get_by_id(&db, org.id, &user, id).await?;
    let row = narrative::latest_for_user(&db, dash.id, user.id)
        .await?
        .ok_or_else(|| ApiError::not_found("no narrative yet"))?;
    let storage = state.storage.resolve(&state.db, org.id).await?;
    let content = match narrative::load_narrative_content(&storage, &row).await {
        Ok(c) => c,
        Err(e) => {
            narrative::report_unreachable(&*state.storage, &state.db, org.id, &e).await;
            return Err(e.into());
        }
    }
    .ok_or_else(|| ApiError::not_found("no narrative yet"))?;
    Ok(Json(LatestNarrativeResponse {
        narrative: NarrativeResponse::new(row, content),
    }))
}
