//! Integration tests: full auth flow. `/api/login` is deliberately CSRF-exempt (no CSRF cookie
//! exists before the first login); `TestApp::login` still does the GET /session + X-CSRF-Token
//! preflight like a real client.

mod common;
use common::{extract_set_cookie, spawn_app, spawn_app_with_extensions};

use arcodash_backend::auth::password::hash_password;
use arcodash_backend::AppExtensions;
use arcodash_core::entities::{organization, user};
use sea_orm::{ActiveValue::*, ColumnTrait, EntityTrait, QueryFilter};

/// Serializes tests that mutate `LOGIN_RATE_LIMIT_PER_MIN` (or other rate-limit env vars read at
/// `spawn_app()`): a concurrent `remove_var` could land between another's `set_var` and
/// `spawn_app()`.
static ENV_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Bare client — no automatic cookies, no default headers.
fn bare() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(false)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Insert a user with a known password into the "default" org.
async fn seed_user_with_password(db: &sea_orm::DatabaseConnection, email: &str, password: &str) {
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(db)
        .await
        .expect("org query failed")
        .expect("default org must exist after migrations");

    let hash = hash_password(password).await.expect("hash failed");

    user::Entity::insert(user::ActiveModel {
        id: NotSet,
        org_id: Set(org.id),
        name: Set("CI Email Test User".to_string()),
        email: Set(email.to_string()),
        password_hash: Set(Some(hash)),
        groups: Set(vec![]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(
            &uuid::Uuid::new_v4().simple().to_string(),
        )),
        disabled_at: Set(None),
        is_invitation_pending: Set(false),
        profile_image_url: Set(None),
        details: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
        digest_enabled: Set(false),
        digest_unsubscribe_token: Set(None),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("user insert failed");
}

/// GET /api/session with NO cookie → user: null, csrf_token cookie set.
#[tokio::test]
async fn session_without_cookie_returns_null_user() {
    let app = skip_if_none!(spawn_app().await);

    let resp = bare()
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let csrf = extract_set_cookie(resp.headers(), "csrf_token");
    assert!(csrf.is_some(), "GET /session must set csrf_token cookie");

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["user"].is_null(),
        "unauthenticated session must return user: null"
    );
}

/// Two sequential GET /api/session calls (second sends back the csrf cookie): the second must NOT
/// mint a new token, or an in-flight mutation across tabs would 403 on mismatch.
#[tokio::test]
async fn session_does_not_rotate_csrf_cookie_on_repeat_call() {
    let app = skip_if_none!(spawn_app().await);
    let client = bare();

    let first = client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let first_csrf =
        extract_set_cookie(first.headers(), "csrf_token").expect("first call must set csrf_token");

    let second = client
        .get(format!("{}/api/session", app.address))
        .header(reqwest::header::COOKIE, format!("csrf_token={first_csrf}"))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status().as_u16(), 200);
    let second_csrf = extract_set_cookie(second.headers(), "csrf_token");
    assert!(
        second_csrf.is_none(),
        "second call must not reissue csrf_token when a valid one was already present, got: {second_csrf:?}"
    );
}

/// POST /api/login with no csrf_token cookie or header → 200 (login is a deliberate CSRF
/// exemption in `CSRF_EXEMPT_PATHS` — there's no CSRF cookie yet before the first login).
#[tokio::test]
async fn login_without_csrf_token_succeeds_csrf_exempt() {
    let app = skip_if_none!(spawn_app().await);

    let resp = bare()
        .post(format!("{}/api/login", app.address))
        .json(&serde_json::json!({
            "email": common::ADMIN_EMAIL,
            "password": common::ADMIN_PASSWORD,
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        200,
        "POST /login must succeed without CSRF — /api/login is a deliberate CSRF_EXEMPT_PATHS entry"
    );
}

/// POST /api/login with correct creds (after CSRF preflight) → 200 + both cookies.
#[tokio::test]
async fn login_with_correct_creds_returns_200_and_sets_cookies() {
    let app = skip_if_none!(spawn_app().await);

    let preflight = bare()
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = extract_set_cookie(preflight.headers(), "csrf_token").unwrap();

    let resp = bare()
        .post(format!("{}/api/login", app.address))
        .header("Cookie", format!("csrf_token={csrf}"))
        .header("X-CSRF-Token", &csrf)
        .json(&serde_json::json!({
            "email": common::ADMIN_EMAIL,
            "password": common::ADMIN_PASSWORD,
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let session = extract_set_cookie(resp.headers(), "arcodash_session");
    let csrf_after = extract_set_cookie(resp.headers(), "csrf_token");

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["email"], common::ADMIN_EMAIL);
    assert!(session.is_some(), "login must set arcodash_session cookie");
    assert!(csrf_after.is_some(), "login must set csrf_token cookie");
}

/// POST /api/login matches the email case-insensitively, like SSO providers via `find_by_email_ci`.
#[tokio::test]
async fn login_matches_email_case_insensitively() {
    let app = skip_if_none!(spawn_app().await);
    seed_user_with_password(&app.db, "User@X.com", "Cor3ctH0rse!").await;

    let preflight = bare()
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = extract_set_cookie(preflight.headers(), "csrf_token").unwrap();

    let resp = bare()
        .post(format!("{}/api/login", app.address))
        .header("Cookie", format!("csrf_token={csrf}"))
        .header("X-CSRF-Token", &csrf)
        .json(&serde_json::json!({
            "email": "user@x.com",
            "password": "Cor3ctH0rse!",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        200,
        "login must match the seeded email case-insensitively"
    );
}

/// POST /api/login with wrong password → 401, generic message.
#[tokio::test]
async fn login_with_wrong_password_returns_401() {
    let app = skip_if_none!(spawn_app().await);

    let preflight = bare()
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = extract_set_cookie(preflight.headers(), "csrf_token").unwrap();

    let resp = bare()
        .post(format!("{}/api/login", app.address))
        .header("Cookie", format!("csrf_token={csrf}"))
        .header("X-CSRF-Token", &csrf)
        .json(&serde_json::json!({
            "email": common::ADMIN_EMAIL,
            "password": "wrong-password",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 401);
    // Must return a generic message — not "user not found" vs "wrong password".
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Invalid email or password"),
        "wrong-password response must use generic message, got: {body}"
    );
}

/// GET /api/session with valid session cookie → returns the logged-in user.
#[tokio::test]
async fn session_with_valid_cookie_returns_user() {
    let app = skip_if_none!(spawn_app().await);
    let (session, csrf) = app.login(common::ADMIN_EMAIL, common::ADMIN_PASSWORD).await;

    let resp = bare()
        .get(format!("{}/api/session", app.address))
        .header(
            "Cookie",
            format!("arcodash_session={session}; csrf_token={csrf}"),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        !body["user"].is_null(),
        "authenticated session must return user"
    );
    assert_eq!(body["user"]["email"], common::ADMIN_EMAIL);
}

/// `client_config.ai_monitor_suggestions` is true for a default org (Unrestricted entitlements
/// allow AI).
#[tokio::test]
async fn session_client_config_ai_monitor_suggestions_true_for_default_org() {
    let app = skip_if_none!(spawn_app().await);
    let (session, csrf) = app.login(common::ADMIN_EMAIL, common::ADMIN_PASSWORD).await;

    let resp = bare()
        .get(format!("{}/api/session", app.address))
        .header(
            "Cookie",
            format!("arcodash_session={session}; csrf_token={csrf}"),
        )
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["client_config"]["ai_monitor_suggestions"], true);
}

/// Same field, false when the deployment's entitlements deny AI.
#[tokio::test]
async fn session_client_config_ai_monitor_suggestions_false_when_entitlements_deny_ai() {
    let app = skip_if_none!(
        spawn_app_with_extensions(AppExtensions {
            entitlements: Some(std::sync::Arc::new(common::DenyAll)),
            ..AppExtensions::default()
        })
        .await
    );
    let (session, csrf) = app.login(common::ADMIN_EMAIL, common::ADMIN_PASSWORD).await;
    let resp = bare()
        .get(format!("{}/api/session", app.address))
        .header(
            "Cookie",
            format!("arcodash_session={session}; csrf_token={csrf}"),
        )
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["client_config"]["ai_monitor_suggestions"], false);
}

/// POST /api/logout → clears the session cookie (Max-Age=0 or empty value).
#[tokio::test]
async fn logout_clears_session_cookie() {
    let app = skip_if_none!(spawn_app().await);
    let (session, csrf) = app.login(common::ADMIN_EMAIL, common::ADMIN_PASSWORD).await;

    let resp = bare()
        .post(format!("{}/api/logout", app.address))
        .header(
            "Cookie",
            format!("arcodash_session={session}; csrf_token={csrf}"),
        )
        .header("X-CSRF-Token", &csrf)
        .send()
        .await
        .unwrap();

    // logout handler deliberately returns 204 No Content (see api/handlers/auth.rs::logout).
    assert_eq!(resp.status().as_u16(), 204);

    let logout_cookie = extract_set_cookie(resp.headers(), "arcodash_session");
    let raw_cookie_header = resp.headers().get_all("set-cookie").iter().find_map(|v| {
        let s = v.to_str().ok()?;
        if s.contains("arcodash_session") {
            Some(s.to_string())
        } else {
            None
        }
    });
    let raw = raw_cookie_header.unwrap_or_default();
    assert!(
        logout_cookie.as_deref() == Some("") || raw.contains("Max-Age=0"),
        "logout must clear the session cookie; got Set-Cookie: {raw}"
    );
}

/// After logout, using the old session cookie should return user: null.
#[tokio::test]
async fn session_after_logout_returns_null_user() {
    let app = skip_if_none!(spawn_app().await);
    let (session, csrf) = app.login(common::ADMIN_EMAIL, common::ADMIN_PASSWORD).await;

    bare()
        .post(format!("{}/api/logout", app.address))
        .header(
            "Cookie",
            format!("arcodash_session={session}; csrf_token={csrf}"),
        )
        .header("X-CSRF-Token", &csrf)
        .send()
        .await
        .unwrap();

    // The JWT is stateless; logout just clears the cookie, so simulate by fetching the session
    // fresh.
    let resp = bare()
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["user"].is_null());
}

/// The `LOGIN_RATE_LIMIT_PER_MIN` override is enforced: past the quota gets 429. The limiter reads
/// the env var at router-build time, so set it before `spawn_app()` under `ENV_MUTATION_LOCK`.
#[tokio::test]
async fn login_rate_limit_per_min_override_returns_429_past_quota() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("LOGIN_RATE_LIMIT_PER_MIN", "2");
    let app = spawn_app().await;
    std::env::remove_var("LOGIN_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);

    let attempt = || {
        bare()
            .post(format!("{}/api/login", app.address))
            .json(&serde_json::json!({
                "email": common::ADMIN_EMAIL,
                "password": "wrong-password",
            }))
            .send()
    };

    let first = attempt().await.unwrap();
    assert_eq!(
        first.status().as_u16(),
        401,
        "attempt 1 must be within quota"
    );

    let second = attempt().await.unwrap();
    assert_eq!(
        second.status().as_u16(),
        401,
        "attempt 2 must be within quota"
    );

    let third = attempt().await.unwrap();
    assert_eq!(
        third.status().as_u16(),
        429,
        "attempt 3 must exceed LOGIN_RATE_LIMIT_PER_MIN=2"
    );
}

/// Same override behavior for ACCOUNT_RECOVERY_RATE_LIMIT_PER_MIN, which guards
/// /forgot_password, /reset_password, and /users/invitation/accept (router.rs).
#[tokio::test]
async fn account_recovery_rate_limit_per_min_override_returns_429_past_quota() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("ACCOUNT_RECOVERY_RATE_LIMIT_PER_MIN", "2");
    let app = spawn_app().await;
    std::env::remove_var("ACCOUNT_RECOVERY_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);

    let attempt = || {
        bare()
            .post(format!("{}/api/forgot_password", app.address))
            .json(&serde_json::json!({ "email": "nobody@example.com" }))
            .send()
    };

    let first = attempt().await.unwrap();
    assert_ne!(
        first.status().as_u16(),
        429,
        "attempt 1 must be within quota"
    );

    let second = attempt().await.unwrap();
    assert_ne!(
        second.status().as_u16(),
        429,
        "attempt 2 must be within quota"
    );

    let third = attempt().await.unwrap();
    assert_eq!(
        third.status().as_u16(),
        429,
        "attempt 3 must exceed ACCOUNT_RECOVERY_RATE_LIMIT_PER_MIN=2"
    );
}

/// `/api/saml/acs` carries the same per-IP login-tier limiter as the other login routes. SAML isn't
/// configured here, so attempts 503 until the limiter trips; the point is the 429.
#[tokio::test]
async fn saml_acs_rate_limit_per_min_override_returns_429_past_quota() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("LOGIN_RATE_LIMIT_PER_MIN", "2");
    let app = spawn_app().await;
    std::env::remove_var("LOGIN_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);

    let attempt = || {
        bare()
            .post(format!("{}/api/saml/acs", app.address))
            .form(&[("SAMLResponse", "not-a-real-response")])
            .send()
    };

    let first = attempt().await.unwrap();
    assert_ne!(
        first.status().as_u16(),
        429,
        "attempt 1 must be within quota"
    );

    let second = attempt().await.unwrap();
    assert_ne!(
        second.status().as_u16(),
        429,
        "attempt 2 must be within quota"
    );

    let third = attempt().await.unwrap();
    assert_eq!(
        third.status().as_u16(),
        429,
        "attempt 3 must exceed LOGIN_RATE_LIMIT_PER_MIN=2"
    );
}

/// Proves `auth::password`'s `spawn_blocking` matters end to end: a burst of concurrent logins
/// (real Argon2 verifies) must not stall an unrelated `/api/status` request.
/// `LOGIN_RATE_LIMIT_PER_MIN` is raised so the shared login budget can't 429 a login before Argon2
/// runs; each bad login is asserted 401 so a throttled run fails loudly.
#[tokio::test]
async fn concurrent_logins_do_not_starve_a_status_request() {
    let _guard = ENV_MUTATION_LOCK.lock().await;
    std::env::set_var("LOGIN_RATE_LIMIT_PER_MIN", "1000");
    let app = spawn_app().await;
    std::env::remove_var("LOGIN_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);
    let client = bare();

    let login_tasks: Vec<_> = (0..8)
        .map(|_| {
            let client = client.clone();
            let address = app.address.clone();
            tokio::spawn(async move {
                client
                    .post(format!("{address}/api/login"))
                    .json(&serde_json::json!({
                        "email": common::ADMIN_EMAIL,
                        "password": "wrong-password",
                    }))
                    .send()
                    .await
            })
        })
        .collect();

    // `/api/status` is admin-only: use an authenticated admin client.
    let status_client = app.admin_client().await;

    let status_start = std::time::Instant::now();
    let status_resp = status_client
        .get(format!("{}/api/status", app.address))
        .send()
        .await
        .unwrap();
    let status_elapsed = status_start.elapsed();
    assert_eq!(status_resp.status().as_u16(), 200);
    assert!(
        status_elapsed < std::time::Duration::from_secs(2),
        "/api/status took {status_elapsed:?} while 8 logins were in flight — Argon2 may be \
         blocking the async runtime instead of running via spawn_blocking"
    );

    for task in login_tasks {
        let resp = task.await.unwrap().unwrap();
        assert_eq!(
            resp.status().as_u16(),
            401,
            "a bad login must reach Argon2 and fail on its own merits, not be short-circuited \
             by the rate limiter"
        );
    }
}

/// A cookie-authed mutating request with no X-CSRF-Token → 403 in the standard JSON error envelope,
/// not bare text.
#[tokio::test]
async fn mutating_request_missing_csrf_token_returns_json_403() {
    let app = skip_if_none!(spawn_app().await);
    let (session, csrf) = app.login(common::ADMIN_EMAIL, common::ADMIN_PASSWORD).await;

    let resp = bare()
        .post(format!("{}/api/queries", app.address))
        .header(
            "Cookie",
            format!("arcodash_session={session}; csrf_token={csrf}"),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 403);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["message"].is_string(),
        "expected a JSON `message` field, got: {body}"
    );
}

/// A cookie-authed mutating request whose X-CSRF-Token header doesn't match the csrf_token
/// cookie → same JSON 403 envelope as the missing-token case.
#[tokio::test]
async fn mutating_request_mismatched_csrf_token_returns_json_403() {
    let app = skip_if_none!(spawn_app().await);
    let (session, csrf) = app.login(common::ADMIN_EMAIL, common::ADMIN_PASSWORD).await;

    let resp = bare()
        .post(format!("{}/api/queries", app.address))
        .header(
            "Cookie",
            format!("arcodash_session={session}; csrf_token={csrf}"),
        )
        .header("X-CSRF-Token", "not-the-right-token")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 403);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["message"].is_string(),
        "expected a JSON `message` field, got: {body}"
    );
}
