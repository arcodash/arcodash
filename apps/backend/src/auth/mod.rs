//! Axum extractors for the request's organization and authenticated user.
//!
//! [`CurrentOrg`] (via `AppState.org_resolver`, else the seeded `default` org) and [`CurrentUser`]
//! (session cookie or API key, 401 otherwise), each with an `Optional*` variant yielding `None`
//! instead of erroring (for routes like `/api/session`).

pub mod api_key;
pub mod org_resolver;
pub mod password;
pub mod session;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum_extra::extract::CookieJar;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use arcodash_core::entities::{organization, user};

use crate::db::request_db;
use crate::error::ApiError;
use crate::state::AppState;

pub use org_resolver::{BoxFuture, OrgResolver};

/// The default single-org slug, seeded by the baseline migration.
pub const DEFAULT_ORG_SLUG: &str = "default";

/// The request's resolved organization, inserted into extensions by `crate::middleware::org` and
/// usable as an extractor.
#[derive(Debug, Clone)]
pub struct CurrentOrg(pub organization::Model);

impl<S> FromRequestParts<S> for CurrentOrg
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        // Prefer the org already resolved by the org middleware (avoids a second query).
        if let Some(org) = parts.extensions.get::<Self>() {
            return Ok(org.clone());
        }

        resolve_org(&AppState::from_ref(state), parts)
            .await
            .map(CurrentOrg)
    }
}

/// Like [`CurrentOrg`], but yields `None` when the org middleware left no extension, for
/// org-optional routes like `GET /api/session`.
///
/// Deliberately does not call `resolve_org` itself: that would reproduce the 404 the middleware
/// swallowed.
#[derive(Debug, Clone)]
pub struct OptionalCurrentOrg(pub Option<organization::Model>);

impl<S> FromRequestParts<S> for OptionalCurrentOrg
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    #[allow(
        clippy::unused_async_trait_impl,
        reason = "keeps the async-fn form every extractor impl here uses; the body just has nothing to await"
    )]
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts.extensions.get::<CurrentOrg>().map(|o| o.0.clone()),
        ))
    }
}

/// Resolve the organization for this request via `state.org_resolver` if injected, else the seeded
/// `default` org.
pub async fn resolve_org(state: &AppState, parts: &Parts) -> Result<organization::Model, ApiError> {
    if let Some(resolver) = &state.org_resolver {
        return resolver.resolve(&state.db, parts).await;
    }

    organization::Entity::find()
        .filter(organization::Column::Slug.eq(DEFAULT_ORG_SLUG))
        .one(&state.db)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("organization '{DEFAULT_ORG_SLUG}' not found")))
}

/// Like [`resolve_org`], but a 404 (no org resolves) becomes `Ok(None)` so identity paths (API key,
/// remote-user header) fail closed to "no user"; other errors propagate.
async fn resolve_org_or_none(
    state: &AppState,
    parts: &Parts,
) -> Result<Option<organization::Model>, ApiError> {
    match resolve_org(state, parts).await {
        Ok(org) => Ok(Some(org)),
        Err(err) if err.status == StatusCode::NOT_FOUND => Ok(None),
        Err(err) => Err(err),
    }
}

/// The authenticated user for this request, resolved from the session cookie (first) or an API key
/// (`Authorization: Key <key>` header / `?api_key=`).
#[derive(Debug, Clone)]
pub struct CurrentUser(pub user::Model);

impl<S> FromRequestParts<S> for CurrentUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        resolve_and_cache_current_user(parts, &AppState::from_ref(state))
            .await?
            .map(CurrentUser)
            .ok_or_else(|| ApiError::unauthorized("not authenticated"))
    }
}

/// Like [`CurrentUser`], but yields `None` instead of rejecting when nothing resolves.
#[derive(Debug, Clone)]
pub struct OptionalCurrentUser(pub Option<user::Model>);

impl<S> FromRequestParts<S> for OptionalCurrentUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        resolve_and_cache_current_user(parts, &AppState::from_ref(state))
            .await
            .map(OptionalCurrentUser)
    }
}

/// Caches the resolved user in request extensions: user auth is per-route (no middleware populates
/// it), so the first extractor to run stores the result and later ones read it back instead of
/// re-querying the session cookie/API key/remote-user header (which can itself be a DB write).
async fn resolve_and_cache_current_user(
    parts: &mut Parts,
    state: &AppState,
) -> Result<Option<user::Model>, ApiError> {
    if let Some(CurrentUser(user)) = parts.extensions.get::<CurrentUser>() {
        return Ok(Some(user.clone()));
    }
    let user = resolve_current_user(parts, state).await?;
    if let Some(user) = &user {
        parts.extensions.insert(CurrentUser(user.clone()));
    }
    Ok(user)
}

/// Session cookie first, then API key, then remote-user header; `Ok(None)` when nothing resolves so
/// callers layer their own error-vs-`None` behavior.
async fn resolve_current_user(
    parts: &Parts,
    state: &AppState,
) -> Result<Option<user::Model>, ApiError> {
    if let Some(user) = resolve_from_session_cookie(parts, state).await? {
        return Ok(Some(user));
    }
    if let Some(user) = resolve_from_api_key(parts, state).await? {
        return Ok(Some(user));
    }
    resolve_from_remote_user_header(parts, state).await
}

/// Remote-user header auth: if `REMOTE_USER_HEADER` is configured and present, treat its value as the authenticated email and auto-provision a user.
/// **Only safe behind a trusted reverse proxy that strips the header from untrusted requests.**
async fn resolve_from_remote_user_header(
    parts: &Parts,
    state: &AppState,
) -> Result<Option<user::Model>, ApiError> {
    let Some(header_name) = &state.config.remote_user_header else {
        return Ok(None);
    };

    let Some(email) = parts
        .headers
        .get(header_name.as_str())
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|e| !e.is_empty())
    else {
        return Ok(None);
    };

    let Some(org) = resolve_org_or_none(state, parts).await? else {
        return Ok(None);
    };
    let db = request_db(parts, state);
    let user =
        crate::services::user_service::find_or_create_sso_user(&db, org.id, email, email, None)
            .await?;
    Ok(Some(user))
}

async fn resolve_from_session_cookie(
    parts: &Parts,
    state: &AppState,
) -> Result<Option<user::Model>, ApiError> {
    let jar = CookieJar::from_headers(&parts.headers);
    let Some(cookie) = jar.get(session::SESSION_COOKIE_NAME) else {
        return Ok(None);
    };

    let Ok(claims) = session::verify_token(
        &state.config.cookie_secret,
        cookie.value(),
        state.session_provider(),
    ) else {
        // Invalid/expired/tampered/wrong-provider: treat as logged out (`session::verify_token`
        // logs the provider mismatch).
        tracing::warn!("rejecting session cookie: failed to verify");
        crate::metrics::record_auth_outcome("session_cookie", "bad_credentials");
        return Ok(None);
    };

    let db = request_db(parts, state);
    let user = user::Entity::find_by_id(claims.user_id).one(&db).await?;

    Ok(user.filter(|u| {
        if u.org_id != claims.org_id {
            tracing::warn!(
                user_id = u.id,
                org_id = u.org_id,
                claim_org_id = claims.org_id,
                "rejecting session cookie: org mismatch"
            );
            crate::metrics::record_auth_outcome("session_cookie", "cross_org");
            return false;
        }
        if u.disabled_at.is_some() {
            tracing::warn!(
                user_id = u.id,
                org_id = u.org_id,
                "rejecting session cookie: user disabled"
            );
            crate::metrics::record_auth_outcome("session_cookie", "disabled");
            return false;
        }
        if u.is_invitation_pending {
            tracing::warn!(
                user_id = u.id,
                org_id = u.org_id,
                "rejecting session cookie: invitation still pending"
            );
            crate::metrics::record_auth_outcome("session_cookie", "invitation_pending");
            return false;
        }
        // A stale `ver` means a password/reset/logout bump happened after this cookie was issued:
        // treat as logged out.
        if i64::from(u.session_token_version) != claims.ver {
            tracing::warn!(
                user_id = u.id,
                org_id = u.org_id,
                "rejecting session cookie: stale session_token_version"
            );
            crate::metrics::record_auth_outcome("session_cookie", "stale_version");
            return false;
        }
        crate::metrics::record_auth_outcome("session_cookie", "success");
        true
    }))
}

async fn resolve_from_api_key(
    parts: &Parts,
    state: &AppState,
) -> Result<Option<user::Model>, ApiError> {
    let Some(key) = api_key::extract_api_key(&parts.headers, parts.uri.query()) else {
        return Ok(None);
    };

    // An API key carries no org context (both lookups are global), so reconcile against the
    // request's resolved org: otherwise org A's key plus a request resolving to org B (e.g. a
    // Host-subdomain fallback) would let A's key holder act against B's data. No org at all fails
    // closed to no user.
    let Some(org) = resolve_org_or_none(state, parts).await? else {
        return Ok(None);
    };
    let db = request_db(parts, state);

    // First try the `api_keys` table, then fall back to the per-user `users.api_key_hash` column.
    // A disabled or invitation-pending user must not authenticate via either path.
    let user = match api_key::resolve_user_for_api_key(&db, &key).await? {
        Some(user) => Some(user),
        None => {
            user::Entity::find()
                .filter(user::Column::ApiKeyHash.eq(arcodash_core::hash::hash_api_key(&key)))
                .one(&db)
                .await?
        }
    };

    Ok(user.filter(|u| {
        if u.org_id != org.id {
            tracing::warn!(
                user_id = u.id,
                user_org_id = u.org_id,
                request_org_id = org.id,
                "rejecting api key: cross-org"
            );
            crate::metrics::record_auth_outcome("api_key", "cross_org");
            return false;
        }
        if u.disabled_at.is_some() || u.is_invitation_pending {
            tracing::warn!(
                user_id = u.id,
                org_id = u.org_id,
                "rejecting api key: user disabled or invitation pending"
            );
            crate::metrics::record_auth_outcome("api_key", "disabled");
            return false;
        }
        crate::metrics::record_auth_outcome("api_key", "success");
        true
    }))
}
