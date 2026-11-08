//! `/api/organization` — org settings read/update. Admin only.

use crate::extract::Json;
use chrono::{DateTime, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use arcodash_core::entities::organization;
use arcodash_core::events::record_event;

use axum::extract::State;

use crate::auth::{CurrentOrg, CurrentUser};
use crate::db::Db;
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Serialize)]
pub struct OrgResponse {
    pub id: i64,
    pub name: String,
    pub slug: String,
    pub settings: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Strip every AI provider's encrypted key — the flat `settings.ai.gemini_api_key_encrypted`
/// and each `settings.ai.providers.<kind>.key_encrypted` (owned by `api::handlers::ai_settings`) —
/// before this generic response echoes `settings` back. Every other key, including the rest of
/// `settings.ai` (e.g. `rate_limit_per_min`, `provider_order`, per-provider `model`), is preserved.
fn strip_ai_secret(mut settings: Value) -> Value {
    if let Some(ai) = settings.get_mut("ai").and_then(Value::as_object_mut) {
        ai.remove("gemini_api_key_encrypted");
        if let Some(providers) = ai.get_mut("providers").and_then(Value::as_object_mut) {
            for provider in providers.values_mut().filter_map(Value::as_object_mut) {
                provider.remove("key_encrypted");
            }
        }
    }
    settings
}

fn to_resp(o: organization::Model) -> OrgResponse {
    OrgResponse {
        id: o.id,
        name: o.name,
        slug: o.slug,
        settings: strip_ai_secret(o.settings),
        created_at: o.created_at.into(),
        updated_at: o.updated_at.into(),
    }
}

pub async fn get(
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    super::require_admin(&db, &caller.0).await?;
    Ok(Json(to_resp(org)))
}

#[derive(Deserialize)]
pub struct UpdateOrgBody {
    pub name: Option<String>,
    pub settings: Option<Value>,
}

pub async fn update(
    State(state): State<AppState>,
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Json(body): Json<UpdateOrgBody>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    // "ai" is owned by the dedicated `/api/organization/ai` endpoints (encrypted provider keys,
    // rate limit — validation this generic endpoint doesn't apply). A deployment may reserve more
    // keys. Stripped before the write merges the rest in; merging (rather than replacing the
    // column) means an omitted key simply keeps its stored value instead of being deleted.
    const SERVER_OWNED_KEYS: [&str; 1] = ["ai"];

    super::require_admin(&db, &caller.0).await?;

    let name_changed = body.name.as_ref().is_some_and(|name| *name != org.name);

    let mut query = organization::Entity::update_many().filter(organization::Column::Id.eq(org.id));
    if let Some(name) = body.name {
        query = query.col_expr(organization::Column::Name, Expr::value(name));
    }
    let mut settings_keys: Vec<String> = Vec::new();
    if let Some(mut settings) = body.settings {
        // A non-object `settings` (e.g. `[]` or `"x"`) has no keys to strip/merge — reject it
        // instead of silently writing garbage into a jsonb column that expects an object.
        let Some(obj) = settings.as_object_mut() else {
            return Err(ApiError::bad_request("settings must be a JSON object"));
        };
        for key in SERVER_OWNED_KEYS
            .iter()
            .chain(state.reserved_org_settings_keys.iter())
        {
            obj.remove(*key);
        }
        settings_keys = obj.keys().cloned().collect();
        query = query.col_expr(
            organization::Column::Settings,
            Expr::cust_with_values("settings || $1::jsonb", [settings]),
        );
    }
    query = query.col_expr(organization::Column::UpdatedAt, Expr::value(Utc::now()));

    let updated = query
        .exec_with_returning(&db)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::not_found("organization not found"))?;

    // One `organization.updated` row per request, naming every top-level field actually touched
    // — "name" if it changed, plus each caller-supplied settings key. Key names only, never
    // values — settings can carry provider configuration, and this answers "what was touched"
    // without becoming a secret store.
    let changed_keys: Vec<String> = name_changed
        .then(|| "name".to_string())
        .into_iter()
        .chain(settings_keys)
        .collect();
    if !changed_keys.is_empty() {
        record_event(
            &db,
            org.id,
            Some(caller.0.id),
            "organization.updated",
            Some("organization"),
            Some(org.id),
            serde_json::json!({ "keys": changed_keys }),
        )
        .await?;
    }

    Ok(Json(to_resp(updated)))
}
