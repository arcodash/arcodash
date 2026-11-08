//! `/api/destinations` — notification destination CRUD.
//! Reads are org-scoped and require `view_destinations`; writes require `manage_destinations`.

use crate::extract::{Json, Path};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use arcodash_core::events::record_event;

use super::encryption_key;
use crate::auth::CurrentUser;
use crate::db::Db;
use crate::error::ApiError;
use crate::services::destination_service;
use crate::state::AppState;

use arcodash_core::entities::notification_destination;

#[derive(Debug, Serialize)]
pub struct DestinationResponse {
    pub id: i64,
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub options: Value,
    pub icon: String,
    pub created_at: DateTime<Utc>,
    /// Where this destination actually points, redacted — e.g.
    /// `"https://hooks.slack.com/…"`, `"sam@…, +2 more"`, `"Routing key ••••1a2b"`. `null` when
    /// there's nothing safe/meaningful to show.
    pub target: Option<String>,
}

fn icon_for_type(t: &str) -> &'static str {
    match t {
        "email" => "fa-envelope",
        "slack" => "fa-slack",
        "webhook" => "fa-bolt",
        "pagerduty" => "fa-pagerduty",
        "mattermost" => "fa-comment",
        "microsoft_teams_webhook" => "fa-windows",
        "discord" => "fa-comments",
        "google_chat" => "fa-google",
        "datadog" => "fa-bar-chart",
        _ => "fa-bell",
    }
}

fn to_response(
    dest: notification_destination::Model,
    key: &[u8; 32],
) -> Result<DestinationResponse, ApiError> {
    // Computed from the FULL decrypted options (secrets included) — never from `options` below,
    // which has already had every secret field stripped.
    let full_options = destination_service::decrypt_full(key, &dest)?;
    Ok(DestinationResponse {
        target: destination_service::compute_target(&dest.r#type, &full_options),
        options: destination_service::decrypt_for_response(key, &dest)?,
        icon: icon_for_type(&dest.r#type).to_string(),
        id: dest.id,
        name: dest.name,
        type_name: dest.r#type,
        created_at: dest.created_at.with_timezone(&Utc),
    })
}

/// `GET /api/destinations/types` — hardcoded catalog.
pub async fn types() -> impl IntoResponse {
    Json(destination_service::destination_types())
}

/// `GET /api/destinations` — requires `view_destinations`.
pub async fn list(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
) -> Result<impl IntoResponse, ApiError> {
    let key = encryption_key(&state)?;
    let dests = destination_service::list(&db, user.org_id, &user).await?;
    let response = dests
        .into_iter()
        .map(|d| to_response(d, &key))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(response))
}

#[derive(Debug, Deserialize)]
pub struct CreateDestinationRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default)]
    pub options: Value,
}

/// `POST /api/destinations`
pub async fn create(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<CreateDestinationRequest>,
) -> Result<impl IntoResponse, ApiError> {
    destination_service::require_manage_destinations(&db, &user).await?;
    let key = encryption_key(&state)?;
    let name = payload.name.clone();
    let dest = destination_service::create(
        &db,
        &key,
        user.org_id,
        user.id,
        payload.name,
        payload.type_name,
        payload.options,
    )
    .await?;

    record_event(
        &db,
        user.org_id,
        Some(user.id),
        "destination.created",
        Some("destination"),
        Some(dest.id),
        serde_json::json!({ "name": name }),
    )
    .await?;

    Ok(Json(to_response(dest, &key)?))
}

/// `GET /api/destinations/:id` — requires `view_destinations`.
pub async fn get(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let key = encryption_key(&state)?;
    let dest = destination_service::get(&db, user.org_id, id, &user).await?;
    Ok(Json(to_response(dest, &key)?))
}

#[derive(Debug, Deserialize)]
pub struct UpdateDestinationRequest {
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub options: Option<Value>,
}

/// `POST /api/destinations/:id`
pub async fn update(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateDestinationRequest>,
) -> Result<impl IntoResponse, ApiError> {
    destination_service::require_manage_destinations(&db, &user).await?;
    let key = encryption_key(&state)?;
    let existing = destination_service::get(&db, user.org_id, id, &user).await?;
    let updated = destination_service::update(
        &db,
        &key,
        existing,
        payload.name,
        payload.type_name,
        payload.options,
    )
    .await?;

    record_event(
        &db,
        user.org_id,
        Some(user.id),
        "destination.updated",
        Some("destination"),
        Some(updated.id),
        serde_json::json!({ "name": updated.name }),
    )
    .await?;

    Ok(Json(to_response(updated, &key)?))
}

#[derive(Debug, Deserialize)]
pub struct TestDestinationRequest {
    /// Inline-options form: send a test through a not-yet-saved configuration (the New/Edit
    /// dialog's live form state).
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub options: Option<Value>,
    /// By-id form: send a test using an already-saved destination's stored (decrypted) options —
    /// the destinations list's row-level test action.
    pub id: Option<i64>,
}

/// `POST /api/destinations/test` — send a generic test message/event, requires
/// `manage_destinations` either way (this reaches a real third party, same bar as create/update).
pub async fn test(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<TestDestinationRequest>,
) -> Result<impl IntoResponse, ApiError> {
    destination_service::require_manage_destinations(&db, &user).await?;
    let key = encryption_key(&state)?;
    let (type_name, options) = if let Some(id) = payload.id {
        let dest = destination_service::get(&db, user.org_id, id, &user).await?;
        let options = destination_service::decrypt_full(&key, &dest)?;
        (dest.r#type, options)
    } else {
        let (Some(type_name), Some(options)) = (payload.type_name, payload.options) else {
            return Err(ApiError::bad_request(
                "either 'id' or both 'type' and 'options' are required",
            ));
        };
        // The inline form sends from the instance's own SMTP identity to a caller-chosen
        // address with no prior review — unlike the by-id form, which only ever reaches a
        // stored, already-reviewed destination. Bounding the recipient to the caller's own
        // address closes that open relay without adding a review step.
        if type_name == "email" {
            let to = options.get("to").and_then(Value::as_str).unwrap_or("");
            if !to.eq_ignore_ascii_case(&user.email) {
                return Err(ApiError::bad_request(
                    "a test to an unsaved destination can only be sent to your own address \
                     — save the destination first, then test it from the list",
                ));
            }
        }
        (type_name, options)
    };

    destination_service::send_test(&state.mailer, &state.config.mail_from, &type_name, &options)
        .await?;

    let recipient = options
        .get("to")
        .and_then(Value::as_str)
        .filter(|_| type_name == "email");
    record_event(
        &db,
        user.org_id,
        Some(user.id),
        "destination.tested",
        Some("destination"),
        None,
        serde_json::json!({
            "type_name": type_name,
            "destination_id": payload.id,
            "recipient": recipient,
        }),
    )
    .await?;

    Ok(Json(serde_json::json!({ "message": "Test message sent" })))
}

/// `DELETE /api/destinations/:id`
pub async fn delete(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    destination_service::require_manage_destinations(&db, &user).await?;
    let existing = destination_service::get(&db, user.org_id, id, &user).await?;
    let (dest_id, name) = (existing.id, existing.name.clone());
    destination_service::delete(&db, existing).await?;

    record_event(
        &db,
        user.org_id,
        Some(user.id),
        "destination.deleted",
        Some("destination"),
        Some(dest_id),
        serde_json::json!({ "name": name }),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}
