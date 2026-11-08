//! `POST /api/dashboards/{id}/ai_widget` — natural language to a dashboard tile.
//!
//! Drives the Ask loop, picks a visualization for the verified result, then creates the query,
//! visualization and draft widget in one transaction. Streams the Ask events plus `visualizing` and
//! `created`; any failure ends in a single `failed` with nothing created.
//!
//! Invariants: every check runs before the first provider call; only the loop's exact
//! `is_read_only_sql` result is saved; the widget lands in the draft, so published readers see
//! nothing until the dashboard is published.

use axum::extract::{Path, State};
use axum::response::sse::Event;
use axum::response::Response;
use std::time::{Duration, Instant};

use sea_orm::TransactionTrait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use arcodash_core::ai::viz::TableOptions;
use arcodash_core::entities::{organization, user};
use arcodash_core::queue::{self, JobStatus};

use crate::api::handlers::ai::viz::{ColumnMeta, VizSuggestion};
use crate::api::handlers::ai::{is_read_only_sql, provider_for, AiProvider, AiProviderKind};
use crate::api::handlers::ai_ask::{
    ask_loop_outcome, emit, prepare_ask, AskCtx, AskRequest, PreviewRunner, PREVIEW_POLL,
};
use crate::api::handlers::ai_query::GenerateSqlResponse;
use crate::api::handlers::ai_viz::{check_column_bounds, suggest_for_columns};
use crate::auth::{CurrentOrg, CurrentUser};
use crate::authz::authorize::{self, Action, InheritedVerb};
use crate::authz::permissions::{load_effective_groups, user_has_permission};
use crate::db::{Db, RequestScope};
use crate::error::ApiError;
use crate::extract::Json;
use crate::services::{dashboard_service, query_service, visualization_service, widget_service};
use crate::state::AppState;

const MAX_NAME_CHARS: usize = 80;
/// How long `created` waits for the new query's first run to land its result pointer.
pub const FIRST_RUN_CAP: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
pub struct AiWidgetRequest {
    pub data_source_id: i64,
    pub prompt: String,
}

/// What the widget needs beyond the Ask context.
pub struct WidgetCtx {
    pub state: AppState,
    pub first_run_cap: Duration,
    pub user: user::Model,
    pub data_source_id: i64,
    pub dashboard_id: i64,
    pub dashboard_name: String,
}

pub(crate) fn truncate_name(s: &str) -> String {
    s.chars().take(MAX_NAME_CHARS).collect()
}

#[allow(
    clippy::struct_field_names,
    reason = "ids of three different entities; the suffix carries the meaning"
)]
struct Created {
    query_id: i64,
    visualization_id: i64,
    widget_id: i64,
}

#[allow(clippy::too_many_arguments)]
async fn create_widget_rows(
    scope: &RequestScope,
    org: &organization::Model,
    w: &WidgetCtx,
    prompt: &str,
    sql: String,
    viz_name: String,
    viz_type: &str,
    viz_options: Value,
) -> Result<Created, ApiError> {
    let scoped = scope.begin(org).await?;
    // A real transaction when pooled and a savepoint inside an org-scoped per-request transaction.
    let txn = scoped.db().begin().await?;
    let q = query_service::create(
        &txn,
        org.id,
        &w.user,
        w.data_source_id,
        truncate_name(prompt),
        Some(format!(
            "Created from dashboard {} with AI",
            w.dashboard_name
        )),
        sql,
        None,
        Some(vec![widget_service::AI_WIDGET_TAG.to_string()]),
    )
    .await?;
    // Widgets only attach to published queries.
    let q = query_service::publish(&txn, &w.user, q.clone(), q.working_version).await?;
    let viz = visualization_service::create(
        &txn,
        org.id,
        &w.user,
        q.id,
        viz_type.to_string(),
        viz_name,
        None,
        viz_options,
    )
    .await?;
    let position = widget_service::next_position(&txn, w.dashboard_id).await?;
    let widget = widget_service::create(
        &txn,
        org.id,
        &w.user,
        w.dashboard_id,
        Some(viz.id),
        None,
        1,
        position,
    )
    .await?;
    txn.commit().await?;
    scoped.commit().await?;
    Ok(Created {
        query_id: q.id,
        visualization_id: viz.id,
        widget_id: widget.id,
    })
}

/// Runs the saved query once, bound to its published pointer, so the tile has data on first
/// render. Best effort: a failed or slow run leaves the widget in place showing the query's own
/// state.
async fn run_first_time(
    scope: &RequestScope,
    org: &organization::Model,
    w: &WidgetCtx,
    query_id: i64,
    tx: &mpsc::Sender<Event>,
) {
    let submit = async {
        let scoped = scope.begin(org).await?;
        let q = query_service::get_for_user(scoped.db(), org.id, &w.user, query_id).await?;
        let r = query_service::resolve_serving_content(&q);
        let outcome = query_service::execute(
            scoped.db(),
            w.state.queue.as_ref(),
            w.state.conveyor.as_deref(),
            &w.state.redis,
            org.id,
            &w.user,
            r.data_source_id,
            r.query,
            r.options,
            Value::Null,
            None,
            Some(q.id),
            None,
            r.repoint_published,
            w.state.config.max_query_rows,
        )
        .await?;
        scoped.commit().await?;
        Ok::<_, ApiError>(outcome)
    };
    let Ok(
        query_service::ExecuteOutcome::Enqueued { job_id }
        | query_service::ExecuteOutcome::Parked { job_id },
    ) = submit.await
    else {
        return;
    };
    let started = Instant::now();
    while !tx.is_closed() && started.elapsed() < w.first_run_cap {
        match queue::get_job_status(&w.state.redis, &job_id).await {
            Ok(Some(rec)) if matches!(rec.status, JobStatus::Pending | JobStatus::Started) => {}
            _ => return,
        }
        tokio::time::sleep(PREVIEW_POLL).await;
    }
}

pub(crate) async fn emit_failed(tx: &mpsc::Sender<Event>, message: String) {
    if let Ok(p) = serde_json::to_value(GenerateSqlResponse::GenerationFailed { message }) {
        let _ = emit(tx, "failed", p).await;
    }
}

pub async fn widget_loop<F, P>(
    ctx: AskCtx,
    w: WidgetCtx,
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
    let (fallback, prompt) = (ctx.fallback_enabled, ctx.question.clone());

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

    let cols: Vec<ColumnMeta> = outcome.columns.iter().map(ColumnMeta::from).collect();
    #[allow(
        clippy::cast_possible_truncation,
        reason = "preview row counts are small, and u64 to usize is lossless on 64-bit servers"
    )]
    let pick = if cols.is_empty() || outcome.row_count == 0 || check_column_bounds(&cols).is_err() {
        None
    } else {
        if emit(&tx, "visualizing", json!({})).await.is_none() {
            return;
        }
        suggest_for_columns(
            &scope,
            &org,
            Some(w.user.id),
            &http,
            &chain,
            fallback,
            &cols,
            outcome.row_count as usize,
            &provider_lookup,
        )
        .await
        .1
        .ok()
    };
    // No pick (nothing to chart, or the provider failed): fall back to a plain table.
    let (viz_name, viz_type, viz_options) = match pick {
        Some(p) => (p.title, p.viz_type, p.options),
        None => (
            truncate_name(&prompt),
            "TABLE",
            VizSuggestion::Table(TableOptions::default()).options_json(),
        ),
    };

    match create_widget_rows(
        &scope,
        &org,
        &w,
        &prompt,
        outcome.sql,
        viz_name,
        viz_type,
        viz_options,
    )
    .await
    {
        Ok(c) => {
            run_first_time(&scope, &org, &w, c.query_id, &tx).await;
            let _ = emit(
                &tx,
                "created",
                json!({
                    "query_id": c.query_id,
                    "visualization_id": c.visualization_id,
                    "widget_id": c.widget_id,
                    "viz_type": viz_type,
                    "row_count": outcome.row_count,
                }),
            )
            .await;
        }
        Err(e) => emit_failed(&tx, format!("Couldn't add the widget: {}", e.message)).await,
    }
}

pub async fn create_ai_widget(
    State(state): State<AppState>,
    db: Db,
    scope: RequestScope,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<AiWidgetRequest>,
) -> Result<Response, ApiError> {
    let dash = dashboard_service::get_by_id(&db, org.id, &user, id).await?;
    authorize::authorize(
        &db,
        &user,
        Action::Widget {
            verb: InheritedVerb::Edit,
            dashboard_id: dash.id,
        },
    )
    .await?;
    let groups = load_effective_groups(&db, &user).await?;
    if !user_has_permission(&user, &groups, "create_query")
        || !user_has_permission(&user, &groups, "edit_query")
    {
        return Err(ApiError::forbidden(
            "creating a widget with AI requires query create and edit permissions",
        ));
    }
    if payload.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt must not be empty"));
    }

    let w = WidgetCtx {
        state: state.clone(),
        first_run_cap: FIRST_RUN_CAP,
        user: user.clone(),
        data_source_id: payload.data_source_id,
        dashboard_id: dash.id,
        dashboard_name: dash.name,
    };
    let req = AskRequest {
        data_source_id: payload.data_source_id,
        question: payload.prompt,
        existing_sql: None,
        error: None,
    };
    Ok(prepare_ask(&state, db, &scope, org, user, req)
        .await?
        .stream(|ctx, preview, tx| widget_loop(ctx, w, provider_for, preview, tx)))
}

#[cfg(test)]
mod tests {
    use super::truncate_name;

    #[test]
    fn truncates_on_char_boundary() {
        let s = "é".repeat(90);
        assert_eq!(truncate_name(&s).chars().count(), 80);
    }
}
