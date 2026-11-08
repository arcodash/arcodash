//! `/api/users*`: user management, invitations and password reset, plus `POST /api/users/me/digest`
//! and the public `GET /api/digest/unsubscribe` page.

use crate::extract::{Json, Path};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use chrono::{DateTime, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};

use arcodash_core::entities::user;
use arcodash_core::events::record_event;

use crate::api::handlers::auth::{host_header, record_event_or_warn, session_jar};
use crate::auth::session::{build_csrf_cookie, secure_cookie_attribute};
use crate::auth::{CurrentOrg, CurrentUser};
use crate::db::Db;
use crate::error::ApiError;
use crate::services::user_service;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct UserResponse {
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    pub email: String,
    pub groups: Vec<i64>,
    pub is_invitation_pending: bool,
    pub disabled: bool,
    pub profile_image_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub digest_enabled: bool,
    pub digest_hour: Option<i16>,
    pub digest_timezone: Option<String>,
}

/// A user's API key is never included: only its digest is stored, so the plaintext exists once, in
/// the response that generated it.
fn user_response(u: user::Model) -> UserResponse {
    UserResponse {
        id: u.id,
        org_id: u.org_id,
        name: u.name,
        email: u.email,
        groups: u.groups,
        is_invitation_pending: u.is_invitation_pending,
        disabled: u.disabled_at.is_some(),
        profile_image_url: u.profile_image_url,
        created_at: u.created_at.with_timezone(&Utc),
        updated_at: u.updated_at.with_timezone(&Utc),
        digest_enabled: u.digest_enabled,
        digest_hour: u.digest_hour,
        digest_timezone: u.digest_timezone,
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ListUsersQuery {
    #[serde(default)]
    pub include_disabled: bool,
}

/// `GET /api/users` — requires `view_users`.
pub async fn list(
    db: Db,
    CurrentUser(caller): CurrentUser,
    Query(params): Query<ListUsersQuery>,
) -> Result<impl IntoResponse, ApiError> {
    crate::authz::permissions::require_permission(&db, &caller, "view_users").await?;
    let users = user_service::list(&db, caller.org_id, params.include_disabled).await?;
    Ok(Json(
        users.into_iter().map(user_response).collect::<Vec<_>>(),
    ))
}

#[derive(Debug, Deserialize)]
pub struct InviteUserRequest {
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub groups: Vec<i64>,
}

/// `POST /api/users`: invite a user (`manage_users`), emailing the invitation if SMTP is
/// configured. Re-inviting a pending email resends with a fresh token.
pub async fn invite(
    State(state): State<AppState>,
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(caller): CurrentUser,
    Json(payload): Json<InviteUserRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let (u, token) = user_service::invite(
        &db,
        caller.org_id,
        &caller,
        payload.name,
        payload.email,
        payload.groups,
    )
    .await?;

    send_invitation_email(&db, &state, &org.name, &caller, &u, &token).await;

    // A key returned before acceptance would be unusable and leak a bearer secret; the row keeps
    // its generated key.
    Ok(Json(user_response(u)))
}

/// Shared by `invite` and `resend_invitation`. Best-effort: with no SMTP it logs the link, and a
/// delivery error doesn't fail the request (the invite row is persisted); failures are logged and
/// recorded as an event. `inviter` is always the authenticated caller.
async fn send_invitation_email(
    db: &impl sea_orm::ConnectionTrait,
    state: &AppState,
    org_name: &str,
    inviter: &user::Model,
    u: &user::Model,
    token: &str,
) {
    let invite_url = format!(
        "{}/users/invitation/accept?token={token}",
        state.config.base_url
    );
    // SSO users can carry an empty name; omit the inviter fact rather than interpolate it or say
    // "someone".
    let inviter_name_known = !inviter.name.trim().is_empty();
    let inviter_label = format!("{} ({})", inviter.name, inviter.email);
    let expires = format!("in {} days", user_service::INVITATION_TOKEN_TTL_DAYS);

    let subject = if inviter_name_known {
        format!("{} invited you to {org_name} on arcodash", inviter.name)
    } else {
        format!("You've been invited to {org_name} on arcodash")
    };
    let heading = format!("Join {org_name} on arcodash");
    let intro = format!(
        "{inviter_label} invited you to the {org_name} workspace on arcodash, a shared place to \
         query data, build dashboards, and catch bad numbers early."
    );
    let mut facts: Vec<(&str, String)> = vec![("Organization", org_name.to_string())];
    if inviter_name_known {
        facts.push(("Invited by", inviter_label));
    }
    facts.push(("Your email", u.email.clone()));
    facts.push(("Link expires", expires));
    let footer = "You're receiving this because someone invited you to a workspace on arcodash.\n\
                  If you weren't expecting this, you can ignore this email.";

    let layout = arcodash_core::email_template::EmailLayout {
        preheader: &format!("{} invited you to the {org_name} workspace.", inviter.name),
        heading: &heading,
        intro: &intro,
        facts: &facts,
        sections: &[],
        cta: Some(("Accept invitation", &invite_url)),
        secondary: &[],
        footer,
    };
    let (text, html) = arcodash_core::email_template::render(&layout);

    if state.mailer.is_some() {
        if let Err(e) = crate::services::mailer::send_html_email(
            &state.mailer,
            &state.config.mail_from,
            &u.email,
            &subject,
            &text,
            &html,
        )
        .await
        {
            tracing::error!(email = %u.email, error = %e, "invitation email delivery failed");
            record_event_or_warn(
                db,
                u.org_id,
                Some(inviter.id),
                "user.invite_email_failed",
                Some(("user", u.id)),
                serde_json::json!({ "email": u.email }),
            )
            .await;
        }
    } else {
        tracing::info!(email = %u.email, invite_url = %invite_url, "SMTP not configured — invitation not emailed");
    }
}

/// `POST /api/users/:id/resend_invitation` — regenerate the token and re-email the invite link.
pub async fn resend_invitation(
    State(state): State<AppState>,
    db: Db,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(caller): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let (u, token) = user_service::resend_invitation(&db, caller.org_id, &caller, id).await?;
    send_invitation_email(&db, &state, &org.name, &caller, &u, &token).await;
    Ok(Json(user_response(u)))
}

/// `DELETE /api/users/:id/invitation` — cancel a pending invitation outright.
pub async fn revoke_invitation(
    db: Db,
    CurrentUser(caller): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    user_service::revoke_invitation(&db, caller.org_id, &caller, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/users/:id` — self-fetch is always allowed; reading another teammate's profile
/// requires `view_users`.
pub async fn get(
    db: Db,
    CurrentUser(caller): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    if caller.id != id {
        crate::authz::permissions::require_permission(&db, &caller, "view_users").await?;
    }
    let u = user_service::get(&db, caller.org_id, id).await?;
    Ok(Json(user_response(u)))
}

/// Email is deliberately not a field (see `user_service::update`); an `email` key in the body is
/// silently ignored (no `deny_unknown_fields`).
#[derive(Debug, Deserialize, Default)]
pub struct UpdateUserRequest {
    pub name: Option<String>,
    pub groups: Option<Vec<i64>>,
    pub disabled: Option<bool>,
}

/// `POST /api/users/:id`
pub async fn update(
    db: Db,
    CurrentUser(caller): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateUserRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let u = user_service::update(
        &db,
        caller.org_id,
        &caller,
        id,
        payload.name,
        payload.groups,
        payload.disabled,
    )
    .await?;
    Ok(Json(user_response(u)))
}

/// `DELETE /api/users/:id`
pub async fn delete(
    db: Db,
    CurrentUser(caller): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    user_service::disable(&db, caller.org_id, &caller, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/users/:id/regenerate_api_key` — the response carries the only plaintext copy of the
/// new key; the row stores just its digest.
pub async fn regenerate_api_key(
    db: Db,
    CurrentUser(caller): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let new_key = user_service::regenerate_api_key(&db, caller.org_id, &caller, id).await?;
    Ok(Json(serde_json::json!({ "api_key": new_key })))
}

/// `GET /api/users/:id/effective_permissions` (admin-only): the flat permission set from the user's
/// groups' roles, plus an admin flag.
pub async fn effective_permissions(
    db: Db,
    CurrentUser(caller): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    super::require_admin(&db, &caller).await?;
    let u = user_service::get(&db, caller.org_id, id).await?;
    let permissions = crate::authz::permissions::effective_permission_keys(&db, &u).await?;
    let is_admin = permissions.iter().any(|p| p == "admin");
    Ok(Json(
        serde_json::json!({ "permissions": permissions, "is_admin": is_admin }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct VerifyInviteQuery {
    pub token: String,
}

/// `GET /api/users/invitation/verify?token=...` — public; validates the token and returns email.
pub async fn verify_invitation(
    db: Db,
    Query(params): Query<VerifyInviteQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let u = user_service::verify_invitation(&db, &params.token).await?;
    Ok(Json(
        serde_json::json!({ "email": u.email, "name": u.name }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct AcceptInviteRequest {
    pub token: String,
    pub name: String,
    pub password: String,
}

/// `POST /api/users/invitation/accept`: creates the account and signs the user straight in, since
/// the token and chosen password already prove identity.
pub async fn accept_invitation(
    State(state): State<AppState>,
    db: Db,
    headers: HeaderMap,
    Json(payload): Json<AcceptInviteRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let u = user_service::accept_invitation(&db, &payload.token, payload.name, payload.password)
        .await?;

    record_event(
        &db,
        u.org_id,
        Some(u.id),
        "user.login",
        Some("user"),
        Some(u.id),
        serde_json::json!({ "via": "invitation_accept" }),
    )
    .await?;

    let secure = secure_cookie_attribute(host_header(&headers));
    let jar = session_jar(&state, &u, u.org_id, &headers)?
        .add(build_csrf_cookie(uuid::Uuid::new_v4().to_string(), secure));

    Ok((jar, Json(user_response(u))))
}

#[derive(Debug, Deserialize)]
pub struct ForgotPasswordRequest {
    pub email: String,
}

/// `POST /api/forgot_password` — sends a password-reset link. Always returns 204 (no enumeration).
pub async fn forgot_password(
    State(state): State<AppState>,
    db: Db,
    CurrentOrg(org): CurrentOrg,
    Json(payload): Json<ForgotPasswordRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if let Some((u, token)) = user_service::forgot_password(&db, org.id, &payload.email).await? {
        let reset_url = format!("{}/reset_password?token={token}", state.config.base_url);
        let body = format!(
            "Hi {},\n\nClick the link below to reset your password (valid for 1 hour):\n\n{}\n",
            u.name, reset_url
        );
        if state.mailer.is_some() {
            if let Err(e) = crate::services::mailer::send_email(
                &state.mailer,
                &state.config.mail_from,
                &u.email,
                "arcodash password reset",
                &body,
            )
            .await
            {
                tracing::error!(email = %u.email, error = %e, "password reset email delivery failed");
                // Always 204 (no enumeration); an audit-write failure must not turn that into a
                // 500.
                record_event_or_warn(
                    &db,
                    u.org_id,
                    Some(u.id),
                    "user.reset_email_failed",
                    Some(("user", u.id)),
                    serde_json::json!({ "email": u.email }),
                )
                .await;
            }
        } else {
            tracing::info!(email = %u.email, reset_url = %reset_url, "SMTP not configured — reset link not emailed");
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct ResetPasswordRequest {
    pub token: String,
    pub password: String,
}

/// `POST /api/reset_password`
pub async fn reset_password(
    db: Db,
    Json(payload): Json<ResetPasswordRequest>,
) -> Result<impl IntoResponse, ApiError> {
    user_service::reset_password(&db, &payload.token, &payload.password).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct SetDigestRequest {
    pub enabled: bool,
    pub hour: Option<i16>,
    pub timezone: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DigestResponse {
    pub enabled: bool,
    pub hour: Option<i16>,
    pub timezone: Option<String>,
}

/// `POST /api/users/me/digest`: self-serve only (no `{id}` in the path).
///
/// Enabling regenerates the unsubscribe token so old links die; disabling clears it. The first
/// enable must carry both `hour` and `timezone` so a user is never implicitly scheduled at UTC;
/// afterwards either can be updated alone.
pub async fn set_digest(
    db: Db,
    CurrentUser(caller): CurrentUser,
    Json(payload): Json<SetDigestRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if payload.hour.is_some_and(|h| !(0..=23).contains(&h)) {
        return Err(ApiError::bad_request("hour must be between 0 and 23"));
    }
    if payload
        .timezone
        .as_deref()
        .is_some_and(|tz| tz.parse::<chrono_tz::Tz>().is_err())
    {
        return Err(ApiError::bad_request(
            "timezone is not a recognized IANA zone",
        ));
    }
    if payload.enabled
        && caller.digest_hour.is_none()
        && (payload.hour.is_none() || payload.timezone.is_none())
    {
        return Err(ApiError::bad_request(
            "hour and timezone are required the first time the digest is enabled",
        ));
    }

    let mut active: user::ActiveModel = caller.into();
    active.digest_enabled = Set(payload.enabled);
    active.digest_unsubscribe_token =
        Set(payload.enabled.then(|| uuid::Uuid::new_v4().to_string()));
    if let Some(h) = payload.hour {
        active.digest_hour = Set(Some(h));
    }
    if let Some(tz) = payload.timezone {
        active.digest_timezone = Set(Some(tz));
    }

    let updated = active.update(&db).await?;

    Ok(Json(DigestResponse {
        enabled: updated.digest_enabled,
        hour: updated.digest_hour,
        timezone: updated.digest_timezone,
    }))
}

#[derive(Debug, Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

/// `POST /api/users/me/password`: self-serve; requires an active session and the current password
/// (unlike the token-based `reset_password`).
///
/// A change bumps `session_token_version`, invalidating every session, so a fresh cookie is
/// reissued for the caller's own browser.
pub async fn change_password(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(caller): CurrentUser,
    headers: HeaderMap,
    Json(payload): Json<ChangePasswordRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let updated = user_service::change_password(
        &db,
        &caller,
        &payload.current_password,
        &payload.new_password,
    )
    .await?;

    let jar = session_jar(&state, &updated, updated.org_id, &headers)?;
    Ok((jar, StatusCode::NO_CONTENT))
}

#[derive(Debug, Deserialize)]
pub struct UnsubscribeQuery {
    #[serde(default)]
    pub token: String,
}

/// Literal markup: only the headline, body and `base_url` are substituted. Self-contained (inline
/// `<style>` + SVG).
fn render_unsubscribe_page(headline: &str, body: &str, base_url: &str) -> String {
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="robots" content="noindex">
  <title>arcodash — daily digest</title>
  <style>
    * {{ box-sizing: border-box; }}
    body {{
      margin: 0; min-height: 100vh;
      display: flex; align-items: center; justify-content: center;
      background: #ffffff; color: #0a0a0a;
      font: 16px/1.5 "Inter", ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
      -webkit-font-smoothing: antialiased;
      padding: 24px;
    }}
    .card {{
      width: 100%; max-width: 420px;
      border: 1px solid #e5e5e5; border-radius: 12px;
      padding: 32px; text-align: center;
    }}
    .mark {{ width: 40px; height: 40px; margin: 0 auto 20px; display: block; }}
    .wordmark {{ font-size: 18px; font-weight: 600; letter-spacing: 0.02em; margin: 0 0 24px; }}
    .wordmark .arc {{ color: #7e14ff; }}
    h1 {{ font-size: 18px; font-weight: 600; margin: 0 0 8px; }}
    p  {{ font-size: 14px; color: #737373; margin: 0 0 20px; }}
    a.btn {{
      display: inline-block; text-decoration: none;
      font-size: 14px; font-weight: 500;
      color: #fafafa; background: #171717;
      padding: 8px 16px; border-radius: 6px;
    }}
  </style>
</head>
<body>
  <main class="card">
    <svg class="mark" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48" aria-hidden="true">
      <rect width="48" height="48" rx="11" fill="#7e14ff"/>
      <path d="M11 36.5 A13 13 0 0 1 37 36.5" fill="none" stroke="#ffffff" stroke-width="6" stroke-linecap="round"/>
      <circle cx="24" cy="34" r="5" fill="#47bfff"/>
    </svg>
    <p class="wordmark"><span class="arc">arc</span>odash</p>
    <h1>{headline}</h1>
    <p>{body}</p>
    <a class="btn" href="{base_url}/settings/notifications">Go to arcodash</a>
  </main>
</body>
</html>
"##
    )
}

/// `GET /api/digest/unsubscribe?token=...`: public, per-IP rate-limited. Always 200 with the same
/// page shape whether the token is valid or not.
pub async fn unsubscribe_digest(
    db: Db,
    State(state): State<AppState>,
    Query(params): Query<UnsubscribeQuery>,
) -> impl IntoResponse {
    let found = if params.token.is_empty() {
        None
    } else {
        match user::Entity::find()
            .filter(user::Column::DigestUnsubscribeToken.eq(&params.token))
            .one(&db)
            .await
        {
            Ok(found) => found,
            Err(e) => {
                // Logged, not recorded as an event: an unauthenticated caller must not write
                // `events` rows by guessing tokens, and the identical page keeps guessing
                // unrewarding.
                tracing::error!(error = %e, "digest unsubscribe: token lookup failed");
                None
            }
        }
    };

    let unsubscribed = match found {
        Some(u) => {
            let (user_id, org_id) = (u.id, u.org_id);
            let mut active: user::ActiveModel = u.into();
            active.digest_enabled = Set(false);
            active.digest_unsubscribe_token = Set(None);
            if let Err(e) = active.update(&db).await {
                tracing::error!(error = %e, user_id, "digest unsubscribe: update failed");
                false
            } else {
                // The token proves it's this user, so `Some(user_id)` is a real actor. Success
                // path only, so failures can't be used to write `events` rows.
                if let Err(e) = record_event(
                    &db,
                    org_id,
                    Some(user_id),
                    "user.digest_unsubscribed",
                    Some("user"),
                    Some(user_id),
                    serde_json::json!({}),
                )
                .await
                {
                    tracing::error!(error = %e, user_id, "digest unsubscribe: failed to record audit event");
                }
                true
            }
        }
        None => false,
    };

    let (headline, body) = if unsubscribed {
        (
            "You're unsubscribed",
            "You won't get the daily digest anymore. You can turn it back on any time in Settings → Notifications.",
        )
    } else {
        (
            "This link is no longer valid",
            "If you meant to unsubscribe, open Settings → Notifications in arcodash and turn the daily digest off.",
        )
    };
    (
        StatusCode::OK,
        Html(render_unsubscribe_page(
            headline,
            body,
            &state.config.base_url,
        )),
    )
}
