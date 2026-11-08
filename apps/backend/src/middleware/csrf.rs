//! CSRF protection for cookie-authenticated, state-mutating endpoints.
//!
//! Uses the double-submit cookie pattern: a non-HttpOnly `csrf_token` cookie (SameSite=Strict) set
//! at session start must be echoed as the `X-CSRF-Token` header on every non-safe request.
//!
//! API-key-authenticated requests skip the check.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::auth::CurrentOrg;
use crate::error::ApiError;

const CSRF_COOKIE: &str = "csrf_token";
const CSRF_HEADER: &str = "X-CSRF-Token";

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get("Cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.split(';').find_map(|part| {
                let (k, v) = part.trim().split_once('=')?;
                (k == name).then(|| v.to_string())
            })
        })
}

/// Whether the request rides a session cookie. The auth path resolves it before an API key, so the
/// API-key CSRF bypass must not apply while a session cookie is present (else a junk `?api_key=`
/// would defeat CSRF).
fn has_session_cookie(headers: &HeaderMap) -> bool {
    cookie_value(headers, crate::auth::session::SESSION_COOKIE_NAME).is_some()
}

/// Paths unconditionally exempt from CSRF: pre-session logins (`/api/login`, `/api/ldap/login`;
/// worst case is low-severity login CSRF), `/api/logout` (revocation is harmless),
/// `/api/saml/acs` (cross-origin `IdP` POST, protected by XML signature + `InResponseTo` + one-time
/// `RelayState`), and the single-use-token endpoints `/api/users/invitation/accept`,
/// `/api/forgot_password` and `/api/reset_password`.
const CSRF_EXEMPT_PATHS: &[&str] = &[
    "/api/login",
    "/api/logout",
    "/api/ldap/login",
    "/api/users/invitation/accept",
    "/api/forgot_password",
    "/api/reset_password",
    "/api/saml/acs",
];

/// Whether the request bypasses CSRF: safe methods, exempt paths, or API-key auth (`Authorization:
/// Key <key>` or `?api_key=`, as [`crate::auth::api_key::extract_api_key`] parses them). Uses a
/// real query-param parse, not a substring check that would match `?not_api_key=x`.
fn should_bypass_csrf(
    method: &Method,
    headers: &HeaderMap,
    query: Option<&str>,
    path: &str,
    extra_exempt_paths: &[&str],
) -> bool {
    if matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    ) {
        return true;
    }
    if CSRF_EXEMPT_PATHS.contains(&path) || extra_exempt_paths.contains(&path) {
        return true;
    }
    // API-key auth is exempt only when no session cookie is present; otherwise the request
    // authenticates as the session and skipping CSRF would leave it open to forgery.
    !has_session_cookie(headers) && crate::auth::api_key::extract_api_key(headers, query).is_some()
}

/// Validate the double-submit cookie: cookie and `X-CSRF-Token` header must both be present,
/// non-empty and equal.
fn tokens_match(headers: &HeaderMap) -> bool {
    let Some(cookie) = nonempty_csrf_cookie(headers) else {
        return false;
    };
    headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok()) == Some(cookie.as_str())
}

fn nonempty_csrf_cookie(headers: &HeaderMap) -> Option<String> {
    cookie_value(headers, CSRF_COOKIE).filter(|c| !c.is_empty())
}

/// Why `tokens_match` failed, for the rejection log only.
fn csrf_failure_reason(headers: &HeaderMap) -> &'static str {
    if nonempty_csrf_cookie(headers).is_some() {
        "mismatch"
    } else {
        "missing_cookie"
    }
}

/// Validates the CSRF token on state-mutating requests. Skips safe methods,
/// `Authorization`/`api_key` requests, `CSRF_EXEMPT_PATHS`, and `extra_exempt_paths` (from
/// `AppExtensions::csrf_exempt_paths`).
pub async fn validate_csrf(
    State(extra_exempt_paths): State<Arc<Vec<&'static str>>>,
    req: Request,
    next: Next,
) -> Response {
    if should_bypass_csrf(
        req.method(),
        req.headers(),
        req.uri().query(),
        req.uri().path(),
        &extra_exempt_paths,
    ) || tokens_match(req.headers())
    {
        return next.run(req).await;
    }

    // `CurrentOrg` is available (org injection runs first) but `CurrentUser` is not. Never log the
    // query string.
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let org_id = req.extensions().get::<CurrentOrg>().map(|o| o.0.id);
    tracing::warn!(
        method = %req.method(),
        path = %req.uri().path(),
        ip = ?ip,
        org_id = ?org_id,
        reason = csrf_failure_reason(req.headers()),
        "rejecting request: CSRF token missing or mismatch"
    );
    ApiError::forbidden("CSRF token missing or mismatch").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn bypasses(method: Method, headers: &[(&str, &str)], query: Option<&str>, path: &str) -> bool {
        should_bypass_csrf(&method, &headers_with(headers), query, path, &[])
    }

    #[test]
    fn safe_methods_bypass_csrf() {
        for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(bypasses(method, &[], None, "/api/queries"));
        }
    }

    #[test]
    fn post_does_not_bypass_csrf_by_default() {
        assert!(!bypasses(Method::POST, &[], None, "/api/queries"));
    }

    #[test]
    fn api_key_auth_bypasses_csrf_on_post() {
        assert!(bypasses(
            Method::POST,
            &[("Authorization", "Key somekey")],
            None,
            "/api/queries"
        ));
        assert!(bypasses(
            Method::POST,
            &[],
            Some("api_key=abc"),
            "/api/queries"
        ));
    }

    #[test]
    fn api_key_does_not_bypass_csrf_when_a_session_cookie_is_present() {
        // A cookie-authenticated request authenticates as the session, so a junk `?api_key=` or
        // `Authorization: Key` must NOT skip CSRF.
        let session = ("Cookie", "arcodash_session=whatever");
        assert!(!bypasses(
            Method::POST,
            &[session],
            Some("api_key=abc"),
            "/api/queries"
        ));
        assert!(!bypasses(
            Method::POST,
            &[session, ("Authorization", "Key somekey")],
            None,
            "/api/queries"
        ));
    }

    #[test]
    fn lookalike_query_param_does_not_bypass_csrf() {
        // A naive `contains("api_key=")` would false-positive here; a real key=value parse is
        // required.
        assert!(!bypasses(
            Method::POST,
            &[],
            Some("not_api_key=abc"),
            "/api/queries"
        ));
    }

    #[test]
    fn login_logout_paths_bypass_csrf() {
        for path in ["/api/ldap/login", "/api/login", "/api/logout"] {
            assert!(bypasses(Method::POST, &[], None, path), "{path}");
        }
    }

    #[test]
    fn extra_exempt_path_bypasses_csrf() {
        // Simulates `AppExtensions::csrf_exempt_paths`: exempt only with the extra list.
        let path = "/api/oauth/google/callback";
        assert!(!should_bypass_csrf(
            &Method::POST,
            &HeaderMap::new(),
            None,
            path,
            &[]
        ));
        assert!(should_bypass_csrf(
            &Method::POST,
            &HeaderMap::new(),
            None,
            path,
            &[path]
        ));
    }

    #[test]
    fn matching_cookie_and_header_tokens_pass() {
        let h = headers_with(&[("Cookie", "csrf_token=tok123"), ("X-CSRF-Token", "tok123")]);
        assert!(tokens_match(&h));
    }

    #[test]
    fn mismatched_tokens_fail() {
        let h = headers_with(&[
            ("Cookie", "csrf_token=tok123"),
            ("X-CSRF-Token", "different"),
        ]);
        assert!(!tokens_match(&h));
    }

    #[test]
    fn missing_csrf_cookie_fails() {
        let h = headers_with(&[("X-CSRF-Token", "tok123")]);
        assert!(!tokens_match(&h));
    }

    #[test]
    fn missing_csrf_header_fails() {
        let h = headers_with(&[("Cookie", "csrf_token=tok123")]);
        assert!(!tokens_match(&h));
    }

    #[test]
    fn empty_cookie_value_fails() {
        let h = headers_with(&[("Cookie", "csrf_token="), ("X-CSRF-Token", "")]);
        assert!(!tokens_match(&h));
    }

    #[test]
    fn failure_reason_is_missing_cookie_when_no_cookie_is_present() {
        assert_eq!(csrf_failure_reason(&HeaderMap::new()), "missing_cookie");
        let h = headers_with(&[("Cookie", "csrf_token=")]);
        assert_eq!(csrf_failure_reason(&h), "missing_cookie");
    }

    #[test]
    fn failure_reason_is_mismatch_when_a_nonempty_cookie_is_present() {
        // Only distinguishes "no cookie at all" from "a cookie that didn't match".
        let h = headers_with(&[("Cookie", "csrf_token=tok123")]);
        assert_eq!(csrf_failure_reason(&h), "mismatch");
    }
}
