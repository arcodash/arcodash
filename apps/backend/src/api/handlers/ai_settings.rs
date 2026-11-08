//! `/api/organization/ai*`: provider settings for "Ask with AI" (text-to-SQL).
//!
//! Stored under the org `settings` JSONB's nested `ai` key: per-provider encrypted key + model, a
//! rate limit, and a `provider_order` (first is primary) with an optional fallback flag.
//!
//! Admin only. Legacy compat: a flat `gemini_api_key_encrypted` is read for Gemini only; the first
//! `update` migrates it into `providers.gemini.key_encrypted`.

use std::collections::HashMap;

use crate::extract::Json;
use axum::extract::State;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use arcodash_core::entities::organization;
use arcodash_core::events::record_event;
use arcodash_core::org_settings::merge_org_settings_at;
use arcodash_core::secrets;
use arcodash_core::Config;

use crate::api::handlers::ai::{provider_for, AiError, AiProviderKind, AI_BUSY_MESSAGE};
use crate::api::handlers::ai_query::{provider_name, rate_limit_message};
use crate::auth::{CurrentOrg, CurrentUser};
use crate::db::Db;
use crate::error::ApiError;
use crate::state::AppState;

use arcodash_core::ai::settings::{
    self as core_settings, ai_obj, env_key_for, env_model_for, provider_entry_mut,
    provider_has_key, provider_key_ciphertext, provider_model, provider_obj,
    resolve_provider_order, ALL_PROVIDER_KINDS,
};
pub use arcodash_core::ai::settings::{
    dashboard_narratives, explain_alerts, fallback_enabled, include_sample_values,
    provider_base_url, provider_free_tier, provider_status_str, ResolvedProvider,
};

/// Resolves the org's full fallback chain; the entry point `ai_query::generate_sql` calls.
pub fn resolve_provider_chain(
    org: &organization::Model,
    config: &Config,
) -> Result<Vec<ResolvedProvider>, ApiError> {
    core_settings::resolve_provider_chain(org, config)
        .map_err(|e| ApiError::internal(e.to_string()))
}

/// Persists the outcome of a real provider call; returns the updated org row so multi-call requests
/// can thread it.
pub async fn record_provider_status(
    db: &Db,
    org: &organization::Model,
    kind: AiProviderKind,
    status: &str,
    free_tier: bool,
) -> Result<organization::Model, ApiError> {
    Ok(core_settings::record_provider_status(db, org, kind, status, free_tier).await?)
}

#[derive(Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent feature flags mirroring the JSON wire format"
)]
pub struct AiSettingsResponse {
    pub key_configured: bool,
    pub key_source: String, // "org" | "env" | "none"
    pub rate_limit_org_value: Option<u32>,
    pub rate_limit_env_default: u32,
    pub rate_limit_effective: u32,
    pub monthly_budget_usd: Option<f64>,
    /// `[0]` is the active/primary provider ([`resolve_provider_order`]); `[]` if none configured.
    pub provider_order: Vec<String>,
    pub fallback_enabled: bool,
    pub include_sample_values: bool,
    pub explain_alerts: bool,
    pub dashboard_narratives: bool,
    /// Every known provider kind, configured or not, keyed by [`AiProviderKind::as_str`].
    pub providers: HashMap<String, ProviderInfo>,
}

/// Everything the frontend needs for one provider's card.
///
/// Key presence/source, model, curated model choices, and the last real call's outcome
/// (`last_status` is one of `ok`, `invalid_key`, `quota_exceeded`, `unavailable`, `error`; both are
/// `None` until the first call).
#[derive(Serialize)]
pub struct ProviderInfo {
    pub key_configured: bool,
    pub key_source: String, // "org" | "env" | "none"
    /// Currently configured model, or the provider's default if none is set.
    pub model: Option<String>,
    /// Org-configured endpoint override — always `None` for providers other than `OpenAI`.
    pub base_url: Option<String>,
    pub default_model: String,
    pub curated_models: Vec<String>,
    pub last_status: Option<String>,
    pub last_status_at: Option<String>,
    /// Last time a free-tier quota/rate limit was seen; cleared when the key changes.
    pub free_tier_seen_at: Option<String>,
}

/// Name of a JSON value's type, for logging what was found where an object was expected.
const fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "the update handler only ever stores a u32 here"
)]
fn org_rate_limit(settings: &Value) -> Option<u32> {
    ai_obj(settings)?
        .get("rate_limit_per_min")?
        .as_u64()
        .map(|v| v as u32)
}

/// Takes only the scalar env values it needs, not the whole `Config`, so it's easy to unit test.
fn build_response(
    settings: &Value,
    gemini_api_key_env: Option<&str>,
    openai_api_key_env: Option<&str>,
    anthropic_api_key_env: Option<&str>,
    rate_limit_env_default: u32,
) -> AiSettingsResponse {
    let order = resolve_provider_order(
        settings,
        gemini_api_key_env,
        openai_api_key_env,
        anthropic_api_key_env,
    );
    let rate_limit_org_value = org_rate_limit(settings);

    // `(key_configured, key_source)`: an org key takes precedence over the env fallback.
    let key_state = |kind| {
        if provider_key_ciphertext(settings, kind).is_some() {
            (true, "org")
        } else if env_key_for(
            kind,
            gemini_api_key_env,
            openai_api_key_env,
            anthropic_api_key_env,
        )
        .is_some()
        {
            (true, "env")
        } else {
            (false, "none")
        }
    };
    let (key_configured, key_source) = order.first().map_or((false, "none"), |&k| key_state(k));

    let providers = ALL_PROVIDER_KINDS
        .into_iter()
        .map(|k| {
            let obj = provider_obj(settings, k);
            let status_field = |field: &str| {
                obj.and_then(|p| p.get(field))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            let provider_impl = provider_for(k, None);
            let (key_configured, key_source) = key_state(k);
            let info = ProviderInfo {
                key_configured,
                key_source: key_source.to_string(),
                model: provider_model(settings, k)
                    .map(str::to_string)
                    .or_else(|| Some(provider_impl.default_model().to_string())),
                base_url: if k == AiProviderKind::OpenAi {
                    provider_base_url(settings, k)
                } else {
                    None
                },
                default_model: provider_impl.default_model().to_string(),
                curated_models: provider_impl
                    .curated_models()
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect(),
                last_status: status_field("last_status"),
                last_status_at: status_field("last_status_at"),
                free_tier_seen_at: status_field("free_tier_seen_at"),
            };
            (k.as_str().to_string(), info)
        })
        .collect();

    AiSettingsResponse {
        key_configured,
        key_source: key_source.to_string(),
        rate_limit_org_value,
        rate_limit_env_default,
        rate_limit_effective: rate_limit_org_value.unwrap_or(rate_limit_env_default),
        monthly_budget_usd: arcodash_core::ai::settings::monthly_budget_usd(settings),
        provider_order: order.iter().map(|k| k.as_str().to_string()).collect(),
        fallback_enabled: fallback_enabled(settings),
        include_sample_values: include_sample_values(settings),
        explain_alerts: explain_alerts(settings),
        dashboard_narratives: dashboard_narratives(settings),
        providers,
    }
}

/// The effective (org-override-or-fallback) config for an org's active AI provider, consumed by the
/// generation endpoint and `GET /api/session`'s `client_config`.
///
/// The single place that knows provider-order resolution and org-vs-env precedence.
pub struct EffectiveAiConfig {
    /// The active provider (`provider_order[0]`), or `None` if none is configured.
    pub provider: Option<AiProviderKind>,
    /// Decrypted org key, else env fallback; `None` if neither (or no provider).
    pub api_key: Option<String>,
    /// Org-configured model, else the provider's env default, else `None`.
    pub model: Option<String>,
    /// Org override, or `config.ai_rate_limit_per_min` fallback.
    pub rate_limit_per_min: u32,
}

/// Resolve `org`'s effective active provider, key, model, and rate limit.
pub fn effective_ai_config(
    org: &organization::Model,
    config: &Config,
) -> Result<EffectiveAiConfig, ApiError> {
    let settings = &org.settings;
    let gemini_api_key_env = config.gemini_api_key.as_deref();
    let openai_api_key_env = config.openai_api_key.as_deref();
    let anthropic_api_key_env = config.anthropic_api_key.as_deref();
    let provider = resolve_provider_order(
        settings,
        gemini_api_key_env,
        openai_api_key_env,
        anthropic_api_key_env,
    )
    .first()
    .copied();

    let (api_key, model) = match provider {
        Some(kind) => core_settings::resolve_key_and_model(
            settings,
            kind,
            gemini_api_key_env,
            openai_api_key_env,
            anthropic_api_key_env,
            &config.gemini_model,
            &config.openai_model,
            &config.anthropic_model,
            &config.encryption_key,
        )
        .map_err(|e| ApiError::internal(e.to_string()))?,
        None => (None, None),
    };

    Ok(EffectiveAiConfig {
        provider,
        api_key,
        model,
        rate_limit_per_min: org_rate_limit(settings).unwrap_or(config.ai_rate_limit_per_min),
    })
}

/// `GET /api/organization/ai`
pub async fn get(
    State(state): State<AppState>,
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    super::require_admin(&db, &caller.0).await?;
    Ok(Json(build_response(
        &org.settings,
        state.config.gemini_api_key.as_deref(),
        state.config.openai_api_key.as_deref(),
        state.config.anthropic_api_key.as_deref(),
        state.config.ai_rate_limit_per_min,
    )))
}

use super::deserialize_present;

#[derive(Deserialize, Default)]
pub struct ProviderUpdateBody {
    #[serde(default, deserialize_with = "deserialize_present")]
    pub api_key: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub model: Option<Option<String>>,
    /// Endpoint override (org-configurable), meaningful for OpenAI-compatible APIs today.
    #[serde(default, deserialize_with = "deserialize_present")]
    pub base_url: Option<Option<String>>,
}

#[derive(Deserialize)]
pub struct UpdateAiSettingsBody {
    /// Legacy flat body field, routed internally to `providers.gemini.api_key`.
    #[serde(default, deserialize_with = "deserialize_present")]
    pub gemini_api_key: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub rate_limit_per_min: Option<Option<u32>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub monthly_budget_usd: Option<Option<f64>>,
    #[serde(default)]
    pub providers: Option<HashMap<String, ProviderUpdateBody>>,
    #[serde(default)]
    pub provider_order: Option<Vec<String>>,
    #[serde(default)]
    pub fallback_enabled: Option<bool>,
    #[serde(default)]
    pub include_sample_values: Option<bool>,
    #[serde(default)]
    pub explain_alerts: Option<bool>,
    #[serde(default)]
    pub dashboard_narratives: Option<bool>,
}

/// Loose sanity check only (no whitespace, minimum length): key formats vary, so real validation is
/// the live `test_key` call.
fn valid_key_format(key: &str) -> bool {
    key.len() >= 20 && !key.contains(char::is_whitespace)
}

/// Moves a legacy flat `gemini_api_key_encrypted` into `providers.gemini.key_encrypted` (unless
/// already set) and drops the flat field. Runs at the top of every `update`.
fn migrate_legacy_gemini_key(ai: &mut serde_json::Map<String, Value>) {
    let Some(Value::String(legacy)) = ai.remove("gemini_api_key_encrypted") else {
        return;
    };
    let providers = ai
        .entry("providers".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::default()));
    let Some(providers_map) = providers.as_object_mut() else {
        return;
    };
    let gemini = providers_map
        .entry(AiProviderKind::Gemini.as_str().to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::default()));
    if let Some(gemini_map) = gemini.as_object_mut() {
        gemini_map
            .entry("key_encrypted".to_string())
            .or_insert(Value::String(legacy));
    }
}

/// Rejects a `base_url` an org admin could use to point outbound AI calls at an internal address
/// (SSRF). Cheap syntactic checks come first; only a sound URL reaches the DNS-resolving
/// `guard_url`.
async fn validate_base_url(url: &str) -> Result<(), ApiError> {
    validate_base_url_with(url, arcodash_core::net_guard::system_resolve).await
}

/// [`validate_base_url`] with an injectable DNS resolver.
async fn validate_base_url_with<R>(url: &str, resolve: R) -> Result<(), ApiError>
where
    R: FnOnce(&str) -> std::io::Result<Vec<std::net::SocketAddr>> + Send + 'static,
{
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| ApiError::bad_request(format!("invalid base_url: {e}")))?;

    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(ApiError::bad_request("base_url must use http or https"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ApiError::bad_request(
            "base_url must not contain embedded credentials",
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ApiError::bad_request(
            "base_url must not contain a query string or fragment",
        ));
    }

    arcodash_core::connectors::http_connectors::guard_url_with(url, resolve)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(())
}

/// Applies one provider's key/model change to the `ai` settings map, encrypting a newly-set key
/// with `encryption_key_hex`.
fn apply_provider_update(
    ai: &mut serde_json::Map<String, Value>,
    kind: AiProviderKind,
    upd: ProviderUpdateBody,
    encryption_key_hex: &str,
) -> Result<(), ApiError> {
    let provider_map = provider_entry_mut(ai, kind);

    if let Some(key_change) = upd.api_key {
        match key_change {
            Some(key) => {
                if !valid_key_format(&key) {
                    return Err(ApiError::bad_request(
                        "That doesn't look like a complete API key — check for missing characters or extra spaces.",
                    ));
                }
                let encryption_key = secrets::parse_key_hex(encryption_key_hex)
                    .map_err(|e| ApiError::internal(e.to_string()))?;
                let ciphertext = secrets::encrypt(&encryption_key, key.as_bytes())
                    .map_err(|e| ApiError::internal(e.to_string()))?;
                provider_map.insert(
                    "key_encrypted".to_string(),
                    Value::String(BASE64.encode(ciphertext)),
                );
            }
            None => {
                provider_map.remove("key_encrypted");
            }
        }
        // A new or removed key invalidates the old key's free-tier observation.
        provider_map.remove("free_tier_seen_at");
    }

    for (field, change) in [("model", upd.model), ("base_url", upd.base_url)] {
        match change {
            Some(Some(v)) => {
                provider_map.insert(field.to_string(), Value::String(v));
            }
            Some(None) => {
                provider_map.remove(field);
            }
            None => {}
        }
    }

    Ok(())
}

/// Validates and stores a new `provider_order`: every entry must be a known [`AiProviderKind`] with
/// a configured key (including keys set earlier in the same request). Rejects the whole request on
/// the first invalid entry; a write is an explicit admin action.
fn validate_and_set_provider_order(
    ai: &mut serde_json::Map<String, Value>,
    order: &[String],
    gemini_api_key_env: Option<&str>,
    openai_api_key_env: Option<&str>,
    anthropic_api_key_env: Option<&str>,
) -> Result<(), ApiError> {
    let mut probe = serde_json::Map::new();
    probe.insert("ai".to_string(), Value::Object(ai.clone()));
    let probe = Value::Object(probe);

    for entry in order {
        let kind: AiProviderKind = entry
            .parse()
            .map_err(|()| ApiError::bad_request(format!("unknown AI provider '{entry}'")))?;
        if !provider_has_key(
            &probe,
            kind,
            gemini_api_key_env,
            openai_api_key_env,
            anthropic_api_key_env,
        ) {
            return Err(ApiError::bad_request(format!(
                "'{entry}' has no configured key — add one before adding it to provider_order"
            )));
        }
    }

    ai.insert(
        "provider_order".to_string(),
        Value::Array(order.iter().cloned().map(Value::String).collect()),
    );
    Ok(())
}

/// Wraps a bare `ai` map into a `settings`-shaped `Value` so the `provider_*` read helpers can be
/// reused for diffing.
fn wrap_ai(ai: &serde_json::Map<String, Value>) -> Value {
    serde_json::json!({ "ai": Value::Object(ai.clone()) })
}

/// One `(action, details)` pair per AI-settings field that changed between two `ai` snapshots;
/// diffing means a request naming an unchanged field emits nothing. `details` never carries key
/// material.
fn collect_ai_settings_audit_events(
    before: &serde_json::Map<String, Value>,
    after: &serde_json::Map<String, Value>,
) -> Vec<(&'static str, Value)> {
    let before_v = wrap_ai(before);
    let after_v = wrap_ai(after);
    let mut events = Vec::new();

    for kind in ALL_PROVIDER_KINDS {
        let before_key = provider_key_ciphertext(&before_v, kind);
        let after_key = provider_key_ciphertext(&after_v, kind);
        if before_key != after_key {
            let action = if after_key.is_some() {
                "ai_settings.provider_key_set"
            } else {
                "ai_settings.provider_key_cleared"
            };
            events.push((action, serde_json::json!({ "provider": kind.as_str() })));
        }

        let before_model = provider_model(&before_v, kind);
        let after_model = provider_model(&after_v, kind);
        if before_model != after_model {
            events.push((
                "ai_settings.model_changed",
                serde_json::json!({ "provider": kind.as_str(), "model": after_model }),
            ));
        }
    }

    // `(settings key, action, details field)` for each top-level scalar setting.
    for (key, action, field) in [
        (
            "provider_order",
            "ai_settings.provider_order_changed",
            "order",
        ),
        (
            "fallback_enabled",
            "ai_settings.fallback_toggled",
            "enabled",
        ),
        (
            "monthly_budget_usd",
            "ai_settings.monthly_budget_changed",
            "monthly_budget_usd",
        ),
        (
            "include_sample_values",
            "ai_settings.sample_values_toggled",
            "enabled",
        ),
        (
            "explain_alerts",
            "ai_settings.explain_alerts_toggled",
            "enabled",
        ),
        (
            "dashboard_narratives",
            "ai_settings.dashboard_narratives_toggled",
            "enabled",
        ),
    ] {
        if before.get(key) != after.get(key) {
            let value = after.get(key).cloned().unwrap_or(Value::Null);
            events.push((
                action,
                Value::Object(std::iter::once((field.to_string(), value)).collect()),
            ));
        }
    }

    events
}

/// The `merge_org_settings_at(&["ai"], ..)` patch for an `update`: only the top-level `ai` keys
/// this request changed, so a concurrent writer to another key (e.g. `record_provider_status`)
/// isn't clobbered. A removed key is an explicit `null`, since the `||` merge can only add or
/// overwrite; readers treat `null` like absent.
fn scoped_ai_patch(
    before: &serde_json::Map<String, Value>,
    after: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    after
        .iter()
        .filter(|(k, v)| before.get(k.as_str()) != Some(*v))
        .map(|(k, v)| (k.clone(), v.clone()))
        .chain(
            before
                .keys()
                .filter(|k| !after.contains_key(k.as_str()))
                .map(|k| (k.clone(), Value::Null)),
        )
        .collect()
}

/// `PUT /api/organization/ai` (routed as POST alongside `organization::update`).
pub async fn update(
    State(state): State<AppState>,
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Json(body): Json<UpdateAiSettingsBody>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    super::require_admin(&db, &caller.0).await?;

    // Shallow-merge into `settings`: only the nested "ai" object is touched.
    let mut settings = org.settings.clone();
    if !settings.is_object() {
        tracing::warn!(
            org_id = org.id,
            json_type = json_type_name(&settings),
            "organization.settings was not a JSON object; replacing with an empty object"
        );
        settings = serde_json::json!({});
    }
    if let Some(ai_value) = settings.get("ai").filter(|v| !v.is_object()) {
        tracing::warn!(
            org_id = org.id,
            json_type = json_type_name(ai_value),
            "organization.settings.ai was not a JSON object; replacing with an empty object"
        );
    }
    let mut ai = ai_obj(&settings).cloned().unwrap_or_default();

    migrate_legacy_gemini_key(&mut ai);
    // Snapshot after legacy migration and before this request's mutations, so the audit diff
    // reflects only this call.
    let ai_before = ai.clone();

    let mut providers_updates = body.providers.unwrap_or_default();
    if let Some(key_change) = body.gemini_api_key {
        providers_updates
            .entry(AiProviderKind::Gemini.as_str().to_string())
            .or_default()
            .api_key = Some(key_change);
    }
    for (kind_str, upd) in providers_updates {
        let kind: AiProviderKind = kind_str
            .parse()
            .map_err(|()| ApiError::bad_request(format!("unknown AI provider '{kind_str}'")))?;
        if let Some(Some(url)) = upd.base_url.as_ref() {
            validate_base_url(url).await?;
        }
        apply_provider_update(&mut ai, kind, upd, &state.config.encryption_key)?;
    }

    if let Some(rate_change) = body.rate_limit_per_min {
        match rate_change {
            Some(n) => {
                if n < 1 {
                    return Err(ApiError::bad_request(
                        "rate_limit_per_min must be at least 1",
                    ));
                }
                ai.insert("rate_limit_per_min".to_string(), Value::from(n));
            }
            None => {
                ai.remove("rate_limit_per_min");
            }
        }
    }

    if let Some(budget_change) = body.monthly_budget_usd {
        match budget_change {
            Some(n) => {
                if !n.is_finite() || n <= 0.0 {
                    return Err(ApiError::bad_request(
                        "monthly_budget_usd must be greater than 0",
                    ));
                }
                ai.insert("monthly_budget_usd".to_string(), Value::from(n));
            }
            None => {
                ai.remove("monthly_budget_usd");
            }
        }
    }

    if let Some(order) = body.provider_order {
        validate_and_set_provider_order(
            &mut ai,
            &order,
            state.config.gemini_api_key.as_deref(),
            state.config.openai_api_key.as_deref(),
            state.config.anthropic_api_key.as_deref(),
        )?;
    }

    for (key, toggle) in [
        ("fallback_enabled", body.fallback_enabled),
        ("include_sample_values", body.include_sample_values),
        ("explain_alerts", body.explain_alerts),
        ("dashboard_narratives", body.dashboard_narratives),
    ] {
        if let Some(v) = toggle {
            ai.insert(key.to_string(), Value::Bool(v));
        }
    }

    let audit_events = collect_ai_settings_audit_events(&ai_before, &ai);
    let changed_ai = scoped_ai_patch(&ai_before, &ai);

    settings
        .as_object_mut()
        .expect("just normalized to an object above")
        .insert("ai".to_string(), Value::Object(ai));

    let response = build_response(
        &settings,
        state.config.gemini_api_key.as_deref(),
        state.config.openai_api_key.as_deref(),
        state.config.anthropic_api_key.as_deref(),
        state.config.ai_rate_limit_per_min,
    );
    merge_org_settings_at(&db, org.id, &["ai"], Value::Object(changed_ai)).await?;

    for (action, details) in audit_events {
        record_event(
            &db,
            org.id,
            Some(caller.0.id),
            action,
            Some("organization"),
            Some(org.id),
            details,
        )
        .await?;
    }

    Ok(Json(response))
}

#[derive(Deserialize)]
pub struct TestKeyBody {
    /// Which provider to test — defaults to the active provider (`provider_order[0]`) if absent.
    pub provider: Option<String>,
    /// A not-yet-saved key to test in place of the stored/env key for `provider`.
    pub api_key: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status")]
pub enum TestKeyResult {
    #[serde(rename = "ok")]
    Ok { message: String },
    #[serde(rename = "invalid_key")]
    InvalidKey { message: String },
    #[serde(rename = "quota_exceeded")]
    QuotaExceeded { message: String },
    #[serde(rename = "network_error")]
    NetworkError { message: String },
}

/// Maps an [`AiError`] to the tagged `test_key` response with per-provider wording; separate so
/// it's unit-testable.
fn map_test_key_error(err: &AiError, kind: AiProviderKind) -> TestKeyResult {
    let provider = provider_name(kind);
    let network = |message: String| TestKeyResult::NetworkError { message };
    match err {
        AiError::BudgetExceeded { .. } => {
            network("Couldn't test the key. Try again shortly.".to_string())
        }
        AiError::Unavailable(_) => network(AI_BUSY_MESSAGE.to_string()),
        AiError::RateLimited { .. } => TestKeyResult::QuotaExceeded {
            message: format!("Key is valid. {}", rate_limit_message(kind)),
        },
        AiError::InvalidKey => TestKeyResult::InvalidKey {
            message: match kind {
                AiProviderKind::Gemini => "Gemini rejected this key. It may be revoked or mistyped — generate a new one in Google AI Studio.",
                AiProviderKind::OpenAi => "OpenAI rejected this key. It may be revoked or mistyped — generate a new one at platform.openai.com.",
                AiProviderKind::Anthropic => "Anthropic rejected this key or request. This can mean the key is invalid, or your Anthropic account has no available credit/billing configured — check both at console.anthropic.com.",
            }
            .to_string(),
        },
        AiError::QuotaExceeded { .. } => {
            let plan = match kind {
                AiProviderKind::Gemini => "Google AI",
                _ => provider,
            };
            TestKeyResult::QuotaExceeded {
                message: format!(
                    "Key is valid but its {provider} quota is exhausted. Generation will fail until \
                     quota resets — check your {plan} plan/billing."
                ),
            }
        }
        AiError::GenerationFailed(_) => network(format!(
            "{provider} returned an unexpected response. Try again shortly."
        )),
        AiError::Network(_) => network(format!(
            "Couldn't reach {provider} (network error/timeout). Check connectivity and try again."
        )),
    }
}

/// `POST /api/organization/ai/test`: confirms a provider's key is accepted without spending
/// generation tokens.
///
/// `body.provider` defaults to the active one; `body.api_key` optionally overrides the stored/env
/// key.
pub async fn test_key(
    State(state): State<AppState>,
    db: Db,
    CurrentOrg(org): CurrentOrg,
    caller: CurrentUser,
    Json(body): Json<TestKeyBody>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    super::require_admin(&db, &caller.0).await?;

    let kind = if let Some(p) = body.provider {
        p.parse::<AiProviderKind>()
            .map_err(|()| ApiError::bad_request(format!("unknown AI provider '{p}'")))?
    } else {
        let effective = effective_ai_config(&org, &state.config)?;
        effective.provider.ok_or_else(|| {
            ApiError::bad_request("No AI provider is configured. Add a key in AI Settings.")
        })?
    };

    let key = if let Some(key) = body.api_key {
        key
    } else {
        let env_key = env_key_for(
            kind,
            state.config.gemini_api_key.as_deref(),
            state.config.openai_api_key.as_deref(),
            state.config.anthropic_api_key.as_deref(),
        );
        core_settings::effective_provider_key(
            &org.settings,
            kind,
            env_key.as_deref(),
            &state.config.encryption_key,
        )
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "No {} API key is configured. Set one in AI Settings.",
                kind.as_str()
            ))
        })?
    };

    let base_url = provider_base_url(&org.settings, kind);
    let provider = provider_for(kind, base_url);
    // Same resolution order as `effective_ai_config`: org model, else env default, else the
    // provider default; the probe must use the model generation would use.
    let model = provider_model(&org.settings, kind)
        .map(str::to_string)
        .or_else(|| {
            env_model_for(
                kind,
                &state.config.gemini_model,
                &state.config.openai_model,
                &state.config.anthropic_model,
            )
        })
        .unwrap_or_else(|| provider.default_model().to_string());
    let call_result = provider.test_key(&state.http, &key, &model).await;
    crate::metrics::record_ai_provider_call(
        kind.as_str(),
        "test_key",
        provider_status_str(&call_result),
    );
    // Log and continue: persisting provider health must not mask the key-validity result.
    if let Err(e) = record_provider_status(
        &db,
        &org,
        kind,
        provider_status_str(&call_result),
        provider_free_tier(&call_result),
    )
    .await
    {
        tracing::warn!(error = ?e, "failed to record provider status");
    }
    let result = match call_result {
        Ok(()) => TestKeyResult::Ok {
            message: "Key is valid and working.".to_string(),
        },
        Err(err) => map_test_key_error(&err, kind),
    };

    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_ENCRYPTION_KEY: &str =
        "00000000000000000000000000000000000000000000000000000000000000aa";

    #[test]
    fn update_body_distinguishes_absent_null_and_present() {
        let absent: UpdateAiSettingsBody = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.gemini_api_key, None);
        assert_eq!(absent.rate_limit_per_min, None);

        let cleared: UpdateAiSettingsBody =
            serde_json::from_str(r#"{"gemini_api_key": null, "rate_limit_per_min": null}"#)
                .unwrap();
        assert_eq!(cleared.gemini_api_key, Some(None));
        assert_eq!(cleared.rate_limit_per_min, Some(None));

        let set: UpdateAiSettingsBody = serde_json::from_str(
            r#"{"gemini_api_key": "AIzaSomeKeyValue1234567890", "rate_limit_per_min": 5}"#,
        )
        .unwrap();
        assert_eq!(
            set.gemini_api_key,
            Some(Some("AIzaSomeKeyValue1234567890".to_string()))
        );
        assert_eq!(set.rate_limit_per_min, Some(Some(5)));
    }

    #[test]
    fn key_format_validation() {
        // Real key prefixes vary (e.g. "AIza...", "sk-...") — only a loose sanity check applies.
        assert!(valid_key_format("AIzaSomeKeyValue1234567890abcdef"));
        assert!(valid_key_format("sk-someOtherKeyFormat1234567890"));
        assert!(!valid_key_format("not-a-key"));
        assert!(!valid_key_format("too-short-19c"));
        assert!(!valid_key_format("has a space in it 1234567890"));
    }

    #[test]
    fn response_reflects_org_env_and_none_key_sources() {
        let none_settings = serde_json::json!({});
        let resp = build_response(&none_settings, None, None, None, 10);
        assert_eq!(resp.key_source, "none");
        assert!(!resp.key_configured);
        assert_eq!(resp.rate_limit_effective, 10);
        assert_eq!(resp.provider_order, Vec::<String>::new());
        assert!(!resp.fallback_enabled);

        let resp = build_response(&none_settings, Some("env-key"), None, None, 10);
        assert_eq!(resp.key_source, "env");
        assert!(resp.key_configured);
        assert_eq!(resp.provider_order, vec!["gemini".to_string()]);

        // Org configured (priority over env) with its own rate limit, via the legacy flat shape.
        let org_settings = serde_json::json!({
            "ai": { "gemini_api_key_encrypted": "ciphertext", "rate_limit_per_min": 25 }
        });
        let resp = build_response(&org_settings, Some("env-key"), None, None, 10);
        assert_eq!(resp.key_source, "org");
        assert!(resp.key_configured);
        assert_eq!(resp.rate_limit_org_value, Some(25));
        assert_eq!(resp.rate_limit_effective, 25);
    }

    /// Legacy flat `gemini_api_key_encrypted` must still resolve as the active org key (the
    /// read-compat shim).
    #[test]
    fn legacy_flat_gemini_key_resolves_via_provider_accessor() {
        let settings = serde_json::json!({
            "ai": { "gemini_api_key_encrypted": "legacy-ciphertext" }
        });
        assert_eq!(
            provider_key_ciphertext(&settings, AiProviderKind::Gemini),
            Some("legacy-ciphertext")
        );
        assert!(provider_has_key(
            &settings,
            AiProviderKind::Gemini,
            None,
            None,
            None
        ));
        let resp = build_response(&settings, None, None, None, 10);
        assert_eq!(resp.key_source, "org");
        assert_eq!(resp.provider_order, vec!["gemini".to_string()]);
    }

    /// With no stored `provider_order`, exactly one configured provider becomes the implicit sole
    /// entry.
    #[test]
    fn provider_order_implicit_single_when_absent() {
        let settings = serde_json::json!({
            "ai": { "providers": { "gemini": { "key_encrypted": "ct" } } }
        });
        assert_eq!(
            resolve_provider_order(&settings, None, None, None),
            vec![AiProviderKind::Gemini]
        );

        assert_eq!(
            resolve_provider_order(&serde_json::json!({}), None, None, None),
            vec![]
        );
    }

    /// A stored `provider_order` naming a provider with no configured key is filtered out on read.
    #[test]
    fn provider_order_filters_keyless_and_unknown_entries_on_read() {
        let settings = serde_json::json!({
            "ai": {
                "provider_order": ["openai", "bogus", "gemini"],
                "providers": { "gemini": { "key_encrypted": "ct" } }
            }
        });
        assert_eq!(
            resolve_provider_order(&settings, None, None, None),
            vec![AiProviderKind::Gemini]
        );
    }

    /// OpenAI's env fallback key/model resolve the same way Gemini's do.
    #[test]
    fn openai_env_fallback_resolves() {
        assert_eq!(
            env_key_for(AiProviderKind::OpenAi, None, Some("sk-env-key"), None),
            Some("sk-env-key".to_string())
        );
        assert_eq!(env_key_for(AiProviderKind::OpenAi, None, None, None), None);
        assert_eq!(
            env_model_for(
                AiProviderKind::OpenAi,
                "gemini-model",
                "gpt-5.6-terra",
                "claude-model"
            ),
            Some("gpt-5.6-terra".to_string())
        );
    }

    /// Anthropic's env fallback key/model resolve the same way.
    #[test]
    fn anthropic_env_fallback_resolves() {
        assert_eq!(
            env_key_for(
                AiProviderKind::Anthropic,
                None,
                None,
                Some("sk-ant-env-key")
            ),
            Some("sk-ant-env-key".to_string())
        );
        assert_eq!(
            env_key_for(AiProviderKind::Anthropic, None, None, None),
            None
        );
        assert_eq!(
            env_model_for(
                AiProviderKind::Anthropic,
                "gemini-model",
                "gpt-5.6-terra",
                "claude-sonnet-5"
            ),
            Some("claude-sonnet-5".to_string())
        );
    }

    /// An org with only an OpenAI env-fallback key gets it as the implicit active provider.
    #[test]
    fn provider_order_implicit_single_openai_via_env() {
        let settings = serde_json::json!({});
        assert_eq!(
            resolve_provider_order(&settings, None, Some("sk-env-key"), None),
            vec![AiProviderKind::OpenAi]
        );
    }

    /// `providers.openai.base_url` round-trips through `apply_provider_update` and
    /// `provider_base_url`: absent by default, settable, clearable.
    #[test]
    fn base_url_round_trips() {
        let mut ai = serde_json::Map::new();
        apply_provider_update(
            &mut ai,
            AiProviderKind::OpenAi,
            ProviderUpdateBody {
                api_key: None,
                model: None,
                base_url: Some(Some("https://api.mistral.ai".to_string())),
            },
            TEST_ENCRYPTION_KEY,
        )
        .unwrap();
        let settings = serde_json::json!({ "ai": Value::Object(ai.clone()) });
        assert_eq!(
            provider_base_url(&settings, AiProviderKind::OpenAi),
            Some("https://api.mistral.ai".to_string())
        );

        apply_provider_update(
            &mut ai,
            AiProviderKind::OpenAi,
            ProviderUpdateBody {
                api_key: None,
                model: None,
                base_url: Some(None),
            },
            TEST_ENCRYPTION_KEY,
        )
        .unwrap();
        let settings = serde_json::json!({ "ai": Value::Object(ai) });
        assert_eq!(provider_base_url(&settings, AiProviderKind::OpenAi), None);
    }

    /// Depends on the process-global `ARCODASH_ALLOW_PRIVATE_CONNECTORS`, so holds the env lock.
    #[tokio::test]
    async fn validate_base_url_rejects_ssrf_and_malformed_values() {
        let _env = arcodash_core::test_support::private_net_env(None);

        for rejected in [
            "ftp://x",
            "not-a-url",
            "https://u:p@host",
            "https://host/v1?k=v",
            "https://host/#frag",
            "http://169.254.169.254",
            "http://127.0.0.1",
        ] {
            assert!(
                validate_base_url(rejected).await.is_err(),
                "expected {rejected} to be rejected"
            );
        }

        // Hostnames go through an injected resolver: no real DNS, same verdict logic.
        fn resolving(
            ips: &'static [&'static str],
        ) -> impl FnOnce(&str) -> std::io::Result<Vec<std::net::SocketAddr>> + Send + 'static
        {
            move |_| {
                Ok(ips
                    .iter()
                    .map(|ip| std::net::SocketAddr::new(ip.parse().unwrap(), 0))
                    .collect())
            }
        }
        assert!(
            validate_base_url_with("https://api.mistral.ai", resolving(&["1.1.1.1"]))
                .await
                .is_ok()
        );
        for internal in [
            &["169.254.169.254"][..],
            &["127.0.0.1"],
            &["10.0.0.5"],
            &["::1"],
            &["fd00::1"],
            &["1.1.1.1", "10.0.0.5"],
        ] {
            assert!(
                validate_base_url_with("https://api.mistral.ai", resolving(internal))
                    .await
                    .is_err(),
                "hostname resolving to {internal:?} must be rejected"
            );
        }
        // A resolver failure fails closed.
        assert!(
            validate_base_url_with("https://api.mistral.ai", |_| Err(std::io::Error::other(
                "NXDOMAIN"
            )))
            .await
            .is_err()
        );
        // An empty answer fails closed too.
        assert!(
            validate_base_url_with("https://api.mistral.ai", resolving(&[]))
                .await
                .is_err()
        );
        // Public IP literal still accepted without any resolver involvement.
        assert!(validate_base_url("https://1.1.1.1").await.is_ok());
    }

    /// The nested `providers.gemini` shape round-trips through `apply_provider_update` and the read
    /// helpers.
    #[test]
    fn new_provider_shape_round_trips() {
        let mut ai = serde_json::Map::new();
        apply_provider_update(
            &mut ai,
            AiProviderKind::Gemini,
            ProviderUpdateBody {
                api_key: Some(Some("AIzaSomeKeyValue1234567890".to_string())),
                model: Some(Some("gemini-3.1-flash-lite".to_string())),
                base_url: None,
            },
            TEST_ENCRYPTION_KEY,
        )
        .unwrap();

        let settings = serde_json::json!({ "ai": Value::Object(ai) });
        assert!(provider_key_ciphertext(&settings, AiProviderKind::Gemini).is_some());
        assert_eq!(
            provider_model(&settings, AiProviderKind::Gemini),
            Some("gemini-3.1-flash-lite")
        );
        let resp = build_response(&settings, None, None, None, 10);
        assert_eq!(resp.key_source, "org");
        assert!(resp.key_configured);
    }

    /// A legacy flat POST body (`gemini_api_key`) must persist as `providers.gemini.key_encrypted`,
    /// verified on the raw JSON (the old accessor would pass even if migration never ran).
    #[test]
    fn legacy_post_body_writes_through_to_new_shape() {
        let mut ai = serde_json::Map::new();
        ai.insert(
            "gemini_api_key_encrypted".to_string(),
            Value::String("legacy-ciphertext".to_string()),
        );
        migrate_legacy_gemini_key(&mut ai);

        assert!(!ai.contains_key("gemini_api_key_encrypted"));
        let stored = ai
            .get("providers")
            .and_then(Value::as_object)
            .and_then(|p| p.get("gemini"))
            .and_then(Value::as_object)
            .and_then(|g| g.get("key_encrypted"))
            .and_then(Value::as_str);
        assert_eq!(stored, Some("legacy-ciphertext"));
    }

    /// Shallow-merge preserves unrelated `settings` keys and other providers when one provider is
    /// updated.
    #[test]
    fn shallow_merge_preserves_other_keys_and_providers() {
        let mut ai = serde_json::Map::new();
        ai.insert(
            "providers".to_string(),
            serde_json::json!({
                "anthropic": { "key_encrypted": "anthropic-ct", "model": "claude-x" }
            }),
        );
        ai.insert("rate_limit_per_min".to_string(), Value::from(42));

        apply_provider_update(
            &mut ai,
            AiProviderKind::Gemini,
            ProviderUpdateBody {
                api_key: Some(Some("AIzaSomeKeyValue1234567890".to_string())),
                model: None,
                base_url: None,
            },
            TEST_ENCRYPTION_KEY,
        )
        .unwrap();

        assert_eq!(
            ai.get("providers")
                .and_then(Value::as_object)
                .and_then(|p| p.get("anthropic"))
                .and_then(Value::as_object)
                .and_then(|a| a.get("key_encrypted"))
                .and_then(Value::as_str),
            Some("anthropic-ct")
        );
        assert_eq!(ai.get("rate_limit_per_min"), Some(&Value::from(42)));
        assert!(ai
            .get("providers")
            .and_then(Value::as_object)
            .and_then(|p| p.get("gemini"))
            .is_some());
    }

    /// Locks the `AiError` → `TestKeyResult` wire contract: tag per variant and the exact message
    /// text.
    #[test]
    fn map_test_key_error_wire_contract_gemini() {
        match map_test_key_error(&AiError::InvalidKey, AiProviderKind::Gemini) {
            TestKeyResult::InvalidKey { message } => assert_eq!(
                message,
                "Gemini rejected this key. It may be revoked or mistyped — generate a new one in Google AI Studio."
            ),
            other => panic!("expected InvalidKey, got {other:?}"),
        }

        match map_test_key_error(&AiError::QuotaExceeded { free_tier: false }, AiProviderKind::Gemini) {
            TestKeyResult::QuotaExceeded { message } => assert_eq!(
                message,
                "Key is valid but its Gemini quota is exhausted. Generation will fail until quota resets — check your Google AI plan/billing."
            ),
            other => panic!("expected QuotaExceeded, got {other:?}"),
        }

        match map_test_key_error(
            &AiError::GenerationFailed("bad json".to_string()),
            AiProviderKind::Gemini,
        ) {
            TestKeyResult::NetworkError { message } => assert_eq!(
                message,
                "Gemini returned an unexpected response. Try again shortly."
            ),
            other => panic!("expected NetworkError, got {other:?}"),
        }

        match map_test_key_error(
            &AiError::Network("timeout".to_string()),
            AiProviderKind::Gemini,
        ) {
            TestKeyResult::NetworkError { message } => assert_eq!(
                message,
                "Couldn't reach Gemini (network error/timeout). Check connectivity and try again."
            ),
            other => panic!("expected NetworkError, got {other:?}"),
        }

        match map_test_key_error(
            &AiError::RateLimited {
                retry_after_secs: 30,
                free_tier: false,
            },
            AiProviderKind::Gemini,
        ) {
            TestKeyResult::QuotaExceeded { message } => assert_eq!(
                message,
                "Key is valid. Gemini is rate-limiting the configured key right now."
            ),
            other => panic!("expected QuotaExceeded, got {other:?}"),
        }
    }

    #[test]
    fn map_test_key_error_wire_contract_openai() {
        match map_test_key_error(&AiError::InvalidKey, AiProviderKind::OpenAi) {
            TestKeyResult::InvalidKey { message } => assert_eq!(
                message,
                "OpenAI rejected this key. It may be revoked or mistyped — generate a new one at platform.openai.com."
            ),
            other => panic!("expected InvalidKey, got {other:?}"),
        }

        match map_test_key_error(&AiError::QuotaExceeded { free_tier: false }, AiProviderKind::OpenAi) {
            TestKeyResult::QuotaExceeded { message } => assert_eq!(
                message,
                "Key is valid but its OpenAI quota is exhausted. Generation will fail until quota resets — check your OpenAI plan/billing."
            ),
            other => panic!("expected QuotaExceeded, got {other:?}"),
        }
    }

    #[test]
    fn map_test_key_error_wire_contract_anthropic() {
        match map_test_key_error(&AiError::InvalidKey, AiProviderKind::Anthropic) {
            TestKeyResult::InvalidKey { message } => assert_eq!(
                message,
                "Anthropic rejected this key or request. This can mean the key is invalid, or your Anthropic account has no available credit/billing configured — check both at console.anthropic.com."
            ),
            other => panic!("expected InvalidKey, got {other:?}"),
        }

        match map_test_key_error(&AiError::QuotaExceeded { free_tier: false }, AiProviderKind::Anthropic) {
            TestKeyResult::QuotaExceeded { message } => assert_eq!(
                message,
                "Key is valid but its Anthropic quota is exhausted. Generation will fail until quota resets — check your Anthropic plan/billing."
            ),
            other => panic!("expected QuotaExceeded, got {other:?}"),
        }
    }

    /// `build_response` surfaces every known provider's key-health status, `None`/`None` until a
    /// real call is recorded.
    #[test]
    fn build_response_surfaces_provider_status() {
        let settings = serde_json::json!({
            "ai": {
                "providers": {
                    "openai": { "key_encrypted": "ct", "last_status": "ok", "last_status_at": "2026-07-29T00:00:00Z" }
                }
            }
        });
        let resp = build_response(&settings, None, None, None, 10);
        let openai = resp.providers.get("openai").expect("openai entry present");
        assert_eq!(openai.last_status.as_deref(), Some("ok"));
        assert_eq!(
            openai.last_status_at.as_deref(),
            Some("2026-07-29T00:00:00Z")
        );

        let gemini = resp.providers.get("gemini").expect("gemini entry present");
        assert_eq!(gemini.last_status, None);
        assert_eq!(gemini.last_status_at, None);
    }

    /// Setting a key emits exactly one `provider_key_set` event naming the provider, never the key.
    #[test]
    fn audit_events_key_set() {
        let before = serde_json::Map::new();
        let mut ai = serde_json::Map::new();
        apply_provider_update(
            &mut ai,
            AiProviderKind::OpenAi,
            ProviderUpdateBody {
                api_key: Some(Some("sk-someRealLookingKeyValue1234567890".to_string())),
                model: None,
                base_url: None,
            },
            TEST_ENCRYPTION_KEY,
        )
        .unwrap();

        let events = collect_ai_settings_audit_events(&before, &ai);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "ai_settings.provider_key_set");
        assert_eq!(events[0].1, serde_json::json!({ "provider": "openai" }));

        // The raw key must never appear anywhere in the details blob.
        let details_text = events[0].1.to_string();
        assert!(!details_text.contains("sk-someRealLookingKeyValue1234567890"));
    }

    /// Clearing a key emits `provider_key_cleared`, again with no key material.
    #[test]
    fn audit_events_key_cleared() {
        let mut before = serde_json::Map::new();
        before.insert(
            "providers".to_string(),
            serde_json::json!({ "openai": { "key_encrypted": "ct" } }),
        );
        let mut after = before.clone();
        apply_provider_update(
            &mut after,
            AiProviderKind::OpenAi,
            ProviderUpdateBody {
                api_key: Some(None),
                model: None,
                base_url: None,
            },
            TEST_ENCRYPTION_KEY,
        )
        .unwrap();

        let events = collect_ai_settings_audit_events(&before, &after);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "ai_settings.provider_key_cleared");
        assert_eq!(events[0].1, serde_json::json!({ "provider": "openai" }));
    }

    /// A model-only change emits exactly one `model_changed` event.
    #[test]
    fn audit_events_model_changed() {
        let mut before = serde_json::Map::new();
        before.insert(
            "providers".to_string(),
            serde_json::json!({ "gemini": { "key_encrypted": "ct", "model": "gemini-3.1-flash-lite" } }),
        );
        let mut after = before.clone();
        apply_provider_update(
            &mut after,
            AiProviderKind::Gemini,
            ProviderUpdateBody {
                api_key: None,
                model: Some(Some("gemini-3.1-pro".to_string())),
                base_url: None,
            },
            TEST_ENCRYPTION_KEY,
        )
        .unwrap();

        let events = collect_ai_settings_audit_events(&before, &after);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "ai_settings.model_changed");
        assert_eq!(
            events[0].1,
            serde_json::json!({ "provider": "gemini", "model": "gemini-3.1-pro" })
        );
    }

    /// `provider_order`/`fallback_enabled` changes each emit an event; re-sending the same value
    /// emits nothing.
    #[test]
    fn audit_events_order_and_fallback_changed_but_not_when_unchanged() {
        let mut before = serde_json::Map::new();
        before.insert("provider_order".to_string(), serde_json::json!(["gemini"]));
        before.insert("fallback_enabled".to_string(), Value::Bool(false));

        let mut after = before.clone();
        after.insert(
            "provider_order".to_string(),
            serde_json::json!(["openai", "gemini"]),
        );
        after.insert("fallback_enabled".to_string(), Value::Bool(true));

        let events = collect_ai_settings_audit_events(&before, &after);
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .any(|(a, d)| *a == "ai_settings.provider_order_changed"
                && *d == serde_json::json!({ "order": ["openai", "gemini"] })));
        assert!(events
            .iter()
            .any(|(a, d)| *a == "ai_settings.fallback_toggled"
                && *d == serde_json::json!({ "enabled": true })));

        assert!(collect_ai_settings_audit_events(&before, &before).is_empty());
    }

    /// Rotating an already-set key is still reported as a "set" event: a fresh key is exactly what
    /// the audit trail is for.
    #[test]
    fn audit_events_key_rotation_reported_as_set() {
        let mut before = serde_json::Map::new();
        before.insert(
            "providers".to_string(),
            serde_json::json!({ "anthropic": { "key_encrypted": "old-ct" } }),
        );
        let mut after = before.clone();
        apply_provider_update(
            &mut after,
            AiProviderKind::Anthropic,
            ProviderUpdateBody {
                api_key: Some(Some("sk-ant-brandNewKeyValue1234567890".to_string())),
                model: None,
                base_url: None,
            },
            TEST_ENCRYPTION_KEY,
        )
        .unwrap();

        let events = collect_ai_settings_audit_events(&before, &after);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "ai_settings.provider_key_set");
        let details_text = events[0].1.to_string();
        assert!(!details_text.contains("brandNewKeyValue"));
        assert!(!details_text.contains("old-ct"));
    }

    /// A request touching only `rate_limit_per_min` must not re-write `providers` in the patch,
    /// since the patch is scoped to changed keys.
    #[test]
    fn scoped_ai_patch_only_includes_changed_keys() {
        let mut before = serde_json::Map::new();
        before.insert(
            "providers".to_string(),
            serde_json::json!({ "gemini": { "key_encrypted": "ct" } }),
        );
        before.insert("rate_limit_per_min".to_string(), serde_json::json!(10));

        let mut after = before.clone();
        after.insert("rate_limit_per_min".to_string(), serde_json::json!(25));

        let patch = scoped_ai_patch(&before, &after);
        assert_eq!(patch.len(), 1);
        assert_eq!(
            patch.get("rate_limit_per_min"),
            Some(&serde_json::json!(25))
        );
        assert!(!patch.contains_key("providers"));
    }

    /// A key removed by the request must appear in the patch as an explicit `null`, or the shallow
    /// `||` merge would keep the stale value.
    #[test]
    fn scoped_ai_patch_represents_a_removed_key_as_null() {
        let mut before = serde_json::Map::new();
        before.insert("rate_limit_per_min".to_string(), serde_json::json!(10));
        let after = serde_json::Map::new();

        let patch = scoped_ai_patch(&before, &after);
        assert_eq!(patch.get("rate_limit_per_min"), Some(&Value::Null));
    }

    #[test]
    fn scoped_ai_patch_is_empty_when_nothing_changed() {
        let mut before = serde_json::Map::new();
        before.insert("fallback_enabled".to_string(), Value::Bool(false));
        assert!(scoped_ai_patch(&before.clone(), &before).is_empty());
    }

    #[test]
    fn build_response_surfaces_free_tier_seen_at() {
        let settings = serde_json::json!({
            "ai": { "providers": { "gemini": { "key_encrypted": "ct", "free_tier_seen_at": "2026-10-07T00:00:00Z" } } }
        });
        let resp = build_response(&settings, None, None, None, 10);
        assert_eq!(
            resp.providers["gemini"].free_tier_seen_at.as_deref(),
            Some("2026-10-07T00:00:00Z")
        );
        assert_eq!(resp.providers["openai"].free_tier_seen_at, None);
    }

    #[test]
    fn key_replacement_or_removal_clears_free_tier_seen_at() {
        for api_key in [Some("sk-ant-brandNewKeyValue1234567890".to_string()), None] {
            let mut ai = serde_json::Map::new();
            ai.insert(
                "providers".to_string(),
                serde_json::json!({ "gemini": { "key_encrypted": "old", "free_tier_seen_at": "2026-10-07T00:00:00Z" } }),
            );
            apply_provider_update(
                &mut ai,
                AiProviderKind::Gemini,
                ProviderUpdateBody {
                    api_key: Some(api_key),
                    model: None,
                    base_url: None,
                },
                TEST_ENCRYPTION_KEY,
            )
            .unwrap();
            assert!(ai["providers"]["gemini"].get("free_tier_seen_at").is_none());
        }
    }
}
