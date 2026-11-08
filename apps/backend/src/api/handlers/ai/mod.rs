//! Provider abstraction for "Ask with AI" text-to-SQL generation and key testing.
//!
//! One trait, one shared error type, one shared success type — each provider (Gemini, `OpenAI`,
//! Anthropic) implements the HTTP mapping to its own API; callers (`ai_query.rs`, `ai_settings.rs`)
//! dispatch through the trait and translate `AiError` into their own wire-response variants, since
//! the same `AiError` means different user-facing text depending on which endpoint is calling.

pub mod context;
pub mod describe;

pub use arcodash_core::ai::*;

use arcodash_core::connectors::TableSchema;
use arcodash_core::entities::data_source;

use crate::error::ApiError;
use crate::services::data_source_service;
use crate::state::AppState;

/// Schema for an AI prompt: the cache when usable (no warehouse round trip), live otherwise.
/// Call after `scope.detach` with the `ds` read beforehand.
pub(crate) async fn schema_for_ai(
    state: &AppState,
    ds: &data_source::Model,
    user_id: i64,
) -> Result<Vec<TableSchema>, ApiError> {
    match cached_schema(ds) {
        Some(tables) => Ok(tables),
        None => live_schema_for_ai(state, ds, user_id).await,
    }
}

/// Schema straight from the warehouse, bypassing the cache — for callers whose cached copy proved
/// stale.
pub(crate) async fn live_schema_for_ai(
    state: &AppState,
    ds: &data_source::Model,
    user_id: i64,
) -> Result<Vec<TableSchema>, ApiError> {
    let encryption_key = super::encryption_key(state)?;
    if ds.r#type == "query_results" {
        return Ok(into_tables(
            data_source_service::live_schema_for_query_results(
                &state.connectors,
                &encryption_key,
                ds,
                user_id,
            )
            .await?,
        ));
    }

    let connector = state.connectors.get(&ds.r#type).ok_or_else(|| {
        ApiError::bad_request(format!("unknown data source type '{}'", ds.r#type))
    })?;
    let options = arcodash_core::connectors::query_results::inject_caller(
        data_source_service::decrypt_options(&encryption_key, ds.encrypted_options.as_deref())?,
        &ds.r#type,
        ds.org_id,
        user_id,
    );
    let message = match tokio::time::timeout(
        data_source_service::interactive_connector_timeout(),
        connector.get_schema(&options),
    )
    .await
    {
        Ok(Ok(tables)) => return Ok(tables),
        Ok(Err(e)) => format!("schema lookup failed: {e:#}"),
        Err(_) => "schema lookup failed: timed out".to_string(),
    };
    Err(ApiError::new(axum::http::StatusCode::BAD_GATEWAY, message))
}
