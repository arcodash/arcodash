//! Resolves the current organization once per request and inserts `CurrentOrg` into request
//! extensions for downstream handlers.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::auth::session::SESSION_COOKIE_NAME;
use crate::auth::{resolve_org, CurrentOrg};
use crate::db::{PostCommitJobs, RequestTxn, RequestTxnSlot};
use crate::error::ApiError;
use crate::state::AppState;

/// State for [`inject_current_org`]'s `from_fn_with_state` layer (it takes one state value):
/// `AppState` plus the extra org-exempt path list.
#[derive(Clone)]
pub struct OrgMiddlewareState {
    pub app_state: AppState,
    pub org_exempt_paths: Arc<Vec<&'static str>>,
    pub org_optional_paths: Arc<Vec<&'static str>>,
}

/// Paths always exempt from org resolution. `/health`, `/ping` and `/metrics` are org-agnostic, and
/// a host-based `OrgResolver` would 404 probes whose `Host` isn't a tenant. Mirrors
/// `middleware::csrf::CSRF_EXEMPT_PATHS`.
const ALWAYS_EXEMPT_PATHS: &[&str] = &["/health", "/ping", "/metrics"];

/// Whether `path` skips org resolution: always-exempt ([`ALWAYS_EXEMPT_PATHS`]) or in the
/// caller-supplied `exempt_paths`. A plain path check, easily unit-tested.
fn should_skip_org_resolution(path: &str, exempt_paths: &[&str]) -> bool {
    ALWAYS_EXEMPT_PATHS.contains(&path) || exempt_paths.contains(&path)
}

/// Whether a resolution failure for `path` degrades to "no org" instead of an error (see
/// [`inject_current_org`]). Unlike [`should_skip_org_resolution`], resolution is still attempted
/// and gets `CurrentOrg` + RLS on success.
fn is_org_optional(path: &str, org_optional_paths: &[&str]) -> bool {
    org_optional_paths.contains(&path)
}

/// The bearer token from a public embed/dashboard read path, if `path` is one. These dynamic routes
/// can't be listed in `org_optional_paths` and must not degrade to "no org": their handlers need a
/// real `CurrentOrg`. Keep in step with the routes in `api::router`.
fn public_embed_token(path: &str) -> Option<&str> {
    let token = path
        .strip_prefix("/api/embed/")
        .or_else(|| path.strip_prefix("/api/dashboards/public/"))?;
    (!token.is_empty() && !token.contains('/')).then_some(token)
}

/// Routes that establish a new identity/org context. A leftover session cookie from another org
/// must not shadow it, so `resolve_session_cookie` strips it before org resolution.
const HOST_PRIORITY_PATHS: &[&str] = &["/api/login"];

fn should_ignore_session_cookie(path: &str) -> bool {
    HOST_PRIORITY_PATHS.contains(&path)
}

/// Removes `SESSION_COOKIE_NAME` from the `Cookie` header, keeping others (e.g. CSRF). No-op if
/// absent.
fn strip_session_cookie(headers: &mut HeaderMap) {
    let Some(raw) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) else {
        return;
    };

    let pairs: Vec<&str> = raw.split(';').map(str::trim).collect();
    let kept: Vec<&str> = pairs
        .iter()
        .copied()
        .filter(|pair| pair.split_once('=').map(|(k, _)| k) != Some(SESSION_COOKIE_NAME))
        .collect();
    if kept.len() == pairs.len() {
        return;
    }

    if kept.is_empty() {
        headers.remove(header::COOKIE);
    } else if let Ok(value) = HeaderValue::from_str(&kept.join("; ")) {
        headers.insert(header::COOKIE, value);
    }
}

/// `axum::middleware::from_fn` handler: resolves [`CurrentOrg`] into `request.extensions()`. Skips
/// resolution for `org_exempt_paths` (empty for the OSS binary).
pub async fn inject_current_org(
    State(mw_state): State<OrgMiddlewareState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if should_skip_org_resolution(request.uri().path(), &mw_state.org_exempt_paths) {
        return Ok(next.run(request).await);
    }

    let (mut parts, body) = request.into_parts();
    if should_ignore_session_cookie(parts.uri.path()) {
        strip_session_cookie(&mut parts.headers);
    }

    // A public link's token is the only thing naming its org; a visitor's session must never shadow
    // it, so this runs first.
    let token_org = match public_embed_token(parts.uri.path()) {
        Some(token) => {
            crate::services::embed_service::org_for_public_token(&mw_state.app_state, token).await?
        }
        None => None,
    };

    let org = match token_org {
        Some(org) => org,
        None => match resolve_org(&mw_state.app_state, &parts).await {
            Ok(org) => org,
            // No tenant for an org-optional route: continue with no `CurrentOrg` or RLS
            // transaction rather than 404.
            Err(err)
                if err.status == StatusCode::NOT_FOUND
                    && is_org_optional(parts.uri.path(), &mw_state.org_optional_paths) =>
            {
                return Ok(next.run(Request::from_parts(parts, body)).await);
            }
            Err(err) => return Err(err),
        },
    };
    parts.extensions.insert(CurrentOrg(org.clone()));

    if let Some(gate) = &mw_state.app_state.org_access_check {
        gate.check(&org, parts.uri.path())?;
    }

    // No hook in the OSS binary, but real once a composition binary wires one up.
    let Some(hook) = &mw_state.app_state.request_txn_hook else {
        return Ok(next.run(Request::from_parts(parts, body)).await);
    };

    let txn = hook.begin(&mw_state.app_state.db, &org).await?;
    let slot = RequestTxnSlot::new(Arc::new(txn));
    let after_commit = PostCommitJobs::default();
    parts
        .extensions
        .insert(RequestTxn(slot.clone(), after_commit.clone()));
    let response = next.run(Request::from_parts(parts, body)).await;

    // A handler may have already ended and committed the per-request transaction
    // (`db::RequestScope::detach`); then there is nothing to do.
    let Some(txn) = slot.take() else {
        return Ok(response);
    };

    // Commit regardless of response status, matching autocommit semantics (an audit row written
    // before an error response still lands).
    let Ok(txn) = Arc::try_unwrap(txn) else {
        // Something kept the `Arc<DatabaseTransaction>` past the request; dropping it would
        // silently roll back or leak, so fail loudly.
        tracing::error!(
            "per-request transaction was still referenced after the handler returned; \
             refusing to drop it silently"
        );
        return Err(ApiError::internal("internal server error"));
    };
    txn.commit().await.map_err(|e| {
        tracing::error!(error = %e, "failed to commit per-request transaction");
        ApiError::internal("internal server error")
    })?;
    // Only after the commit: work queued via `Db::after_commit` must never see writes before
    // they're visible.
    after_commit.run_all().await;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exempt_path_is_skipped() {
        assert!(should_skip_org_resolution(
            "/api/admin/orgs",
            &["/api/admin/orgs", "/api/admin/orgs/disable"]
        ));
    }

    #[test]
    fn non_exempt_path_is_not_skipped() {
        assert!(!should_skip_org_resolution(
            "/api/queries",
            &["/api/admin/orgs"]
        ));
    }

    #[test]
    fn empty_exempt_list_skips_nothing() {
        assert!(!should_skip_org_resolution("/api/admin/orgs", &[]));
    }

    #[test]
    fn session_path_is_org_optional() {
        assert!(is_org_optional("/api/session", &["/api/session"]));
    }

    #[test]
    fn other_paths_are_not_org_optional() {
        assert!(!is_org_optional("/api/queries", &["/api/session"]));
    }

    #[test]
    fn empty_optional_list_treats_nothing_as_optional() {
        assert!(!is_org_optional("/api/session", &[]));
    }

    #[test]
    fn health_is_always_skipped_regardless_of_exempt_list() {
        assert!(should_skip_org_resolution("/health", &[]));
        assert!(should_skip_org_resolution("/health", &["/api/admin/orgs"]));
    }

    #[test]
    fn ping_is_always_skipped_regardless_of_exempt_list() {
        assert!(should_skip_org_resolution("/ping", &[]));
        assert!(should_skip_org_resolution("/ping", &["/api/admin/orgs"]));
    }

    #[test]
    fn login_is_a_host_priority_path() {
        assert!(should_ignore_session_cookie("/api/login"));
    }

    #[test]
    fn other_routes_are_not_host_priority_paths() {
        assert!(!should_ignore_session_cookie("/api/queries"));
        assert!(!should_ignore_session_cookie("/api/session"));
    }

    fn headers_with_cookie(raw: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(raw).unwrap());
        headers
    }

    #[test]
    fn strip_session_cookie_removes_only_the_session_cookie() {
        let mut headers = headers_with_cookie("arcodash_session=stale; csrf_token=abc");
        strip_session_cookie(&mut headers);
        assert_eq!(
            headers.get(header::COOKIE).unwrap().to_str().unwrap(),
            "csrf_token=abc"
        );
    }

    #[test]
    fn strip_session_cookie_removes_header_entirely_when_it_was_the_only_cookie() {
        let mut headers = headers_with_cookie("arcodash_session=stale");
        strip_session_cookie(&mut headers);
        assert!(headers.get(header::COOKIE).is_none());
    }

    #[test]
    fn strip_session_cookie_is_a_noop_without_a_session_cookie() {
        let mut headers = headers_with_cookie("csrf_token=abc");
        strip_session_cookie(&mut headers);
        assert_eq!(
            headers.get(header::COOKIE).unwrap().to_str().unwrap(),
            "csrf_token=abc"
        );
    }

    #[test]
    fn strip_session_cookie_is_a_noop_without_any_cookie_header() {
        let mut headers = HeaderMap::new();
        strip_session_cookie(&mut headers);
        assert!(headers.get(header::COOKIE).is_none());
    }

    #[test]
    fn public_embed_token_extracts_from_both_routes() {
        assert_eq!(public_embed_token("/api/embed/abc"), Some("abc"));
        assert_eq!(
            public_embed_token("/api/dashboards/public/abc"),
            Some("abc")
        );
    }

    #[test]
    fn public_embed_token_rejects_non_token_paths() {
        assert_eq!(public_embed_token("/api/embed"), None);
        assert_eq!(public_embed_token("/api/embed/"), None);
        assert_eq!(public_embed_token("/api/embeds"), None);
        assert_eq!(public_embed_token("/api/embeds/7"), None);
        assert_eq!(public_embed_token("/api/dashboards/public"), None);
        assert_eq!(public_embed_token("/api/embed/a/b"), None);
        assert_eq!(public_embed_token("/api/session"), None);
        assert_eq!(public_embed_token("/api/queries"), None);
    }
}
