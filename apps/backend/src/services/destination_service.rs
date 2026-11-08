//! Notification destination business logic — CRUD + encrypt/decrypt options.
//! Access: reads require `view_destinations`; create/update/delete require `manage_destinations`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sea_orm::sea_query::Expr;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder,
    TransactionTrait,
};
use serde_json::Value;

use arcodash_core::entities::{escalation_step, notification_destination, user};
use arcodash_core::mailer::Mailer;
use arcodash_core::secrets;

use crate::error::ApiError;

fn decrypt_options(encryption_key: &[u8; 32], encrypted: Option<&[u8]>) -> Result<Value, ApiError> {
    let Some(bytes) = encrypted else {
        return Ok(Value::Object(serde_json::Map::default()));
    };
    let plaintext = secrets::decrypt(encryption_key, bytes)
        .map_err(|_| ApiError::internal("failed to decrypt destination options"))?;
    serde_json::from_slice(&plaintext)
        .map_err(|_| ApiError::internal("stored destination options are not valid JSON"))
}

fn encrypt_options(encryption_key: &[u8; 32], options: &Value) -> Result<Vec<u8>, ApiError> {
    let plaintext = serde_json::to_vec(options)
        .map_err(|_| ApiError::internal("failed to serialize destination options"))?;
    secrets::encrypt(encryption_key, &plaintext)
        .map_err(|_| ApiError::internal("failed to encrypt destination options"))
}

pub async fn require_manage_destinations(
    db: &impl ConnectionTrait,
    user: &user::Model,
) -> Result<(), ApiError> {
    crate::authz::permissions::require_permission(db, user, "manage_destinations").await
}

pub async fn require_view_destinations(
    db: &impl ConnectionTrait,
    user: &user::Model,
) -> Result<(), ApiError> {
    crate::authz::permissions::require_permission(db, user, "view_destinations").await
}

pub async fn list(
    db: &impl ConnectionTrait,
    org_id: i64,
    user: &user::Model,
) -> Result<Vec<notification_destination::Model>, ApiError> {
    require_view_destinations(db, user).await?;

    Ok(notification_destination::Entity::find()
        .filter(notification_destination::Column::OrgId.eq(org_id))
        .all(db)
        .await?)
}

pub async fn get(
    db: &impl ConnectionTrait,
    org_id: i64,
    id: i64,
    user: &user::Model,
) -> Result<notification_destination::Model, ApiError> {
    require_view_destinations(db, user).await?;

    notification_destination::Entity::find_by_id(id)
        .one(db)
        .await?
        .filter(|d| d.org_id == org_id)
        .ok_or_else(|| ApiError::not_found("destination not found"))
}

/// The option keys `destination_types()` declares for `dest_type`; `None` for an unrecognized type.
fn allowed_option_keys(dest_type: &str) -> Option<HashSet<String>> {
    destination_types()
        .as_array()?
        .iter()
        .find(|t| t["type"].as_str() == Some(dest_type))
        .and_then(|t| t["configuration_schema"].as_object())
        .map(|schema| schema.keys().cloned().collect())
}

/// Drops any key in `options` not declared in `allowed` (mirrors
/// `data_source_service::strip_to_schema`).
fn strip_to_schema(options: &mut Value, allowed: &HashSet<String>) {
    if let Some(obj) = options.as_object_mut() {
        obj.retain(|k, _| allowed.contains(k));
    }
}

fn unknown_type_error(type_name: &str) -> ApiError {
    ApiError::bad_request(format!("unknown destination type '{type_name}'"))
}

const EMAIL_RECIPIENT_MESSAGE: &str =
    "Email destinations need at least one valid recipient address.";

fn is_plausible_email(s: &str) -> bool {
    s.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty()
            && !s.contains(char::is_whitespace)
            && !domain.contains('@')
            && domain
                .split_once('.')
                .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty())
    })
}

/// Email destinations need a non-empty comma-separated `to` where every entry looks like an
/// address.
fn validate_email_recipients(options: &Value) -> Result<(), ApiError> {
    let ok = options
        .get("to")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some_and(|to| to.split(',').all(|p| is_plausible_email(p.trim())));
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request(EMAIL_RECIPIENT_MESSAGE))
    }
}

/// `create`'s validation: resolves `type_name` against the catalog and strips `options` to its
/// declared keys. Pure, no I/O.
fn validate_create_options(type_name: &str, mut options: Value) -> Result<Value, ApiError> {
    let allowed = allowed_option_keys(type_name).ok_or_else(|| unknown_type_error(type_name))?;
    strip_to_schema(&mut options, &allowed);
    if type_name == "email" {
        validate_email_recipients(&options)?;
    }
    Ok(options)
}

/// `update`'s options step: merges `new_options` onto the decrypted existing blob, then strips to
/// `final_type`'s keys, only when the type is in the catalog. Pure, no I/O.
fn merge_and_strip_options(
    encryption_key: &[u8; 32],
    existing_encrypted: Option<&[u8]>,
    final_type: &str,
    new_options: Value,
) -> Result<Value, ApiError> {
    // Merge into the decrypted blob so an edit omitting a secret ("leave blank to keep current")
    // never wipes it.
    let mut merged = decrypt_options(encryption_key, existing_encrypted)?;
    if let (Some(base), Some(incoming)) = (merged.as_object_mut(), new_options.as_object()) {
        for (k, v) in incoming {
            base.insert(k.clone(), v.clone());
        }
    } else {
        merged = new_options;
    }
    if let Some(allowed) = allowed_option_keys(final_type) {
        strip_to_schema(&mut merged, &allowed);
    }
    Ok(merged)
}

pub async fn create(
    db: &impl ConnectionTrait,
    encryption_key: &[u8; 32],
    org_id: i64,
    user_id: i64,
    name: String,
    type_name: String,
    options: Value,
) -> Result<notification_destination::Model, ApiError> {
    let options = validate_create_options(&type_name, options)?;
    let encrypted = encrypt_options(encryption_key, &options)?;
    let model = notification_destination::ActiveModel {
        org_id: Set(org_id),
        user_id: Set(Some(user_id)),
        name: Set(name),
        r#type: Set(type_name),
        options: Set(Some(encrypted)),
        ..Default::default()
    };
    Ok(model.insert(db).await?)
}

pub async fn update(
    db: &impl ConnectionTrait,
    encryption_key: &[u8; 32],
    existing: notification_destination::Model,
    name: Option<String>,
    type_name: Option<String>,
    options: Option<Value>,
) -> Result<notification_destination::Model, ApiError> {
    if let Some(t) = &type_name {
        if allowed_option_keys(t).is_none() {
            return Err(unknown_type_error(t));
        }
    }
    let final_type = type_name.clone().unwrap_or_else(|| existing.r#type.clone());
    let type_changed = final_type != existing.r#type;

    let existing_encrypted = existing.options.clone();
    let mut active: notification_destination::ActiveModel = existing.into();
    if let Some(n) = name {
        active.name = Set(n);
    }
    if let Some(t) = type_name {
        active.r#type = Set(t);
    }
    // Re-strip on every type change, or the previous type's secret fields would survive under a
    // redaction list that doesn't name them.
    if options.is_some() || type_changed {
        let merged = merge_and_strip_options(
            encryption_key,
            existing_encrypted.as_deref(),
            &final_type,
            options.unwrap_or_else(|| Value::Object(serde_json::Map::default())),
        )?;
        if final_type == "email" {
            validate_email_recipients(&merged)?;
        }
        active.options = Set(Some(encrypt_options(encryption_key, &merged)?));
    }
    Ok(active.update(db).await?)
}

/// Dense `1..n` renumbering of an ordered `(step_id, position)` list, returning only changed pairs.
/// A hole (`[1, 2, 4, 5]`) would stop the worker's escalation walk dead.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "escalation chains hold a handful of steps, far below i32::MAX"
)]
fn renumbered_positions(steps: &[(i64, i32)]) -> Vec<(i64, i32)> {
    steps
        .iter()
        .enumerate()
        .filter_map(|(i, &(id, pos))| {
            let want = (i + 1) as i32;
            (pos != want).then_some((id, want))
        })
        .collect()
}

/// Deletes a destination and closes the position gap the FK cascade leaves in monitors' escalation
/// chains, in one transaction (see `renumbered_positions`).
pub async fn delete(
    db: &(impl ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>),
    existing: notification_destination::Model,
) -> Result<(), ApiError> {
    let dest_id = existing.id;
    let txn = db.begin().await?;

    let removed = escalation_step::Entity::find()
        .filter(escalation_step::Column::DestinationId.eq(dest_id))
        .all(&txn)
        .await?;
    let mut removed_by_monitor: HashMap<i64, usize> = HashMap::new();
    for step in removed {
        *removed_by_monitor.entry(step.monitor_id).or_default() += 1;
    }

    notification_destination::Entity::delete_by_id(dest_id)
        .exec(&txn)
        .await?;

    for (monitor_id, removed_count) in removed_by_monitor {
        let ordered: Vec<(i64, i32)> = escalation_step::Entity::find()
            .filter(escalation_step::Column::MonitorId.eq(monitor_id))
            .order_by_asc(escalation_step::Column::Position)
            .all(&txn)
            .await?
            .iter()
            .map(|s| (s.id, s.position))
            .collect();
        for (step_id, new_position) in renumbered_positions(&ordered) {
            escalation_step::Entity::update_many()
                .col_expr(escalation_step::Column::Position, Expr::value(new_position))
                .filter(escalation_step::Column::Id.eq(step_id))
                .exec(&txn)
                .await?;
        }
        tracing::warn!(
            monitor_id,
            removed_steps = removed_count,
            "destination delete removed escalation step(s); remaining steps renumbered to stay dense"
        );
    }

    txn.commit().await?;
    Ok(())
}

/// Per-type secret fields stripped from response options (destinations use the hardcoded catalog,
/// not a connector). For webhook-style types the `url` itself is the secret. `None` = unknown type
/// → fail closed.
fn secret_fields_for_type(dest_type: &str) -> Option<&'static [&'static str]> {
    match dest_type {
        "email" => Some(&[]),
        "slack"
        | "mattermost"
        | "microsoft_teams_webhook"
        | "google_chat"
        | "discord"
        | "webhook" => Some(&["url"]),
        "pagerduty" => Some(&["integration_key"]),
        "datadog" => Some(&["api_key", "app_key"]),
        _ => None,
    }
}

pub fn decrypt_for_response(
    encryption_key: &[u8; 32],
    dest: &notification_destination::Model,
) -> Result<Value, ApiError> {
    // Fail closed for an unrecognized `type` (removed kind or forged value): never echo the
    // encrypted blob.
    let Some(secret_fields) = secret_fields_for_type(&dest.r#type) else {
        return Ok(Value::Object(serde_json::Map::default()));
    };
    let mut options = decrypt_options(encryption_key, dest.options.as_deref())?;
    // Strip secret fields so `GET /api/destinations` never echoes tokens/webhook URLs to org
    // members.
    if let Some(obj) = options.as_object_mut() {
        for field in secret_fields {
            obj.remove(*field);
        }
    }
    Ok(options)
}

/// Which `configuration_schema` keys the frontend marks required; never enforced server-side, since
/// a destination is routinely saved before every field is final.
fn required_fields_for_type(dest_type: &str) -> &'static [&'static str] {
    match dest_type {
        "email" => &["to"],
        "slack"
        | "webhook"
        | "mattermost"
        | "microsoft_teams_webhook"
        | "discord"
        | "google_chat" => &["url"],
        "pagerduty" => &["integration_key"],
        "datadog" => &["api_key"],
        _ => &[],
    }
}

/// Full decrypted options (secrets included), used only by [`send_test`]; never returned to a
/// client (contrast [`decrypt_for_response`]).
pub fn decrypt_full(
    encryption_key: &[u8; 32],
    dest: &notification_destination::Model,
) -> Result<Value, ApiError> {
    decrypt_options(encryption_key, dest.options.as_deref())
}

/// A `reqwest::Error`'s `Display` embeds the request URL, which for webhook-style destinations is
/// the secret; mirrors the worker's `redact_reqwest_error` (duplicated, as the binaries don't share
/// code).
fn redact(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

/// Fail fast: this is a synchronous "Send test message" click, and a blocked host would otherwise
/// hang until the OS TCP timeout.
const TEST_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Test-send client with redirects disabled (like the worker's `webhook_client`), so a webhook host
/// can't 302 past `guard_url` into a blocked address.
fn guarded_client() -> Result<reqwest::Client, ApiError> {
    arcodash_core::connectors::http_connectors::http_client()
        .map_err(|e| ApiError::internal(format!("failed to build HTTP client: {e}")))
}

/// `options[key]` as a non-blank string, else a 400 with `missing`.
fn required_str<'a>(options: &'a Value, key: &str, missing: &str) -> Result<&'a str, ApiError> {
    options
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ApiError::bad_request(missing))
}

fn send_failed(e: reqwest::Error) -> ApiError {
    ApiError::bad_request(format!("Failed to send: {}", redact(e)))
}

/// A non-2xx response (including an unfollowed redirect) means the destination wasn't reached;
/// surfaced without the body or URL, which may echo a secret.
#[allow(
    clippy::needless_pass_by_value,
    reason = "consumes the response so its connection is released before any follow-up request"
)]
fn require_success(resp: reqwest::Response) -> Result<(), ApiError> {
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "destination responded with status {}",
            resp.status()
        )))
    }
}

const TEST_TEXT: &str = "Test message from arcodash.";

fn test_body(dest_type: &str, url: &str) -> Value {
    if dest_type == "webhook" {
        return serde_json::json!({ "text": TEST_TEXT });
    }
    arcodash_core::chat_body::chat_test_body(dest_type, url, chrono::Utc::now())
}

/// `test_body` plus Mattermost's `icon_emoji`/`username` overrides, as in the worker's send path.
fn test_body_with_options(dest_type: &str, url: &str, options: &Value) -> Value {
    let mut body = test_body(dest_type, url);
    if dest_type == "mattermost" {
        if let Some(obj) = body.as_object_mut() {
            for key in ["icon_emoji", "username"] {
                if let Some(v) = options.get(key).and_then(Value::as_str) {
                    obj.insert(key.to_string(), serde_json::json!(v));
                }
            }
        }
    }
    body
}

/// Sends a generic test message using the same request shapes as production alerts, so success
/// means an alert would reach it.
pub async fn send_test(
    mailer: &Option<Arc<Mailer>>,
    mail_from: &str,
    dest_type: &str,
    options: &Value,
) -> Result<(), ApiError> {
    match dest_type {
        "email" => {
            let to = required_str(
                options,
                "to",
                "No recipient address is set for this destination.",
            )?;
            match crate::services::mailer::send_email(
                mailer,
                mail_from,
                to,
                "arcodash test notification",
                "This is a test message from arcodash. If you can read this, the destination is configured correctly.",
            )
            .await
            {
                Ok(arcodash_core::mailer::MailOutcome::Sent) => Ok(()),
                // Reporting success without sending would defeat the test; surface an error.
                Ok(arcodash_core::mailer::MailOutcome::Skipped) => Err(ApiError::bad_request(
                    "SMTP is not configured for this deployment (SMTP_HOST is unset).",
                )),
                Err(e) => Err(ApiError::bad_request(format!("Failed to send: {e:#}"))),
            }
        }
        "slack"
        | "mattermost"
        | "microsoft_teams_webhook"
        | "google_chat"
        | "discord"
        | "webhook" => {
            let url = required_str(options, "url", "No URL is set for this destination.")?;
            let (host, addrs) = arcodash_core::connectors::http_connectors::guard_url(url)
                .await
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
            let body = test_body_with_options(dest_type, url, options);
            let resp =
                arcodash_core::connectors::http_connectors::pinned_http_client(&host, &addrs)
                    .map_err(|e| ApiError::internal(format!("failed to build HTTP client: {e}")))?
                    .post(url)
                    .timeout(TEST_SEND_TIMEOUT)
                    .json(&body)
                    .send()
                    .await
                    .map_err(send_failed)?;
            require_success(resp)
        }
        "pagerduty" => {
            const PD_DEDUP_KEY: &str = "arcodash-destination-test";
            const PD_URL: &str = "https://events.pagerduty.com/v2/enqueue";
            let key = required_str(
                options,
                "integration_key",
                "No integration key is set for this destination.",
            )?;
            let client = guarded_client()?;
            let trigger_body = serde_json::json!({
                "routing_key": key,
                "event_action": "trigger",
                "dedup_key": PD_DEDUP_KEY,
                "payload": {
                    "summary": "arcodash test notification",
                    "severity": "info",
                    "source": "arcodash"
                }
            });
            let resp = client
                .post(PD_URL)
                .timeout(TEST_SEND_TIMEOUT)
                .json(&trigger_body)
                .send()
                .await
                .map_err(send_failed)?;
            require_success(resp)?;

            // Resolve the incident right away (same dedup_key) so the test doesn't page on-call;
            // best-effort.
            let resolve_body = serde_json::json!({
                "routing_key": key,
                "event_action": "resolve",
                "dedup_key": PD_DEDUP_KEY,
            });
            if let Err(e) = client
                .post(PD_URL)
                .timeout(TEST_SEND_TIMEOUT)
                .json(&resolve_body)
                .send()
                .await
            {
                tracing::warn!(error = %redact(e), "failed to auto-resolve PagerDuty test incident");
            }
            Ok(())
        }
        "datadog" => {
            let api_key = required_str(
                options,
                "api_key",
                "No API key is set for this destination.",
            )?;
            let site = options.get("site").and_then(Value::as_str).unwrap_or("");
            let api_url = if site.contains("eu") {
                "https://api.datadoghq.eu/api/v1/events"
            } else {
                "https://api.datadoghq.com/api/v1/events"
            };
            let body = serde_json::json!({
                "title": "arcodash test notification",
                "text": "This is a test event from arcodash.",
                "alert_type": "info",
                "aggregation_key": "arcodash-destination-test",
                "tags": ["source:arcodash"]
            });
            let resp = guarded_client()?
                .post(api_url)
                .timeout(TEST_SEND_TIMEOUT)
                .header("DD-API-KEY", api_key)
                .json(&body)
                .send()
                .await
                .map_err(send_failed)?;
            require_success(resp)
        }
        other => Err(ApiError::bad_request(format!(
            "unknown destination type '{other}'"
        ))),
    }
}

/// A display-only, secret-free summary of where a destination points, computed where the decrypted
/// options are available. `None` when nothing safe or meaningful can be shown.
pub fn compute_target(dest_type: &str, options: &Value) -> Option<String> {
    let as_str = |k: &str| {
        options
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    match dest_type {
        // A `channel` field isn't in Slack's schema, but an older or future row carrying one takes
        // priority over the URL.
        "slack" => as_str("channel")
            .map(|c| {
                if c.starts_with('#') {
                    c.to_string()
                } else {
                    format!("#{c}")
                }
            })
            .or_else(|| origin_target(as_str("url"))),
        "webhook" | "mattermost" | "discord" | "microsoft_teams_webhook" | "google_chat" => {
            origin_target(as_str("url"))
        }
        "email" => {
            let addrs: Vec<&str> = as_str("to")?
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            match addrs.as_slice() {
                [] => None,
                [only] => Some(only.to_string()),
                [first, rest @ ..] => Some(format!("{first}, +{} more", rest.len())),
            }
        }
        "pagerduty" => {
            let key = as_str("integration_key")?;
            // Char-based: slicing a multibyte key by byte index can land mid-character and panic.
            let char_count = key.chars().count();
            let last4: String = key.chars().skip(char_count.saturating_sub(4)).collect();
            Some(format!("Routing key ••••{last4}"))
        }
        "datadog" => {
            let site = as_str("site").unwrap_or("");
            Some(if site.contains("eu") {
                "datadoghq.eu".to_string()
            } else {
                "datadoghq.com".to_string()
            })
        }
        _ => None,
    }
}

/// `scheme://host/…` — enough to recognize the service, never the path/token.
fn origin_target(url: Option<&str>) -> Option<String> {
    let parsed = reqwest::Url::parse(url?).ok()?;
    Some(format!("{}://{}/…", parsed.scheme(), parsed.host_str()?))
}

/// Hardcoded destination type catalog with configuration schemas.
#[must_use]
pub fn destination_types() -> serde_json::Value {
    let mut types = serde_json::json!([
        {
            "name": "Email",
            "type": "email",
            "icon": "fa-envelope",
            "configuration_schema": {
                "to": { "type": "string", "title": "To" }
            }
        },
        {
            "name": "Slack",
            "type": "slack",
            "icon": "fa-slack",
            "configuration_schema": {
                "url": { "type": "string", "title": "Webhook URL" }
            }
        },
        {
            "name": "Webhook",
            "type": "webhook",
            "icon": "fa-bolt",
            // Slack's `username`/`icon_emoji` don't belong on a generic webhook.
            "configuration_schema": {
                "url": { "type": "string", "title": "URL" }
            }
        },
        {
            "name": "PagerDuty",
            "type": "pagerduty",
            "icon": "fa-pagerduty",
            "configuration_schema": {
                "integration_key": { "type": "string", "title": "Integration Key" }
            }
        },
        {
            "name": "Mattermost",
            "type": "mattermost",
            "icon": "fa-comment",
            "configuration_schema": {
                "url": { "type": "string", "title": "Webhook URL" },
                "icon_emoji": { "type": "string", "title": "Icon Emoji" },
                "username": { "type": "string", "title": "Username" }
            }
        },
        {
            "name": "Microsoft Teams",
            "type": "microsoft_teams_webhook",
            "icon": "fa-windows",
            "configuration_schema": {
                "url": { "type": "string", "title": "Webhook URL" }
            }
        },
        {
            "name": "Discord",
            "type": "discord",
            "icon": "fa-comments",
            "configuration_schema": {
                "url": { "type": "string", "title": "Webhook URL" }
            }
        },
        {
            "name": "Google Chat",
            "type": "google_chat",
            "icon": "fa-google",
            "configuration_schema": {
                "url": { "type": "string", "title": "Webhook URL" }
            }
        },
        {
            "name": "Datadog",
            "type": "datadog",
            "icon": "fa-bar-chart",
            "configuration_schema": {
                "api_key": { "type": "string", "title": "API Key" },
                "site": { "type": "string", "title": "Site / API URL (optional, default datadoghq.com)" }
            }
        }
    ]);
    for t in types.as_array_mut().unwrap() {
        let ty = t["type"].as_str().unwrap().to_string();
        t["secret"] = serde_json::json!(secret_fields_for_type(&ty).unwrap_or(&[]));
        t["required"] = serde_json::json!(required_fields_for_type(&ty));
    }
    types
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_test_bodies_use_each_destinations_card_shape() {
        let native = test_body("discord", "https://discord.com/api/webhooks/1/tok");
        assert_eq!(native["embeds"][0]["title"], "Test message from arcodash");
        assert!(native.get("attachments").is_none());
        let compat = test_body("discord", "https://discord.com/api/webhooks/1/tok/slack");
        assert_eq!(
            compat["attachments"][0]["title"],
            "Test message from arcodash"
        );
        assert!(compat.get("embeds").is_none());
        let slack = test_body("slack", "https://hooks.slack.com/services/x");
        assert_eq!(
            slack["attachments"][0]["title"],
            "Test message from arcodash"
        );
        let teams = test_body("microsoft_teams_webhook", "https://x.webhook.office.com/y");
        assert_eq!(teams["type"], "message");
        assert_eq!(
            teams["attachments"][0]["contentType"],
            "application/vnd.microsoft.card.adaptive"
        );
        let gchat = test_body("google_chat", "https://chat.googleapis.com/v1/spaces/x");
        assert!(gchat["text"]
            .as_str()
            .unwrap()
            .contains("Test message from arcodash"));
        assert_eq!(
            test_body("webhook", "https://example.test/h"),
            serde_json::json!({ "text": TEST_TEXT })
        );
    }

    #[test]
    fn mattermost_test_body_carries_icon_and_username() {
        let opts = serde_json::json!({ "icon_emoji": ":bell:", "username": "arcodash" });
        let body = test_body_with_options("mattermost", "https://mm.example.test/hook", &opts);
        assert_eq!(body["icon_emoji"], ":bell:");
        assert_eq!(body["username"], "arcodash");
        let slack = test_body_with_options("slack", "https://hooks.example.test/x", &opts);
        assert!(slack.get("icon_emoji").is_none() && slack.get("username").is_none());
    }

    #[test]
    fn email_recipients_validation() {
        let bad = [
            serde_json::json!({}),
            serde_json::json!({"to": ""}),
            serde_json::json!({"to": "   "}),
            serde_json::json!({"to": "nope"}),
            serde_json::json!({"to": "a@b"}),
            serde_json::json!({"to": "a@x.io, bad"}),
            serde_json::json!({"to": 5}),
        ];
        for o in bad {
            assert!(validate_email_recipients(&o).is_err(), "{o}");
        }
        assert!(validate_email_recipients(&serde_json::json!({"to": "a@x.io"})).is_ok());
        assert!(validate_email_recipients(&serde_json::json!({"to": " a@x.io , b@y.co "})).is_ok());
        assert!(validate_create_options("email", serde_json::json!({})).is_err());
        assert!(validate_create_options("slack", serde_json::json!({})).is_ok());
    }

    #[test]
    fn renumbered_positions_closes_a_gap() {
        let steps = vec![(101, 1), (102, 2), (104, 4), (105, 5)];
        assert_eq!(
            renumbered_positions(&steps),
            vec![(104, 3), (105, 4)],
            "only the steps after the gap should move, down by one each"
        );
    }

    #[test]
    fn renumbered_positions_already_dense_is_unchanged() {
        let steps = vec![(1, 1), (2, 2), (3, 3)];
        assert!(renumbered_positions(&steps).is_empty());
    }

    #[test]
    fn renumbered_positions_empty_is_unchanged() {
        assert!(renumbered_positions(&[]).is_empty());
    }

    #[test]
    fn compute_target_webhook_style_shows_origin_never_the_full_url() {
        for ty in [
            "slack",
            "webhook",
            "mattermost",
            "discord",
            "microsoft_teams_webhook",
            "google_chat",
        ] {
            let target = compute_target(
                ty,
                &serde_json::json!({"url": "https://hooks.slack.com/services/T00/B00/SECRETTOKEN"}),
            )
            .unwrap();
            assert_eq!(target, "https://hooks.slack.com/…");
            assert!(
                !target.contains("SECRETTOKEN"),
                "{ty} target must not leak the token"
            );
        }
    }

    #[test]
    fn compute_target_slack_channel_wins_over_url() {
        let target = compute_target(
            "slack",
            &serde_json::json!({"url": "https://hooks.slack.com/services/T00", "channel": "ops-alerts"}),
        )
        .unwrap();
        assert_eq!(target, "#ops-alerts");
    }

    #[test]
    fn compute_target_email_single_and_multiple() {
        assert_eq!(
            compute_target("email", &serde_json::json!({"to": "sam@x.io"})).unwrap(),
            "sam@x.io"
        );
        assert_eq!(
            compute_target(
                "email",
                &serde_json::json!({"to": "sam@x.io, joe@x.io, ann@x.io"})
            )
            .unwrap(),
            "sam@x.io, +2 more"
        );
    }

    #[test]
    fn compute_target_pagerduty_shows_only_last_four() {
        let target = compute_target(
            "pagerduty",
            &serde_json::json!({"integration_key": "R0123456789abcdef1a2b"}),
        )
        .unwrap();
        assert_eq!(target, "Routing key ••••1a2b");
        assert!(!target.contains("R0123456789abcdef"));
    }

    #[test]
    fn compute_target_datadog_site() {
        assert_eq!(
            compute_target(
                "datadog",
                &serde_json::json!({"api_key": "k", "site": "datadoghq.eu"})
            )
            .unwrap(),
            "datadoghq.eu"
        );
        assert_eq!(
            compute_target("datadog", &serde_json::json!({"api_key": "k"})).unwrap(),
            "datadoghq.com"
        );
    }

    #[test]
    fn compute_target_pagerduty_multibyte_key_does_not_panic() {
        // "é" is 2 bytes / 1 char; byte-index slicing at `len - 4` lands mid-character here.
        let target =
            compute_target("pagerduty", &serde_json::json!({"integration_key": "éabc"})).unwrap();
        assert_eq!(target, "Routing key ••••éabc");
    }

    #[test]
    fn compute_target_missing_field_or_unmapped_type_is_none() {
        assert_eq!(compute_target("webhook", &serde_json::json!({})), None);
        assert_eq!(
            compute_target("something-unmapped", &serde_json::json!({"url": "x"})),
            None
        );
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = [7u8; 32];
        let opts = serde_json::json!({"url": "https://hooks.slack.com/T123"});
        let enc = encrypt_options(&key, &opts).unwrap();
        let dec = decrypt_options(&key, Some(&enc)).unwrap();
        assert_eq!(dec, opts);
    }

    #[test]
    fn create_and_update_reject_unknown_type() {
        let err = validate_create_options("not-a-real-type", serde_json::json!({})).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn create_strips_a_secret_sent_under_a_bogus_field_name() {
        // Slack's schema only declares `url`; a secret under another key must not survive into
        // storage.
        let options =
            serde_json::json!({"url": "https://hooks.slack.com/T123", "token": "SECRETVALUE"});
        let stripped = validate_create_options("slack", options).unwrap();
        assert!(stripped.get("token").is_none());

        let key = [5u8; 32];
        let encrypted = encrypt_options(&key, &stripped).unwrap();
        let decrypted = decrypt_options(&key, Some(&encrypted)).unwrap();
        assert!(!decrypted.to_string().contains("SECRETVALUE"));
    }

    #[test]
    fn update_strips_stale_keys_post_merge() {
        let key = [5u8; 32];
        let existing = encrypt_options(
            &key,
            &serde_json::json!({"url": "https://old", "stale": "leftover"}),
        )
        .unwrap();
        let merged = merge_and_strip_options(
            &key,
            Some(&existing),
            "slack",
            serde_json::json!({"url": "https://new"}),
        )
        .unwrap();
        assert_eq!(merged["url"], "https://new");
        assert!(merged.get("stale").is_none());
    }

    fn dest_model(
        id: i64,
        name: &str,
        dest_type: &str,
        options: Option<Vec<u8>>,
    ) -> notification_destination::Model {
        notification_destination::Model {
            id,
            org_id: 1,
            user_id: None,
            name: name.to_string(),
            r#type: dest_type.to_string(),
            options,
            created_at: chrono::Utc::now().into(),
            updated_at: chrono::Utc::now().into(),
        }
    }

    #[test]
    fn decrypt_for_response_fails_closed_for_unknown_destination_type() {
        let key = [3u8; 32];
        let opts = serde_json::json!({"api_key": "super-secret-key"});
        let encrypted = encrypt_options(&key, &opts).unwrap();

        let dest = dest_model(
            1,
            "Mystery Destination",
            "totally-unknown-destination-type",
            Some(encrypted),
        );

        let result = decrypt_for_response(&key, &dest).unwrap();
        assert_eq!(
            result,
            Value::Object(serde_json::Map::default()),
            "unknown destination type must yield an empty object, never the decrypted blob"
        );
    }

    #[test]
    fn decrypt_for_response_strips_secret_fields_per_type() {
        let key = [3u8; 32];

        // Webhook: the whole `url` is the secret token.
        let webhook = dest_model(
            1,
            "Slack",
            "slack",
            Some(
                encrypt_options(
                    &key,
                    &serde_json::json!({"url": "https://hooks.slack.com/services/SECRET/TOKEN"}),
                )
                .unwrap(),
            ),
        );
        let out = decrypt_for_response(&key, &webhook).unwrap();
        assert!(out.get("url").is_none(), "webhook url must be stripped");
        assert!(
            !out.to_string().contains("SECRET"),
            "no secret value may appear in the response"
        );

        // Datadog: api_key/app_key stripped, non-secret `site` preserved.
        let datadog = dest_model(
            2,
            "DD",
            "datadog",
            Some(
                encrypt_options(
                    &key,
                    &serde_json::json!({"api_key": "SECRETKEY", "app_key": "SECRETAPP", "site": "datadoghq.eu"}),
                )
                .unwrap(),
            ),
        );
        let out = decrypt_for_response(&key, &datadog).unwrap();
        assert!(out.get("api_key").is_none());
        assert!(out.get("app_key").is_none());
        assert_eq!(
            out.get("site").and_then(Value::as_str),
            Some("datadoghq.eu")
        );
        assert!(!out.to_string().contains("SECRET"));
    }

    #[test]
    fn destination_types_expose_secret_field_lists() {
        let types = destination_types();
        let arr = types.as_array().unwrap();
        assert!(!arr.is_empty());
        for t in arr {
            let secret = t["secret"]
                .as_array()
                .unwrap_or_else(|| panic!("{:?} missing secret array", t["type"]));
            match t["type"].as_str().unwrap() {
                "slack" => assert_eq!(secret, &vec![Value::String("url".to_string())]),
                "datadog" => assert_eq!(
                    secret,
                    &vec![
                        Value::String("api_key".to_string()),
                        Value::String("app_key".to_string())
                    ]
                ),
                "email" => assert!(secret.is_empty()),
                _ => {}
            }
        }
    }

    // ── DB-backed: update() type-change re-strip, delete() renumber transaction ──

    /// A fresh org and user on `TEST_DATABASE_URL`; `None` (test skipped) when it's unset.
    async fn seed() -> Option<(
        sea_orm::DatabaseConnection,
        arcodash_core::entities::organization::Model,
        user::Model,
    )> {
        let Ok(db_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return None;
        };
        let db = arcodash_core::db::connect(&db_url).await.unwrap();
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let mut org = arcodash_core::test_support::new_organization();
        org.slug = Set(format!("dest-svc-{tag}"));
        let org = org.insert(&db).await.unwrap();
        let mut user = arcodash_core::test_support::new_user(org.id);
        user.email = Set(format!("dest-svc-{tag}@test.local"));
        user.api_key_hash = Set(arcodash_core::hash::hash_api_key(&format!(
            "dest-svc-{tag}"
        )));
        let user = user.insert(&db).await.unwrap();
        Some((db, org, user))
    }

    /// A PATCH that only flips `type` must still re-strip stored options to the new type's schema,
    /// or the old type's secret `url` would ride out in cleartext.
    #[tokio::test]
    async fn type_change_without_options_drops_the_previous_types_secret_fields() {
        let Some((db, org, user)) = seed().await else {
            return;
        };
        let key = [9u8; 32];
        let secret_url = "https://hooks.slack.com/services/T00/B00/DangleTestSecret";

        let dest = create(
            &db,
            &key,
            org.id,
            user.id,
            "Slack".to_string(),
            "slack".to_string(),
            serde_json::json!({ "url": secret_url }),
        )
        .await
        .unwrap();

        let updated = update(
            &db,
            &key,
            dest,
            None,
            Some("datadog".to_string()),
            None, // no options sent — the type-only PATCH the UI's dropdown fires first
        )
        .await
        .unwrap();
        assert_eq!(updated.r#type, "datadog");

        let stored = decrypt_options(&key, updated.options.as_deref()).unwrap();
        assert!(
            stored.get("url").is_none(),
            "datadog's schema doesn't recognize `url` — it must be stripped, not merely redacted: {stored}"
        );
        assert!(
            !stored.to_string().contains("hooks.slack.com"),
            "the previous type's secret must not survive anywhere in the stored options: {stored}"
        );

        // The API-facing redaction must also show nothing, as a second line of defense.
        let response_view = decrypt_for_response(&key, &updated).unwrap();
        assert!(response_view.get("url").is_none());
    }

    /// Positive control: a type change that carries `options` still merges normally.
    #[tokio::test]
    async fn type_change_with_options_still_merges_as_before() {
        let Some((db, org, user)) = seed().await else {
            return;
        };
        let key = [9u8; 32];

        let dest = create(
            &db,
            &key,
            org.id,
            user.id,
            "Mattermost".to_string(),
            "mattermost".to_string(),
            serde_json::json!({ "url": "https://mm.example.test/hook", "username": "old" }),
        )
        .await
        .unwrap();

        let updated = update(
            &db,
            &key,
            dest,
            None,
            Some("mattermost".to_string()),
            Some(serde_json::json!({ "username": "new" })),
        )
        .await
        .unwrap();

        let stored = decrypt_options(&key, updated.options.as_deref()).unwrap();
        assert_eq!(
            stored.get("url").and_then(Value::as_str),
            Some("https://mm.example.test/hook"),
            "an options-bearing update must still preserve an omitted secret via merge"
        );
        assert_eq!(stored.get("username").and_then(Value::as_str), Some("new"));
    }

    /// `delete`'s cascade-delete-then-renumber must be atomic, so no monitor is left with a
    /// positional gap in its escalation chain (real 4-step chain).
    #[tokio::test]
    async fn delete_renumbers_escalation_steps_after_removing_a_mid_chain_destination() {
        let Some((db, org, user)) = seed().await else {
            return;
        };
        let key = [9u8; 32];

        let ds = arcodash_core::test_support::new_data_source(org.id)
            .insert(&db)
            .await
            .unwrap();
        let mut q = arcodash_core::test_support::new_query(org.id, ds.id, user.id);
        q.api_key = Set(uuid::Uuid::new_v4().simple().to_string());
        let query = q.insert(&db).await.unwrap();
        let monitor = arcodash_core::entities::monitor::ActiveModel {
            org_id: Set(org.id),
            name: Set("Escalating".to_string()),
            query_id: Set(query.id),
            user_id: Set(user.id),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let mut dests = Vec::new();
        for i in 0..4 {
            dests.push(
                create(
                    &db,
                    &key,
                    org.id,
                    user.id,
                    format!("Dest {i}"),
                    "webhook".to_string(),
                    serde_json::json!({ "url": format!("https://example.test/{i}") }),
                )
                .await
                .unwrap(),
            );
        }

        let now = chrono::Utc::now();
        for (i, dest) in dests.iter().enumerate() {
            escalation_step::ActiveModel {
                org_id: Set(org.id),
                monitor_id: Set(monitor.id),
                position: Set((i + 1) as i32),
                delay_minutes: Set(5 * (i as i32 + 1)),
                destination_id: Set(dest.id),
                created_at: Set(now.into()),
                updated_at: Set(now.into()),
                ..Default::default()
            }
            .insert(&db)
            .await
            .unwrap();
        }

        // Remove step 2's destination — the cascade FK deletes its escalation_step row too.
        let removed = dests.remove(1);
        delete(&db, removed).await.unwrap();

        let remaining = escalation_step::Entity::find()
            .filter(escalation_step::Column::MonitorId.eq(monitor.id))
            .order_by_asc(escalation_step::Column::Position)
            .all(&db)
            .await
            .unwrap();
        let positions: Vec<i32> = remaining.iter().map(|s| s.position).collect();
        assert_eq!(
            positions,
            vec![1, 2, 3],
            "surviving steps must be densely renumbered with no gap"
        );
        let destination_ids: Vec<i64> = remaining.iter().map(|s| s.destination_id).collect();
        assert_eq!(
            destination_ids,
            dests.iter().map(|d| d.id).collect::<Vec<_>>(),
            "renumbering must preserve the surviving steps' relative order"
        );
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use sea_orm::Database;

    #[tokio::test]
    #[ignore = "sends a real message to a live destination"]
    async fn send_test_live_destination() {
        let (dest_type, opts) = if let Ok(url) = std::env::var("LIVE_WEBHOOK_URL") {
            let t = std::env::var("LIVE_WEBHOOK_TYPE").unwrap_or_else(|_| "discord".into());
            (t, serde_json::json!({ "url": url }))
        } else {
            let id: i64 = std::env::var("DISCORD_LIVE_DEST_ID")
                .expect("DISCORD_LIVE_DEST_ID unset")
                .parse()
                .unwrap();
            let db = Database::connect(
                std::env::var("DISCORD_LIVE_DB_URL").expect("DISCORD_LIVE_DB_URL unset"),
            )
            .await
            .unwrap();
            let dest = notification_destination::Entity::find_by_id(id)
                .one(&db)
                .await
                .unwrap()
                .expect("destination not found");
            let key = secrets::parse_key_hex(
                &std::env::var("ENCRYPTION_KEY").expect("ENCRYPTION_KEY unset"),
            )
            .unwrap();
            let opts: Value = serde_json::from_slice(
                &secrets::decrypt(&key, dest.options.as_deref().unwrap()).unwrap(),
            )
            .unwrap();
            (dest.r#type, opts)
        };
        let url = opts.get("url").and_then(Value::as_str).unwrap_or_default();
        let url_shape =
            if arcodash_core::chat_body::chat_test_body(&dest_type, url, chrono::Utc::now())
                .get("embeds")
                .is_none()
            {
                "slack"
            } else {
                "native"
            };
        let ok = send_test(&None, "", &dest_type, &opts).await.is_ok();
        println!(
            "ok={ok} url_shape={url_shape} status={}",
            if ok { "sent" } else { "failed" }
        );
        assert!(ok, "send_test failed (details redacted)");
    }
}
