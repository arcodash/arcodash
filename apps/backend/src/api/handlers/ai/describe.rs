//! Request-path trigger for LLM column descriptions: a cheap pre-check, then a
//! `DescribeDataSource` job. The provider call itself runs in the worker.

use arcodash_core::connectors::TableSchema;
use arcodash_core::entities::organization;
use arcodash_core::models::{ai_profile_describing_key, DataSourceAiProfile};
use arcodash_core::queue::{JobMessage, QueueName};

use crate::api::handlers::ai_settings::ResolvedProvider;
use crate::db::RequestScope;

pub use arcodash_core::ai::describe::*;

/// No-op unless the source has a usable profile without descriptions, a provider is configured, and
/// no describe run holds the per-source guard. Duplicate jobs are absorbed by the worker's guard.
#[allow(clippy::too_many_arguments)]
pub fn spawn_for_request(
    state: &crate::state::AppState,
    _scope: &RequestScope,
    org: &organization::Model,
    ds: &arcodash_core::entities::data_source::Model,
    profile: Option<&DataSourceAiProfile>,
    _tables: &[TableSchema],
    chain: &[ResolvedProvider],
    _include_samples: bool,
    _user_id: i64,
) {
    if chain.is_empty() || !profile.is_some_and(needs_descriptions) {
        return;
    }
    let state = state.clone();
    let (org_id, data_source_id) = (org.id, ds.id);
    tokio::spawn(async move {
        let mut redis = state.redis.clone();
        let held: bool = redis::cmd("EXISTS")
            .arg(ai_profile_describing_key(data_source_id))
            .query_async(&mut redis)
            .await
            .unwrap_or(true);
        if held {
            return;
        }
        let job = JobMessage::DescribeDataSource {
            org_id,
            data_source_id,
        };
        if let Err(e) = state.queue.enqueue(QueueName::Schemas, job).await {
            tracing::warn!(data_source_id, error = %e, "enqueueing describe failed");
        }
    });
}
