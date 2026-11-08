//! JWT-backed session cookie issue/verify.
//!
//! The cookie carries `{ user_id, org_id, exp }` signed HS256 with `Config::cookie_secret`, set as
//! an httpOnly, `SameSite=Lax` cookie (`Secure` when the request isn't to localhost/dev — see
//! [`secure_cookie_attribute`]).

use anyhow::{Context, Result};
use axum_extra::extract::cookie::{Cookie, SameSite};
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

/// `provider` claim of sessions minted by the built-in login.
pub const INTERNAL_PROVIDER: &str = "internal";

/// Name of the session cookie.
pub const SESSION_COOKIE_NAME: &str = "arcodash_session";

/// Session lifetime: 14 days.
const SESSION_TTL_DAYS: i64 = 14;

/// JWT claims carried by the session cookie.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClaims {
    pub user_id: i64,
    pub org_id: i64,
    pub exp: i64,
    /// The `users.session_token_version` this token was issued under — checked against the
    /// row's current value on every request (`auth::resolve_from_session_cookie`) so a
    /// password/reset change invalidates every previously-issued cookie at once.
    pub ver: i64,
    /// The login system this session was established under. Checked against the running
    /// deployment's own provider on every request, so a cookie minted by a differently
    /// configured deployment is rejected even if the signing secret matches. `None` is a
    /// pre-claim token and is always rejected.
    #[serde(default)]
    pub provider: Option<String>,
}

/// Issue a signed session JWT for `user_id`/`org_id`, expiring `SESSION_TTL_DAYS` from now.
///
/// `session_token_version` should be the user row's current `session_token_version` at issuance
/// time (`user.session_token_version as i64`) — passing a stale value would mint a token that
/// fails verification on its very next use.
pub fn issue_token(
    cookie_secret: &str,
    user_id: i64,
    org_id: i64,
    session_token_version: i64,
    provider: &str,
) -> Result<String> {
    let exp = (Utc::now() + Duration::days(SESSION_TTL_DAYS)).timestamp();
    let claims = SessionClaims {
        user_id,
        org_id,
        exp,
        ver: session_token_version,
        provider: Some(provider.to_owned()),
    };
    encode(
        &Header::default(), // HS256
        &claims,
        &EncodingKey::from_secret(cookie_secret.as_bytes()),
    )
    .context("failed to sign session JWT")
}

/// Verify and decode a session JWT.
///
/// Rejects expired/invalid/tampered tokens, or one minted under a different provider than
/// `expected_provider` (including a pre-claim legacy token, whose `provider` decodes to `None`).
pub fn verify_token(
    cookie_secret: &str,
    token: &str,
    expected_provider: &str,
) -> Result<SessionClaims> {
    let validation = Validation::new(jsonwebtoken::Algorithm::HS256);
    let data = decode::<SessionClaims>(
        token,
        &DecodingKey::from_secret(cookie_secret.as_bytes()),
        &validation,
    )
    .context("invalid or expired session token")?;

    // A cookie minted under a different provider — or before this claim existed —
    // is not this deployment's session, even when the signature verifies.
    if data.claims.provider.as_deref() != Some(expected_provider) {
        tracing::warn!(
            claim = ?data.claims.provider,
            expected = ?expected_provider,
            "rejecting session cookie: auth provider claim mismatch"
        );
        anyhow::bail!("session token was issued under a different auth provider");
    }
    Ok(data.claims)
}

/// Build the `Set-Cookie` cookie for a freshly issued session.
///
/// `Secure` is set whenever the request host is not localhost/dev, since this is a real auth cookie
/// and must not be downgraded to plain HTTP in any real deployment.
#[must_use]
pub fn build_session_cookie(token: String, secure: bool) -> Cookie<'static> {
    session_shaped_cookie(SESSION_COOKIE_NAME, token, secure)
}

/// Build the CSRF double-submit cookie (`csrf_token`). HttpOnly=false so JS can read it;
/// SameSite=Strict so it's only sent on same-origin requests.
#[must_use]
pub fn build_csrf_cookie(token: String, secure: bool) -> Cookie<'static> {
    let mut cookie = session_shaped_cookie("csrf_token", token, secure);
    cookie.set_http_only(false);
    cookie.set_same_site(SameSite::Strict);
    cookie
}

/// Build the cookie used to clear a session on logout (empty value, immediately expired).
#[must_use]
pub fn build_logout_cookie(secure: bool) -> Cookie<'static> {
    let mut cookie = session_shaped_cookie(SESSION_COOKIE_NAME, String::new(), secure);
    cookie.set_max_age(Some(time::Duration::seconds(0)));
    cookie
}

/// `HttpOnly`, `SameSite=Lax`, path `/`, `SESSION_TTL_DAYS` lifetime.
fn session_shaped_cookie(name: &'static str, value: String, secure: bool) -> Cookie<'static> {
    Cookie::build((name, value))
        .http_only(true)
        .same_site(SameSite::Lax)
        .path("/")
        .secure(secure)
        .max_age(time::Duration::days(SESSION_TTL_DAYS))
        .build()
}

/// Decide the `Secure` cookie attribute for a given request `Host` header value.
///
/// `Secure` is off only for localhost/127.0.0.1/dev hosts so local HTTP development keeps working;
/// any other host (real deployments) always gets `Secure` — this must never be hardcoded off.
#[must_use]
pub fn secure_cookie_attribute(host: Option<&str>) -> bool {
    !host.is_some_and(|h| {
        // A bracketed IPv6 host (`[::1]:3000`) has its own colons, so it must be unwrapped
        // before splitting off the port — otherwise splitting on ':' first yields "[".
        let host_only = h.strip_prefix('[').map_or_else(
            || h.split(':').next().unwrap_or(h),
            |rest| rest.split(']').next().unwrap_or(rest),
        );
        matches!(host_only, "localhost" | "127.0.0.1" | "::1")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_then_verify_round_trips() {
        let token = issue_token("test-secret", 42, 7, 0, INTERNAL_PROVIDER).unwrap();
        let claims = verify_token("test-secret", &token, INTERNAL_PROVIDER).unwrap();
        assert_eq!(claims.user_id, 42);
        assert_eq!(claims.org_id, 7);
        assert_eq!(claims.ver, 0);
    }

    #[test]
    fn issue_then_verify_round_trips_a_nonzero_version() {
        let token = issue_token("test-secret", 42, 7, 3, INTERNAL_PROVIDER).unwrap();
        let claims = verify_token("test-secret", &token, INTERNAL_PROVIDER).unwrap();
        assert_eq!(claims.ver, 3);
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let token = issue_token("test-secret", 42, 7, 0, INTERNAL_PROVIDER).unwrap();
        assert!(verify_token("a-different-secret", &token, INTERNAL_PROVIDER).is_err());
    }

    #[test]
    fn verify_rejects_garbage_token() {
        assert!(verify_token("test-secret", "not-a-jwt", INTERNAL_PROVIDER).is_err());
    }

    #[test]
    fn verify_rejects_expired_token() {
        let claims = SessionClaims {
            user_id: 1,
            org_id: 1,
            exp: (Utc::now() - Duration::days(1)).timestamp(),
            ver: 0,
            provider: Some(INTERNAL_PROVIDER.to_owned()),
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(b"test-secret"),
        )
        .unwrap();
        assert!(verify_token("test-secret", &token, INTERNAL_PROVIDER).is_err());
    }

    #[test]
    fn secure_attribute_off_for_localhost_only() {
        assert!(!secure_cookie_attribute(Some("localhost")));
        assert!(!secure_cookie_attribute(Some("localhost:8080")));
        assert!(!secure_cookie_attribute(Some("127.0.0.1")));
        assert!(!secure_cookie_attribute(Some("127.0.0.1:8080")));
        assert!(secure_cookie_attribute(Some("app.example.com")));
        assert!(secure_cookie_attribute(Some("example.com:443")));
        assert!(secure_cookie_attribute(None));
    }

    #[test]
    fn session_cookie_is_http_only_and_same_site_lax() {
        let cookie = build_session_cookie("token".to_string(), true);
        assert_eq!(cookie.http_only(), Some(true));
        assert_eq!(cookie.same_site(), Some(SameSite::Lax));
        assert_eq!(cookie.secure(), Some(true));
    }

    #[test]
    fn csrf_cookie_is_not_http_only_and_same_site_strict() {
        let cookie = build_csrf_cookie("csrf-value".to_string(), true);
        assert_eq!(cookie.http_only(), Some(false));
        assert_eq!(cookie.same_site(), Some(SameSite::Strict));
        assert_eq!(cookie.name(), "csrf_token");
        assert_eq!(cookie.value(), "csrf-value");
    }

    #[test]
    fn logout_cookie_has_empty_value_and_zero_max_age() {
        let cookie = build_logout_cookie(false);
        assert_eq!(cookie.value(), "");
        assert_eq!(cookie.max_age(), Some(time::Duration::seconds(0)));
        assert_eq!(cookie.name(), SESSION_COOKIE_NAME);
    }

    #[test]
    fn secure_attribute_off_for_ipv6_loopback() {
        assert!(!secure_cookie_attribute(Some("[::1]")));
        assert!(!secure_cookie_attribute(Some("[::1]:3000")));
    }

    #[test]
    fn rotated_secret_invalidates_token() {
        let token = issue_token("secret-v1", 1, 1, 0, INTERNAL_PROVIDER).unwrap();
        assert!(verify_token("secret-v2", &token, INTERNAL_PROVIDER).is_err());
    }

    #[test]
    fn token_expired_far_in_the_past_is_rejected() {
        // jsonwebtoken has a built-in ~60s leeway; use a token well past that.
        let claims = SessionClaims {
            user_id: 5,
            org_id: 2,
            exp: (Utc::now() - Duration::minutes(5)).timestamp(),
            ver: 0,
            provider: Some(INTERNAL_PROVIDER.to_owned()),
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(b"test-secret"),
        )
        .unwrap();
        assert!(verify_token("test-secret", &token, INTERNAL_PROVIDER).is_err());
    }

    #[test]
    fn issue_token_round_trips_org_id() {
        let token = issue_token("s3cr3t", 99, 42, 0, INTERNAL_PROVIDER).unwrap();
        let claims = verify_token("s3cr3t", &token, INTERNAL_PROVIDER).unwrap();
        assert_eq!(claims.user_id, 99);
        assert_eq!(claims.org_id, 42);
    }

    #[test]
    fn verify_rejects_a_token_from_the_other_provider() {
        let internal = issue_token("s", 1, 1, 0, INTERNAL_PROVIDER).unwrap();
        assert!(verify_token("s", &internal, "other").is_err());

        let other = issue_token("s", 1, 1, 0, "other").unwrap();
        assert!(verify_token("s", &other, INTERNAL_PROVIDER).is_err());
    }

    #[test]
    fn verify_accepts_a_matching_provider() {
        for p in [INTERNAL_PROVIDER, "other"] {
            let t = issue_token("s", 1, 1, 0, p).unwrap();
            assert_eq!(
                verify_token("s", &t, p).unwrap().provider.as_deref(),
                Some(p)
            );
        }
    }

    #[test]
    fn verify_rejects_a_pre_claim_legacy_token() {
        // A token minted before `provider` existed: the claim is simply absent from the JSON.
        let legacy = encode(
            &Header::default(),
            &serde_json::json!({
                "user_id": 1, "org_id": 1,
                "exp": (Utc::now() + Duration::days(1)).timestamp(), "ver": 0
            }),
            &EncodingKey::from_secret(b"s"),
        )
        .unwrap();
        assert!(verify_token("s", &legacy, INTERNAL_PROVIDER).is_err());
        assert!(verify_token("s", &legacy, "other").is_err());
    }
}
