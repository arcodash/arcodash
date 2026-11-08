//! `POST /api/monitors/ask` — natural language to a proposed monitor.
//!
//! Drives the Ask loop for a verified query, then asks the provider for one monitor over its result
//! columns. Streams the Ask events plus `suggesting` and `suggested`; any failure ends in a single
//! `failed`.
//!
//! Invariants: permission checks run before the first provider call; only the loop's exact
//! `is_read_only_sql` result is proposed; sample rows reach the second prompt only when the org
//! allows it; nothing is created — accepting is the client's job.

use axum::extract::State;
use axum::response::sse::Event;
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use crate::api::handlers::ai::monitor_suggestions::{suggest_for_query, QuerySuggestInput};
use crate::api::handlers::ai::viz::ColumnMeta;
use crate::api::handlers::ai::{
    dialect_name, is_read_only_sql, provider_for, AiError, AiProvider, AiProviderKind,
    AI_BUSY_MESSAGE,
};
use crate::api::handlers::ai_ask::{
    ask_loop_outcome, emit, prepare_ask, AskCtx, AskRequest, PreviewRunner,
};
use crate::api::handlers::ai_query::ScopeSink;
use crate::api::handlers::ai_settings::include_sample_values;
use crate::api::handlers::ai_widget::{emit_failed, truncate_name};
use crate::auth::{CurrentOrg, CurrentUser};
use crate::authz::permissions::{load_effective_groups, user_has_permission};
use crate::db::{Db, RequestScope};
use crate::error::ApiError;
use crate::extract::Json;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct MonitorAskRequest {
    pub data_source_id: i64,
    pub prompt: String,
}

/// What the monitor step needs beyond the Ask context.
pub struct MonitorAskCtx {
    pub data_source_id: i64,
    pub include_samples: bool,
}

pub async fn monitor_ask_loop<F, P>(
    ctx: AskCtx,
    m: MonitorAskCtx,
    provider_lookup: F,
    preview: P,
    tx: mpsc::Sender<Event>,
) where
    F: Fn(AiProviderKind, Option<String>) -> Box<dyn AiProvider> + Send + Sync,
    P: PreviewRunner,
{
    let (scope, org, http, chain) = (
        ctx.scope.clone(),
        ctx.org.clone(),
        ctx.http.clone(),
        ctx.chain.clone(),
    );
    let fallback = ctx.fallback_enabled;
    let (prompt, schema_text, user_id) =
        (ctx.question.clone(), ctx.schema_text.clone(), ctx.user_id);
    let dialect = dialect_name(&ctx.data_source_type).to_string();

    let Some(outcome) = ask_loop_outcome(ctx, &provider_lookup, preview, &tx).await else {
        return;
    };
    if !outcome.executed || !is_read_only_sql(&outcome.sql) {
        let msg = outcome
            .error
            .unwrap_or_else(|| "Couldn't get a working query for this prompt.".to_string());
        emit_failed(&tx, msg).await;
        return;
    }
    if emit(&tx, "suggesting", json!({})).await.is_none() {
        return;
    }

    let input = QuerySuggestInput {
        sql: outcome.sql.clone(),
        dialect,
        columns: outcome.columns.iter().map(ColumnMeta::from).collect(),
        sample_rows: m.include_samples.then_some(outcome.rows),
        profile_context: Some(schema_text),
        existing: vec![],
        query_name: truncate_name(&prompt),
        prompt: Some(prompt),
    };
    let sink = ScopeSink::new(&scope, &org, user_id);
    let (_, result) =
        suggest_for_query(&input, &http, &chain, fallback, &provider_lookup, &sink).await;

    match result {
        Ok(mut s) if !s.suggestions.is_empty() => {
            let suggestion = s.suggestions.swap_remove(0);
            let _ = emit(
                &tx,
                "suggested",
                json!({
                    "sql": outcome.sql,
                    "data_source_id": m.data_source_id,
                    "suggestion": suggestion,
                }),
            )
            .await;
        }
        Ok(s) => {
            let msg = s
                .unsupported
                .unwrap_or_else(|| "Couldn't propose a monitor for this query.".to_string());
            emit_failed(&tx, msg).await;
        }
        Err(AiError::BudgetExceeded { cap, resets_at, .. }) => {
            emit_failed(&tx, arcodash_core::ai::budget_message(cap, resets_at)).await;
        }
        Err(AiError::Unavailable(_) | AiError::Network(_)) => {
            emit_failed(&tx, AI_BUSY_MESSAGE.to_string()).await;
        }
        Err(_) => {
            emit_failed(
                &tx,
                "Couldn't propose a monitor for this query.".to_string(),
            )
            .await;
        }
    }
}

pub async fn ask_monitor(
    State(state): State<AppState>,
    db: Db,
    scope: RequestScope,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Json(body): Json<MonitorAskRequest>,
) -> Result<Response, ApiError> {
    if body.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt must not be empty"));
    }
    let groups = load_effective_groups(&db, &user).await?;
    if !user_has_permission(&user, &groups, "create_query")
        || !user_has_permission(&user, &groups, "create_monitor")
    {
        return Err(ApiError::forbidden(
            "creating a monitor with AI requires query and monitor create permissions",
        ));
    }
    let m = MonitorAskCtx {
        data_source_id: body.data_source_id,
        include_samples: include_sample_values(&org.settings),
    };
    let req = AskRequest {
        data_source_id: body.data_source_id,
        question: body.prompt,
        existing_sql: None,
        error: None,
    };
    Ok(prepare_ask(&state, db, &scope, org, user, req)
        .await?
        .stream(|ctx, preview, tx| monitor_ask_loop(ctx, m, provider_for, preview, tx)))
}
