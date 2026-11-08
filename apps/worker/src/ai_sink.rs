//! Worker-side AI plumbing: the provider runtime carried on the job context and the sink that
//! meters each provider attempt with the worker's plain (RLS-bypassing) connection.

use std::sync::Arc;

use arcodash_core::ai::fallback::{Attempt, AttemptSink};
use arcodash_core::ai::settings::record_provider_status;
use arcodash_core::ai::usage::record_usage;
use arcodash_core::ai::{AiError, AiProvider, AiProviderKind};
use arcodash_core::config::Config;
use arcodash_core::entities::organization;
use sea_orm::DatabaseConnection;

pub struct AiRuntime {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub provider_lookup: fn(AiProviderKind, Option<String>) -> Box<dyn AiProvider>,
}

/// Each attempt's `record_provider_status` returns the updated row, threaded through the mutex
/// so later attempts and the caller see the latest settings.
pub struct DbAttemptSink<'a> {
    db: &'a DatabaseConnection,
    org: tokio::sync::Mutex<organization::Model>,
    user_id: Option<i64>,
}

impl<'a> DbAttemptSink<'a> {
    pub fn new(db: &'a DatabaseConnection, org: organization::Model, user_id: Option<i64>) -> Self {
        Self {
            db,
            org: tokio::sync::Mutex::new(org),
            user_id,
        }
    }
}

#[async_trait::async_trait]
impl AttemptSink for DbAttemptSink<'_> {
    async fn check_budget(&self) -> Result<(), AiError> {
        let org_id = self.org.lock().await.id;
        arcodash_core::ai::budget::check_budget(self.db, org_id)
            .await
            .map(|_| ())
    }

    async fn record(&self, attempt: Attempt<'_>) {
        let mut org = self.org.lock().await;
        if let Ok(updated) = record_provider_status(
            self.db,
            &org,
            attempt.kind,
            attempt.status,
            attempt.free_tier,
        )
        .await
        {
            *org = updated;
        }
        let org_id = org.id;
        drop(org);
        let _ = record_usage(
            self.db,
            org_id,
            self.user_id,
            attempt.kind,
            attempt.model,
            attempt.feature,
            attempt.status,
            attempt.tokens_in,
            attempt.tokens_out,
        )
        .await;
    }
}
