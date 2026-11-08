//! Metering write for AI-provider calls.

use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::{ActiveModelTrait, ConnectionTrait, DbErr};

use super::{pricing, AiProviderKind};
use crate::entities::ai_usage;

/// Insert one `ai_usage` row for a real provider call.
#[allow(clippy::too_many_arguments)]
pub async fn record_usage(
    db: &impl ConnectionTrait,
    org_id: i64,
    user_id: Option<i64>,
    kind: AiProviderKind,
    model: &str,
    feature: &str,
    status: &str,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
) -> Result<(), DbErr> {
    let cost_usd = pricing::cost_usd(kind, model, tokens_in, tokens_out);
    ai_usage::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        provider: Set(kind.as_str().to_string()),
        model: Set(model.to_string()),
        feature: Set(feature.to_string()),
        status: Set(status.to_string()),
        tokens_in: Set(tokens_in),
        tokens_out: Set(tokens_out),
        user_id: Set(user_id),
        cost_usd: Set(cost_usd),
        created_at: NotSet,
    }
    .insert(db)
    .await?;
    Ok(())
}
