//! Reads of the org-level `settings.ai` JSON and resolution of the provider fallback chain, shared
//! by the backend and the worker.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use sea_orm::{ConnectionTrait, DbErr};
use serde_json::Value;

use super::{AiError, AiProviderKind};
use crate::entities::organization;
use crate::org_settings::merge_org_settings_at_returning;
use crate::secrets;
use crate::Config;

pub const ALL_PROVIDER_KINDS: [AiProviderKind; 3] = [
    AiProviderKind::Gemini,
    AiProviderKind::OpenAi,
    AiProviderKind::Anthropic,
];

pub fn ai_obj(settings: &Value) -> Option<&serde_json::Map<String, Value>> {
    settings.get("ai").and_then(Value::as_object)
}

#[must_use]
pub fn provider_obj(
    settings: &Value,
    kind: AiProviderKind,
) -> Option<&serde_json::Map<String, Value>> {
    ai_obj(settings)?
        .get("providers")?
        .as_object()?
        .get(kind.as_str())?
        .as_object()
}

/// A provider's stored ciphertext; Gemini also falls back to the legacy flat
/// `gemini_api_key_encrypted`.
pub fn provider_key_ciphertext(settings: &Value, kind: AiProviderKind) -> Option<&str> {
    provider_obj(settings, kind)
        .and_then(|p| p.get("key_encrypted"))
        .and_then(Value::as_str)
        .or_else(|| match kind {
            AiProviderKind::Gemini => ai_obj(settings)?.get("gemini_api_key_encrypted")?.as_str(),
            _ => None,
        })
}

#[must_use]
pub fn provider_model(settings: &Value, kind: AiProviderKind) -> Option<&str> {
    provider_obj(settings, kind)?.get("model")?.as_str()
}

/// A provider's stored endpoint override; `None` means the provider's default.
pub fn provider_base_url(settings: &Value, kind: AiProviderKind) -> Option<String> {
    provider_obj(settings, kind)?
        .get("base_url")?
        .as_str()
        .map(str::to_string)
}

/// The mutable `providers.<kind>` entry, inserting empty objects if absent.
pub fn provider_entry_mut(
    ai: &mut serde_json::Map<String, Value>,
    kind: AiProviderKind,
) -> &mut serde_json::Map<String, Value> {
    let providers = ai
        .entry("providers".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::default()));
    let providers_map = providers
        .as_object_mut()
        .expect("just ensured providers is an object");
    providers_map
        .entry(kind.as_str().to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::default()))
        .as_object_mut()
        .expect("just ensured provider entry is an object")
}

/// `settings.ai.<key>` as a bool, `default` when absent or not a bool.
fn ai_flag(settings: &Value, key: &str, default: bool) -> bool {
    ai_obj(settings)
        .and_then(|ai| ai.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

/// Whether an org opted into fallback retries beyond the first provider. Default off, since
/// cross-provider billing must be a deliberate admin choice.
#[must_use]
pub fn fallback_enabled(settings: &Value) -> bool {
    ai_flag(settings, "fallback_enabled", false)
}

/// Org monthly AI spend cap in USD; `None` when unset or not a finite positive number.
pub fn monthly_budget_usd(settings: &Value) -> Option<f64> {
    ai_obj(settings)
        .and_then(|ai| ai.get("monthly_budget_usd"))
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v > 0.0)
}

/// Whether sampled column values may be put in AI prompts. Absent/null means on.
#[must_use]
pub fn include_sample_values(settings: &Value) -> bool {
    ai_flag(settings, "include_sample_values", true)
}

/// Whether alert notifications may carry an AI-written explanation. Absent/non-bool means on.
#[must_use]
pub fn explain_alerts(settings: &Value) -> bool {
    ai_flag(settings, "explain_alerts", true)
}

/// Whether dashboard narratives and morning-brief summaries are enabled. Absent/non-bool means on.
#[must_use]
pub fn dashboard_narratives(settings: &Value) -> bool {
    ai_flag(settings, "dashboard_narratives", true)
}

/// The env-level fallback key for a provider
/// (`GEMINI_API_KEY`/`OPENAI_API_KEY`/`ANTHROPIC_API_KEY`), decoupled from `Config` for easy unit
/// testing.
pub fn env_key_for(
    kind: AiProviderKind,
    gemini_api_key_env: Option<&str>,
    openai_api_key_env: Option<&str>,
    anthropic_api_key_env: Option<&str>,
) -> Option<String> {
    match kind {
        AiProviderKind::Gemini => gemini_api_key_env,
        AiProviderKind::OpenAi => openai_api_key_env,
        AiProviderKind::Anthropic => anthropic_api_key_env,
    }
    .map(str::to_string)
}

#[must_use]
pub fn env_model_for(
    kind: AiProviderKind,
    gemini_model_env: &str,
    openai_model_env: &str,
    anthropic_model_env: &str,
) -> Option<String> {
    let model = match kind {
        AiProviderKind::Gemini => gemini_model_env,
        AiProviderKind::OpenAi => openai_model_env,
        AiProviderKind::Anthropic => anthropic_model_env,
    };
    Some(model.to_string())
}

#[must_use]
pub fn provider_has_key(
    settings: &Value,
    kind: AiProviderKind,
    gemini_api_key_env: Option<&str>,
    openai_api_key_env: Option<&str>,
    anthropic_api_key_env: Option<&str>,
) -> bool {
    provider_key_ciphertext(settings, kind).is_some()
        || env_key_for(
            kind,
            gemini_api_key_env,
            openai_api_key_env,
            anthropic_api_key_env,
        )
        .is_some()
}

/// Resolves the effective `provider_order`: the stored list minus unknown or keyless entries.
///
/// If nothing valid remains, exactly one keyed provider becomes the sole entry; otherwise `[]`
/// (ambiguous, an admin must choose).
pub fn resolve_provider_order(
    settings: &Value,
    gemini_api_key_env: Option<&str>,
    openai_api_key_env: Option<&str>,
    anthropic_api_key_env: Option<&str>,
) -> Vec<AiProviderKind> {
    let has_key = |k: &AiProviderKind| {
        provider_has_key(
            settings,
            *k,
            gemini_api_key_env,
            openai_api_key_env,
            anthropic_api_key_env,
        )
    };
    let stored: Vec<AiProviderKind> = ai_obj(settings)
        .and_then(|ai| ai.get("provider_order"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|s| s.parse::<AiProviderKind>().ok())
        .filter(has_key)
        .collect();
    if !stored.is_empty() {
        return stored;
    }

    let configured: Vec<AiProviderKind> = ALL_PROVIDER_KINDS.into_iter().filter(has_key).collect();
    if configured.len() == 1 {
        configured
    } else {
        Vec::new()
    }
}

/// Decrypt a provider's stored key, else its env fallback; `None` if neither. `encryption_key_hex`
/// is passed directly to stay testable.
pub fn effective_provider_key(
    settings: &Value,
    kind: AiProviderKind,
    env_key: Option<&str>,
    encryption_key_hex: &str,
) -> anyhow::Result<Option<String>> {
    let Some(ciphertext_b64) = provider_key_ciphertext(settings, kind) else {
        return Ok(env_key.map(str::to_string));
    };
    let encryption_key =
        secrets::parse_key_hex(encryption_key_hex).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let ciphertext = BASE64
        .decode(ciphertext_b64)
        .map_err(|e| anyhow::anyhow!("stored {} key is not valid base64: {e}", kind.as_str()))?;
    let plaintext = secrets::decrypt(&encryption_key, &ciphertext)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let key = String::from_utf8(plaintext)
        .map_err(|_| anyhow::anyhow!("decrypted {} key is not valid UTF-8", kind.as_str()))?;
    Ok(Some(key))
}

/// The effective key + model for one provider from already-extracted env values; shared by
/// [`effective_ai_config`] and [`resolve_provider_chain_with`]. `None` key means no usable key.
#[allow(clippy::too_many_arguments)]
pub fn resolve_key_and_model(
    settings: &Value,
    kind: AiProviderKind,
    gemini_api_key_env: Option<&str>,
    openai_api_key_env: Option<&str>,
    anthropic_api_key_env: Option<&str>,
    gemini_model_env: &str,
    openai_model_env: &str,
    anthropic_model_env: &str,
    encryption_key_hex: &str,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    let env_key = env_key_for(
        kind,
        gemini_api_key_env,
        openai_api_key_env,
        anthropic_api_key_env,
    );
    let api_key = effective_provider_key(settings, kind, env_key.as_deref(), encryption_key_hex)?;
    let model = provider_model(settings, kind)
        .map(str::to_string)
        .or_else(|| {
            env_model_for(
                kind,
                gemini_model_env,
                openai_model_env,
                anthropic_model_env,
            )
        });
    Ok((api_key, model))
}

/// One resolvable entry in the fallback chain: a provider with everything needed to dispatch.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub kind: AiProviderKind,
    pub api_key: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
}

/// Resolves every entry in [`resolve_provider_order`] into dispatchable providers in priority
/// order. An entry that lost its key mid-request is skipped rather than erroring the chain.
#[allow(clippy::too_many_arguments)]
pub fn resolve_provider_chain_with(
    settings: &Value,
    gemini_api_key_env: Option<&str>,
    openai_api_key_env: Option<&str>,
    anthropic_api_key_env: Option<&str>,
    gemini_model_env: &str,
    openai_model_env: &str,
    anthropic_model_env: &str,
    encryption_key_hex: &str,
) -> anyhow::Result<Vec<ResolvedProvider>> {
    let order = resolve_provider_order(
        settings,
        gemini_api_key_env,
        openai_api_key_env,
        anthropic_api_key_env,
    );

    let mut chain = Vec::with_capacity(order.len());
    for kind in order {
        let (api_key, model) = resolve_key_and_model(
            settings,
            kind,
            gemini_api_key_env,
            openai_api_key_env,
            anthropic_api_key_env,
            gemini_model_env,
            openai_model_env,
            anthropic_model_env,
            encryption_key_hex,
        )?;
        if let Some(api_key) = api_key {
            chain.push(ResolvedProvider {
                kind,
                api_key,
                model,
                base_url: provider_base_url(settings, kind),
            });
        }
    }
    Ok(chain)
}

/// [`resolve_provider_chain_with`] with scalar args extracted from a real `Config`.
pub fn resolve_provider_chain(
    org: &organization::Model,
    config: &Config,
) -> anyhow::Result<Vec<ResolvedProvider>> {
    resolve_provider_chain_with(
        &org.settings,
        config.gemini_api_key.as_deref(),
        config.openai_api_key.as_deref(),
        config.anthropic_api_key.as_deref(),
        &config.gemini_model,
        &config.openai_model,
        &config.anthropic_model,
        &config.encryption_key,
    )
}

/// Maps a provider call outcome to the status string persisted at
/// `settings.ai.providers.<kind>.last_status`.
pub const fn provider_status_str<T>(result: &Result<T, AiError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(AiError::InvalidKey) => "invalid_key",
        Err(AiError::QuotaExceeded { .. }) => "quota_exceeded",
        Err(AiError::RateLimited { .. }) => "rate_limited",
        Err(AiError::Unavailable(_)) => "unavailable",
        Err(AiError::BudgetExceeded { .. }) => "budget_exceeded",
        Err(AiError::Network(_) | AiError::GenerationFailed(_)) => "error",
    }
}

pub const fn provider_free_tier<T>(result: &Result<T, AiError>) -> bool {
    matches!(
        result,
        Err(AiError::QuotaExceeded { free_tier: true }
            | AiError::RateLimited {
                free_tier: true,
                ..
            })
    )
}

/// Persists the outcome of a provider call so admins can see when a key stops working. Best-effort.
///
/// Writes only `last_status`/`last_status_at` (plus `free_tier_seen_at`) atomically against the
/// live row, so a stale `org` snapshot never reverts a concurrent save.
pub async fn record_provider_status(
    db: &impl ConnectionTrait,
    org: &organization::Model,
    kind: AiProviderKind,
    status: &str,
    free_tier: bool,
) -> Result<organization::Model, DbErr> {
    let now = chrono::Utc::now().to_rfc3339();
    let mut patch = serde_json::json!({ "last_status": status, "last_status_at": now });
    if free_tier {
        patch["free_tier_seen_at"] = Value::String(now);
    }
    merge_org_settings_at_returning(db, org.id, &["ai", "providers", kind.as_str()], patch).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_db;

    const TEST_ENCRYPTION_KEY: &str =
        "00000000000000000000000000000000000000000000000000000000000000aa";

    #[test]
    fn monthly_budget_reads_only_positive_finite_numbers() {
        let b =
            |v: Value| monthly_budget_usd(&serde_json::json!({"ai": {"monthly_budget_usd": v}}));
        assert_eq!(b(serde_json::json!(5.0)), Some(5.0));
        for bad in [serde_json::json!(null), 0.into(), (-1.5).into(), "5".into()] {
            assert_eq!(b(bad), None);
        }
        assert_eq!(monthly_budget_usd(&serde_json::json!({})), None);
    }

    #[test]
    fn explain_alerts_defaults_on_and_honors_false() {
        assert!(explain_alerts(&serde_json::json!({})));
        assert!(!explain_alerts(
            &serde_json::json!({"ai": {"explain_alerts": false}})
        ));
        assert!(explain_alerts(
            &serde_json::json!({"ai": {"explain_alerts": "no"}})
        ));
    }

    #[test]
    fn dashboard_narratives_defaults_on_and_honors_false() {
        assert!(dashboard_narratives(&serde_json::json!({})));
        assert!(!dashboard_narratives(
            &serde_json::json!({"ai": {"dashboard_narratives": false}})
        ));
        assert!(dashboard_narratives(
            &serde_json::json!({"ai": {"dashboard_narratives": "no"}})
        ));
    }

    fn encrypt_for_test(plaintext: &str) -> String {
        let key = secrets::parse_key_hex(TEST_ENCRYPTION_KEY).unwrap();
        let ciphertext = secrets::encrypt(&key, plaintext.as_bytes()).unwrap();
        BASE64.encode(ciphertext)
    }

    #[test]
    fn effective_provider_key_resolves_org_env_and_none() {
        let org_ciphertext = encrypt_for_test("org-secret-key-1234567890");
        let settings = serde_json::json!({
            "ai": { "providers": { "gemini": { "key_encrypted": org_ciphertext } } }
        });
        assert_eq!(
            effective_provider_key(&settings, AiProviderKind::Gemini, None, TEST_ENCRYPTION_KEY)
                .unwrap(),
            Some("org-secret-key-1234567890".to_string())
        );

        let none_settings = serde_json::json!({});
        assert_eq!(
            effective_provider_key(
                &none_settings,
                AiProviderKind::Gemini,
                Some("env-key"),
                TEST_ENCRYPTION_KEY
            )
            .unwrap(),
            Some("env-key".to_string())
        );

        assert_eq!(
            effective_provider_key(
                &none_settings,
                AiProviderKind::Gemini,
                None,
                TEST_ENCRYPTION_KEY
            )
            .unwrap(),
            None
        );
    }

    /// The fallback chain resolves every `provider_order` entry in order, each with its own
    /// key/model/base_url.
    #[test]
    fn resolve_provider_chain_resolves_every_entry_in_order() {
        let gemini_ct = encrypt_for_test("gemini-secret-key-1234567890");
        let anthropic_ct = encrypt_for_test("anthropic-secret-key-1234567890");
        let settings = serde_json::json!({
            "ai": {
                "provider_order": ["anthropic", "gemini"],
                "providers": {
                    "gemini": { "key_encrypted": gemini_ct, "model": "gemini-3.1-pro" },
                    "anthropic": { "key_encrypted": anthropic_ct, "base_url": "https://custom.anthropic.example" }
                }
            }
        });

        let chain = resolve_provider_chain_with(
            &settings,
            None,
            None,
            None,
            "gemini-default",
            "openai-default",
            "anthropic-default",
            TEST_ENCRYPTION_KEY,
        )
        .unwrap();

        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].kind, AiProviderKind::Anthropic);
        assert_eq!(chain[0].api_key, "anthropic-secret-key-1234567890");
        assert_eq!(chain[0].model.as_deref(), Some("anthropic-default"));
        assert_eq!(
            chain[0].base_url.as_deref(),
            Some("https://custom.anthropic.example")
        );

        assert_eq!(chain[1].kind, AiProviderKind::Gemini);
        assert_eq!(chain[1].api_key, "gemini-secret-key-1234567890");
        assert_eq!(chain[1].model.as_deref(), Some("gemini-3.1-pro"));
        assert_eq!(chain[1].base_url, None);
    }

    /// A `provider_order` entry with no configured key never reaches the chain.
    #[test]
    fn resolve_provider_chain_omits_keyless_and_unknown_entries() {
        let gemini_ct = encrypt_for_test("gemini-secret-key-1234567890");
        let settings = serde_json::json!({
            "ai": {
                "provider_order": ["openai", "bogus", "gemini"],
                "providers": { "gemini": { "key_encrypted": gemini_ct } }
            }
        });

        let chain = resolve_provider_chain_with(
            &settings,
            None,
            None,
            None,
            "gemini-default",
            "openai-default",
            "anthropic-default",
            TEST_ENCRYPTION_KEY,
        )
        .unwrap();

        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].kind, AiProviderKind::Gemini);
    }

    /// An empty `provider_order` with nothing configured resolves to an empty chain.
    #[test]
    fn resolve_provider_chain_empty_when_nothing_configured() {
        let chain = resolve_provider_chain_with(
            &serde_json::json!({}),
            None,
            None,
            None,
            "gemini-default",
            "openai-default",
            "anthropic-default",
            TEST_ENCRYPTION_KEY,
        )
        .unwrap();
        assert!(chain.is_empty());
    }

    /// The `AiError` -> key-health status mapping, the vocabulary `GET /api/organization/ai`
    /// exposes.
    #[test]
    fn provider_status_str_maps_every_ai_error_variant() {
        let ok: Result<(), AiError> = Ok(());
        assert_eq!(provider_status_str(&ok), "ok");
        let invalid: Result<(), AiError> = Err(AiError::InvalidKey);
        assert_eq!(provider_status_str(&invalid), "invalid_key");
        let quota: Result<(), AiError> = Err(AiError::QuotaExceeded { free_tier: false });
        assert_eq!(provider_status_str(&quota), "quota_exceeded");
        let network: Result<(), AiError> = Err(AiError::Network("timeout".to_string()));
        assert_eq!(provider_status_str(&network), "error");
        let generation: Result<(), AiError> =
            Err(AiError::GenerationFailed("bad json".to_string()));
        assert_eq!(provider_status_str(&generation), "error");
        let limited: Result<(), AiError> = Err(AiError::RateLimited {
            retry_after_secs: 12,
            free_tier: false,
        });
        assert_eq!(provider_status_str(&limited), "rate_limited");
        let unavailable: Result<(), AiError> = Err(AiError::Unavailable("503".to_string()));
        assert_eq!(provider_status_str(&unavailable), "unavailable");
    }

    async fn seed_org(db: &sea_orm::DatabaseConnection, settings: Value) -> organization::Model {
        use sea_orm::ActiveModelTrait;
        use sea_orm::ActiveValue::Set;
        sea_orm::ConnectionTrait::execute_unprepared(
            db,
            "DELETE FROM organizations WHERE slug LIKE 'ai-status-test-%' \
             AND created_at < now() - interval '10 minutes'",
        )
        .await
        .unwrap();
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
        let suffix = uuid::Uuid::new_v4();
        organization::ActiveModel {
            name: Set(format!("ai-status-test-{suffix}")),
            slug: Set(format!("ai-status-test-{suffix}")),
            settings: Set(settings),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    async fn stored_settings(db: &sea_orm::DatabaseConnection, org_id: i64) -> Value {
        use sea_orm::EntityTrait;
        organization::Entity::find_by_id(org_id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
            .settings
    }

    /// The test DB is shared and persists orgs; fake ciphertexts would break a later full-table
    /// pass (key rotation).
    async fn drop_org(db: &sea_orm::DatabaseConnection, org_id: i64) {
        use sea_orm::EntityTrait;
        organization::Entity::delete_by_id(org_id)
            .exec(db)
            .await
            .unwrap();
    }

    /// A status write from a stale org snapshot must not revert fields an admin saved since.
    #[tokio::test]
    async fn record_provider_status_does_not_revert_a_concurrent_key_save() {
        let Some(db) = test_db().await else { return };
        let snapshot = seed_org(
            &db,
            serde_json::json!({ "ai": { "providers": { "gemini": {
                "key_encrypted": "old", "model": "m-old", "free_tier_seen_at": "2020-01-01T00:00:00Z"
            } } } }),
        )
        .await;
        crate::org_settings::merge_org_settings_at(
            &db,
            snapshot.id,
            &["ai", "providers", "gemini"],
            serde_json::json!({ "key_encrypted": "new", "model": "m-new" }),
        )
        .await
        .unwrap();

        record_provider_status(&db, &snapshot, AiProviderKind::Gemini, "ok", false)
            .await
            .unwrap();

        let g = stored_settings(&db, snapshot.id).await["ai"]["providers"]["gemini"].clone();
        assert_eq!(g["key_encrypted"], "new");
        assert_eq!(g["model"], "m-new");
        assert_eq!(g["last_status"], "ok");
        drop_org(&db, snapshot.id).await;
    }

    #[tokio::test]
    async fn record_provider_status_touches_only_status_fields() {
        let Some(db) = test_db().await else { return };
        let org = seed_org(
            &db,
            serde_json::json!({
                "other": { "x": 1 },
                "ai": {
                    "rate_limit_per_min": 42,
                    "providers": {
                        "openai": { "key_encrypted": "o-ct", "model": "gpt" },
                        "gemini": { "key_encrypted": "g-ct", "last_status": "ok" }
                    }
                }
            }),
        )
        .await;
        let before = org.settings.clone();

        let updated =
            record_provider_status(&db, &org, AiProviderKind::Gemini, "quota_exceeded", true)
                .await
                .unwrap();

        let stored = stored_settings(&db, org.id).await;
        assert_eq!(updated.settings, stored);
        let g = &stored["ai"]["providers"]["gemini"];
        assert_eq!(g["last_status"], "quota_exceeded");
        assert_eq!(g["key_encrypted"], "g-ct");
        assert_eq!(g["last_status_at"], g["free_tier_seen_at"]);
        assert_eq!(
            stored["ai"]["providers"]["openai"],
            before["ai"]["providers"]["openai"]
        );
        assert_eq!(stored["ai"]["rate_limit_per_min"], 42);
        assert_eq!(stored["other"], before["other"]);

        let later = record_provider_status(&db, &updated, AiProviderKind::Gemini, "ok", false)
            .await
            .unwrap();
        let g2 = &later.settings["ai"]["providers"]["gemini"];
        assert_eq!(g2["last_status"], "ok");
        assert_eq!(g2["free_tier_seen_at"], g["free_tier_seen_at"]);
        drop_org(&db, org.id).await;
    }

    #[tokio::test]
    async fn record_provider_status_creates_missing_path() {
        let Some(db) = test_db().await else { return };
        for settings in [
            serde_json::json!({}),
            serde_json::json!({ "other": { "x": 1 } }),
            serde_json::json!({ "ai": {} }),
            serde_json::json!({ "ai": { "providers": {} } }),
        ] {
            let org = seed_org(&db, settings.clone()).await;
            record_provider_status(&db, &org, AiProviderKind::OpenAi, "ok", false)
                .await
                .unwrap();
            let stored = stored_settings(&db, org.id).await;
            let o = &stored["ai"]["providers"]["openai"];
            assert_eq!(o["last_status"], "ok", "settings: {settings}");
            assert!(o["last_status_at"].is_string());
            assert!(o.get("free_tier_seen_at").is_none());
            assert_eq!(stored["other"], settings["other"]);
            drop_org(&db, org.id).await;
        }
    }

    #[test]
    fn provider_free_tier_only_for_free_tier_429s() {
        let free: Result<(), AiError> = Err(AiError::QuotaExceeded { free_tier: true });
        assert!(provider_free_tier(&free));
        let free_rl: Result<(), AiError> = Err(AiError::RateLimited {
            retry_after_secs: 1,
            free_tier: true,
        });
        assert!(provider_free_tier(&free_rl));
        let paid: Result<(), AiError> = Err(AiError::QuotaExceeded { free_tier: false });
        assert!(!provider_free_tier(&paid));
        assert!(!provider_free_tier(&Ok(())));
    }
}
