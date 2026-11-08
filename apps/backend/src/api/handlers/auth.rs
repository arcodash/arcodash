//! `POST /api/login`, `POST /api/logout`, `GET /api/session`.

use std::net::SocketAddr;

use crate::extract::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum_extra::extract::CookieJar;
use serde::{Deserialize, Serialize};
use serde_json::json;

use arcodash_core::entities::{organization, user};

use crate::auth::password::{verify_dummy_password, verify_password};
use crate::auth::session::{
    build_csrf_cookie, build_logout_cookie, build_session_cookie, issue_token,
    secure_cookie_attribute,
};
use crate::auth::{CurrentOrg, OptionalCurrentOrg, OptionalCurrentUser};
use crate::db::Db;
use crate::error::ApiError;
use crate::middleware::events::record_event;
use crate::state::AppState;

/// Generic message for any login failure, identical for unknown email, wrong password and disabled
/// account so the response never leaks which.
const GENERIC_LOGIN_FAILURE_MESSAGE: &str = "Invalid email or password";

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct UserResponse {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub groups: Vec<i64>,
    /// Rides the session payload so the frontend sees the daily-digest opt-in without a separate
    /// GET.
    pub digest_enabled: bool,
    /// `None` until the digest has been enabled once; lets the Notifications page tell a first-time
    /// enable from a returning one.
    pub digest_hour: Option<i16>,
    pub digest_timezone: Option<String>,
}

impl From<&user::Model> for UserResponse {
    fn from(user: &user::Model) -> Self {
        Self {
            id: user.id,
            name: user.name.clone(),
            email: user.email.clone(),
            groups: user.groups.clone(),
            digest_enabled: user.digest_enabled,
            digest_hour: user.digest_hour,
            digest_timezone: user.digest_timezone.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct OrgResponse {
    pub id: i64,
    pub name: String,
    pub slug: String,
}

impl From<&organization::Model> for OrgResponse {
    fn from(org: &organization::Model) -> Self {
        Self {
            id: org.id,
            name: org.name.clone(),
            slug: org.slug.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "response DTO; each bool is an independent flag in the JSON wire format"
)]
pub struct ClientConfigResponse {
    pub disable_public_urls: bool,
    /// Whether each SSO method is configured server-side, driving which login buttons show
    /// (`Config` is the source of truth).
    pub saml_enabled: bool,
    pub ldap_enabled: bool,
    /// Whether "Ask with AI" is available: a system-wide `GEMINI_API_KEY` or this org's own key.
    pub ai_sql_generation: bool,
    /// Whether the org is entitled to AI monitor suggestions (`OrgEntitlements::ai_features`),
    /// independent of provider configuration; the backend endpoint is authoritative.
    pub ai_monitor_suggestions: bool,
    /// Dashboard Explain is usable: a provider key is set, the org is entitled, and the org has
    /// narratives and sample values on.
    pub ai_dashboard_narratives: bool,
    /// `false` when an embedding deployment's `ExternalAuth` replaces the built-in login.
    pub password_auth_enabled: bool,
    /// `ExternalAuth::client_config`, flattened into the object.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct SessionResponse {
    pub user: Option<UserResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<OrgResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_config: Option<ClientConfigResponse>,
    /// The signed-in user's EXPANDED effective permission keys (a held `manage_query` yields its
    /// implied keys too), so the frontend `has()` needs no implies logic. `[]` when logged out.
    pub permissions: Vec<String>,
    /// `true` if the signed-in user holds `admin` (implies every permission). `false` when
    /// logged out.
    pub is_admin: bool,
}

/// `POST /api/login` — form body `{ email, password }`; issues the session cookie and returns the
/// user JSON.
///
/// Every failure (unknown email, wrong password, disabled account) is an indistinguishable generic
/// 401. Records an `events` row either way.
pub async fn login(
    State(state): State<AppState>,
    db: Db,
    CurrentOrg(org): CurrentOrg,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<LoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let user = crate::services::user_service::find_by_email_ci(&db, org.id, &payload.email).await?;

    let verified = if let Some(hash) = user.as_ref().and_then(|u| u.password_hash.as_deref()) {
        verify_password(&payload.password, hash)
            .await
            .unwrap_or(false)
    } else {
        // Unknown email or no password set (e.g. SSO-only): reject, but still pay Argon2's cost
        // so timing can't distinguish either from a wrong password.
        verify_dummy_password(&payload.password).await;
        false
    };

    // A pending invitation has a password_hash only if set out of band: reject it like a disabled
    // account, since it isn't live until accepted.
    let login_succeeded = verified
        && user
            .as_ref()
            .is_some_and(|u| u.disabled_at.is_none() && !u.is_invitation_pending);

    if !login_succeeded {
        let outcome = match &user {
            Some(u) if u.disabled_at.is_some() => "disabled",
            Some(u) if u.is_invitation_pending => "invitation_pending",
            _ => "bad_credentials",
        };
        crate::metrics::record_auth_outcome("password", outcome);
        // Bounded to RFC 5321's maximum path length: this is unauthenticated input bounded only by
        // axum's body limit.
        let attempted_email: String = payload.email.chars().take(320).collect();
        tracing::warn!(
            org_id = org.id,
            via = "internal",
            ip = %addr.ip(),
            attempted_email = %attempted_email,
            "rejecting login: invalid credentials"
        );
        record_event_or_warn(
            &db,
            org.id,
            user.as_ref().map(|u| u.id),
            "user.login_failed",
            None,
            json!({ "attempted_email": attempted_email, "ip": addr.ip().to_string() }),
        )
        .await;
        return Err(ApiError::unauthorized(GENERIC_LOGIN_FAILURE_MESSAGE));
    }

    let user = user.expect("login_succeeded implies user is Some");
    crate::metrics::record_auth_outcome("password", "success");

    record_event(
        &db,
        org.id,
        Some(user.id),
        "user.login",
        Some("user"),
        Some(user.id),
        json!({}),
    )
    .await?;

    let secure = secure_cookie_attribute(host_header(&headers));
    let jar = session_jar(&state, &user, org.id, &headers)?
        .add(build_csrf_cookie(uuid::Uuid::new_v4().to_string(), secure));
    Ok((jar, Json(UserResponse::from(&user))))
}

/// `POST /api/logout` — clears the session cookie and, if a valid session was presented, revokes it
/// server-side (bumps `session_token_version`, signing out every device).
///
/// `OptionalCurrentUser` keeps it idempotent: an expired cookie still gets a 204.
pub async fn logout(
    db: Db,
    OptionalCurrentUser(user): OptionalCurrentUser,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(u) = user {
        crate::services::user_service::revoke_sessions(&db, &u).await?;
        record_event_or_warn(
            &db,
            u.org_id,
            Some(u.id),
            "user.logout",
            Some(("user", u.id)),
            json!({}),
        )
        .await;
    }
    let jar = CookieJar::new().add(build_logout_cookie(secure_cookie_attribute(host_header(
        &headers,
    ))));
    Ok((jar, StatusCode::NO_CONTENT))
}

/// `GET /api/session` — `{ "user": null }` if logged out, else the user plus org and client config.
///
/// Uses the `Optional*` extractors since it bootstraps the login UI for visitors who may map to no
/// tenant. Also refreshes the CSRF cookie.
pub async fn session(
    OptionalCurrentOrg(org): OptionalCurrentOrg,
    OptionalCurrentUser(user): OptionalCurrentUser,
    State(state): State<AppState>,
    db: Db,
    headers: HeaderMap,
    jar: CookieJar,
) -> Result<impl IntoResponse, ApiError> {
    // No org resolved: `OptionalCurrentUser` already queried `users` on the plain pool, which an
    // RLS composition binary blocks; whatever it returned is discarded and never attached to a
    // tenant. Nothing past here queries an org-scoped table.
    let Some(org) = org else {
        let response = SessionResponse {
            user: None,
            org: None,
            client_config: Some(client_config(&state, false, false, false)),
            permissions: vec![],
            is_admin: false,
        };
        return Ok(session_response(&headers, &jar, response));
    };

    // A decrypt failure (e.g. a corrupted stored key) fails closed: the feature reports unavailable
    // instead of 500ing the session endpoint.
    let ai_sql_generation =
        crate::api::handlers::ai_settings::effective_ai_config(&org, &state.config)
            .is_ok_and(|c| c.api_key.is_some());
    let ai_features = state.entitlements.ai_features(&org);

    let (permissions, is_admin) = match &user {
        Some(u) => {
            let load_err =
                |e: sea_orm::DbErr| ApiError::internal(format!("failed to load permissions: {e}"));
            let groups = crate::authz::permissions::load_effective_groups(&db, u)
                .await
                .map_err(load_err)?;
            let is_admin = crate::authz::permissions::is_admin(&groups);
            let keys = crate::authz::permissions::effective_permission_keys(&db, u)
                .await
                .map_err(load_err)?;
            (
                arcodash_core::permissions_catalog::expand_permissions(&keys),
                is_admin,
            )
        }
        None => (vec![], false),
    };

    let response = SessionResponse {
        user: user.as_ref().map(UserResponse::from),
        org: Some(OrgResponse::from(&org)),
        client_config: Some(client_config(
            &state,
            ai_sql_generation,
            ai_features,
            ai_sql_generation
                && ai_features
                && crate::api::handlers::dashboard_narrative::narratives_enabled(&org.settings),
        )),
        permissions,
        is_admin,
    };

    Ok(session_response(&headers, &jar, response))
}

/// The [`ClientConfigResponse`] parts that come from server config regardless of org; the
/// org-dependent AI flags are `false` when no org resolves.
fn client_config(
    state: &AppState,
    ai_sql_generation: bool,
    ai_monitor_suggestions: bool,
    ai_dashboard_narratives: bool,
) -> ClientConfigResponse {
    ClientConfigResponse {
        disable_public_urls: state.config.disable_public_urls,
        saml_enabled: state.external_auth.is_none() && state.config.saml_idp_metadata_xml.is_some(),
        ldap_enabled: state.external_auth.is_none()
            && state.config.ldap_url.is_some()
            && state.config.ldap_search_base_dn.is_some(),
        ai_sql_generation,
        ai_monitor_suggestions,
        ai_dashboard_narratives,
        password_auth_enabled: state.external_auth.is_none(),
        extra: state
            .external_auth
            .as_ref()
            .map(|e| e.client_config.clone())
            .unwrap_or_default(),
    }
}

/// Builds the CSRF-cookie-bearing response every `/api/session` reply returns; `POST /api/login`
/// needs the token even when logged out.
///
/// Only mints a fresh `csrf_token` when the request didn't carry a valid-looking one: reissuing on
/// every call races in-flight mutations across tabs (the header and the new cookie stop matching,
/// so the request 403s).
fn session_response(
    headers: &HeaderMap,
    jar: &CookieJar,
    response: SessionResponse,
) -> (CookieJar, Json<SessionResponse>) {
    let secure = secure_cookie_attribute(host_header(headers));
    let has_valid_token = jar
        .get("csrf_token")
        .is_some_and(|c| uuid::Uuid::parse_str(c.value()).is_ok());
    let out_jar = if has_valid_token {
        CookieJar::new()
    } else {
        CookieJar::new().add(build_csrf_cookie(uuid::Uuid::new_v4().to_string(), secure))
    };
    (out_jar, Json(response))
}

pub(crate) fn host_header(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::HOST).and_then(|v| v.to_str().ok())
}

/// A jar carrying a freshly issued built-in-login session cookie for `user` in `org_id`.
pub(crate) fn session_jar(
    state: &AppState,
    user: &user::Model,
    org_id: i64,
    headers: &HeaderMap,
) -> Result<CookieJar, ApiError> {
    let token = issue_token(
        &state.config.cookie_secret,
        user.id,
        org_id,
        i64::from(user.session_token_version),
        crate::auth::session::INTERNAL_PROVIDER,
    )
    .map_err(|e| ApiError::internal(format!("failed to issue session: {e}")))?;
    let secure = secure_cookie_attribute(host_header(headers));
    Ok(CookieJar::new().add(build_session_cookie(token, secure)))
}

/// An SSO user's display name: "first last", whichever half exists, else the email.
pub(crate) fn sso_display_name(first: Option<String>, last: Option<String>, email: &str) -> String {
    match (first, last) {
        (Some(f), Some(l)) => format!("{f} {l}"),
        (Some(f), None) => f,
        (None, Some(l)) => l,
        (None, None) => email.to_string(),
    }
}

/// `record_event` for an audit row that must never fail the request: a write error is only
/// logged.
pub(crate) async fn record_event_or_warn(
    db: &impl sea_orm::ConnectionTrait,
    org_id: i64,
    user_id: Option<i64>,
    action: &str,
    object: Option<(&str, i64)>,
    details: serde_json::Value,
) {
    let (object_type, object_id) = object.unzip();
    if let Err(e) = record_event(db, org_id, user_id, action, object_type, object_id, details).await
    {
        tracing::warn!(error = %e, "failed to record {action} audit event");
    }
}
