//! Integration tests: self-serve `POST /api/users/me/password`.
//!
//! Distinct from the mailed-token `reset_password` flow (covered in `integration_users.rs`):
//! this path requires an active session plus proof of the current password.

mod common;
use common::{authed_client, extract_set_cookie, spawn_app, ADMIN_EMAIL, ADMIN_PASSWORD};

/// Guards every `spawn_app()` call in this binary against
/// `change_password_rate_limit_per_min_override_returns_429_past_quota`, which mutates the
/// process-global `CHANGE_PASSWORD_RATE_LIMIT_PER_MIN` env var around its own `spawn_app()`
/// call (the router reads it once at build time). Tests in one `--test` file share a process, so
/// without this every other test here would need to build its router outside that window too —
/// taking this lock before `spawn_app()` is what actually enforces that ordering.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn lock_env() -> tokio::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().await
}

/// POST /api/users/me/password with the wrong current password → 401, and the old password
/// still works (nothing changed).
#[tokio::test]
async fn wrong_current_password_returns_401_and_leaves_password_unchanged() {
    let _guard = lock_env().await;
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .post(format!("{}/api/users/me/password", app.address))
        .json(&serde_json::json!({
            "current_password": "definitely-not-the-password",
            "new_password": "N3wPassword!Pass"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        401,
        "wrong current password must be 401"
    );

    // The original password must still authenticate.
    let (session, _csrf) = app.login(ADMIN_EMAIL, ADMIN_PASSWORD).await;
    assert!(
        !session.is_empty(),
        "the original password must still work after a rejected change"
    );
}

/// POST /api/users/me/password with the right current password → 204, the new password logs
/// in, the old one no longer does, and the request reissues a fresh session cookie the caller's
/// own browser keeps working with — changing your own password must not kill your own active
/// session, even though it bumps `session_token_version` (see below) and so invalidates every
/// OTHER outstanding cookie for this user.
#[tokio::test]
async fn correct_current_password_changes_password_and_keeps_session_valid() {
    let _guard = lock_env().await;
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (session, csrf) = app.login(ADMIN_EMAIL, ADMIN_PASSWORD).await;
    let admin = authed_client(&app.address, &session, &csrf);
    let new_password = "Ch4ngedPassword!Pass";

    let resp = admin
        .post(format!("{}/api/users/me/password", app.address))
        .json(&serde_json::json!({
            "current_password": ADMIN_PASSWORD,
            "new_password": new_password
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        204,
        "correct current password must succeed with 204"
    );

    // The pre-change cookie is now stale (session_token_version bumped) — a request still using
    // it must read as logged out, not merely "the client didn't bother refreshing."
    let stale_check = admin
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let stale_body: serde_json::Value = stale_check.json().await.unwrap();
    assert!(
        stale_body["user"].is_null(),
        "the pre-change session cookie must no longer resolve to a user"
    );

    // The response's freshly reissued cookie is what actually keeps the caller logged in.
    let reissued_session = extract_set_cookie(resp.headers(), "arcodash_session")
        .expect("a successful password change must reissue a session cookie");
    let refreshed = authed_client(&app.address, &reissued_session, &csrf);
    let still_valid = refreshed
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        still_valid.status().as_u16(),
        200,
        "the calling session must remain valid after a password change"
    );
    let session_body: serde_json::Value = still_valid.json().await.unwrap();
    assert!(
        !session_body["user"].is_null(),
        "the reissued cookie must still resolve to a logged-in user"
    );

    // New password logs in.
    let (new_session, _csrf) = app.login(ADMIN_EMAIL, new_password).await;
    assert!(
        !new_session.is_empty(),
        "the new password must be able to log in"
    );

    // Old password no longer logs in — issue a fresh login attempt directly rather than via
    // the `login()` helper (which asserts success).
    let anon = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap();
    let sess = anon
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = sess
        .cookies()
        .find(|c| c.name() == "csrf_token")
        .map(|c| c.value().to_string())
        .unwrap();
    let old_login = anon
        .post(format!("{}/api/login", app.address))
        .header("X-CSRF-Token", csrf)
        .json(&serde_json::json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        old_login.status().as_u16(),
        401,
        "the superseded password must no longer authenticate"
    );
}

/// A second, independently-issued session (e.g. a different browser/device logged in earlier)
/// must stop resolving the moment the password changes from the first session — the whole point
/// of bumping `session_token_version` on a credential change.
#[tokio::test]
async fn changing_password_invalidates_a_different_devices_session() {
    let _guard = lock_env().await;
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    // Two independent logins for the same account -- two distinct signed cookies, both valid
    // right now (session_token_version starts at 0 for a freshly seeded user).
    let (session_a, csrf_a) = app.login(ADMIN_EMAIL, ADMIN_PASSWORD).await;
    let device_a = authed_client(&app.address, &session_a, &csrf_a);
    let (session_b, csrf_b) = app.login(ADMIN_EMAIL, ADMIN_PASSWORD).await;
    let device_b = authed_client(&app.address, &session_b, &csrf_b);

    let pre_check = device_b
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let pre_body: serde_json::Value = pre_check.json().await.unwrap();
    assert!(
        !pre_body["user"].is_null(),
        "device B's session must be valid before the change"
    );

    let resp = device_a
        .post(format!("{}/api/users/me/password", app.address))
        .json(&serde_json::json!({
            "current_password": ADMIN_PASSWORD,
            "new_password": "AnotherDevice!Pass1"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    let post_check = device_b
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let post_body: serde_json::Value = post_check.json().await.unwrap();
    assert!(
        post_body["user"].is_null(),
        "device B's session must be invalidated by device A's password change"
    );
}

/// POST /api/users/me/password with no session and no CSRF token → 403. The CSRF middleware
/// (mutating request, no cookie, no api_key, no X-CSRF-Token) rejects before the request ever
/// reaches auth — same as every other cookie-scoped mutating endpoint, not a 401 (see
/// `middleware/csrf.rs`).
#[tokio::test]
async fn change_password_without_session_returns_403() {
    let _guard = lock_env().await;
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let anon = reqwest::Client::new();
    let resp = anon
        .post(format!("{}/api/users/me/password", app.address))
        .json(&serde_json::json!({
            "current_password": "whatever",
            "new_password": "N3wPassword!Pass"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        403,
        "an anonymous caller must not be able to change any password"
    );
}

/// A password shorter than the shared 8-character floor (`user_service::MIN_PASSWORD_LENGTH`)
/// is rejected server-side even though the request is otherwise well-formed (right current
/// password, valid session) — the frontend's `minLength={8}` is not the only enforcement.
#[tokio::test]
async fn new_password_below_minimum_length_returns_400_and_leaves_password_unchanged() {
    let _guard = lock_env().await;
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .post(format!("{}/api/users/me/password", app.address))
        .json(&serde_json::json!({
            "current_password": ADMIN_PASSWORD,
            "new_password": "short1"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "a new password under the minimum length must be 400"
    );

    // The original password must still authenticate — a rejected change must not touch the row.
    let (session, _csrf) = app.login(ADMIN_EMAIL, ADMIN_PASSWORD).await;
    assert!(
        !session.is_empty(),
        "the original password must still work after a rejected too-short change"
    );
}

/// A successful password change records a `user.password_changed` audit event (queryable via
/// `GET /api/events`, same table `group.role_attached`-style tests already assert against).
#[tokio::test]
async fn correct_password_change_records_audit_event() {
    let _guard = lock_env().await;
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .post(format!("{}/api/users/me/password", app.address))
        .json(&serde_json::json!({
            "current_password": ADMIN_PASSWORD,
            "new_password": "Aud1tEv3nt!Pass"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    use arcodash_core::entities::event;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let events = event::Entity::find()
        .filter(event::Column::Action.eq("user.password_changed"))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        events.len(),
        1,
        "a successful password change must record exactly one user.password_changed event, got {events:?}"
    );
}

/// A rejected password change (wrong current password) must NOT record an audit event — only
/// the actual mutation is audited.
#[tokio::test]
async fn rejected_password_change_records_no_audit_event() {
    let _guard = lock_env().await;
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .post(format!("{}/api/users/me/password", app.address))
        .json(&serde_json::json!({
            "current_password": "definitely-not-the-password",
            "new_password": "N3wPassword!Pass"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    use arcodash_core::entities::event;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let events = event::Entity::find()
        .filter(event::Column::Action.eq("user.password_changed"))
        .all(&app.db)
        .await
        .unwrap();
    assert!(
        events.is_empty(),
        "a rejected password change must not record an audit event, got {events:?}"
    );
}

/// `CHANGE_PASSWORD_RATE_LIMIT_PER_MIN` is actually enforced, same override pattern as
/// `login_rate_limit_per_min_override_returns_429_past_quota` in `integration_auth.rs`: the env
/// var is read once at router-build time, so it must be set before `spawn_app()`. Wrong current
/// password is used for every attempt so the quota is exhausted without ever mutating the row.
/// Holds `ENV_LOCK` for the whole window the env var is set, so no sibling test's `spawn_app()`
/// (each guarded by the same lock) can read it.
#[tokio::test]
async fn change_password_rate_limit_per_min_override_returns_429_past_quota() {
    let _guard = lock_env().await;
    std::env::set_var("CHANGE_PASSWORD_RATE_LIMIT_PER_MIN", "2");
    let app = spawn_app().await;
    std::env::remove_var("CHANGE_PASSWORD_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let attempt = || {
        admin
            .post(format!("{}/api/users/me/password", app.address))
            .json(&serde_json::json!({
                "current_password": "definitely-wrong",
                "new_password": "N3wPassword!Pass"
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
        "attempt 3 must exceed CHANGE_PASSWORD_RATE_LIMIT_PER_MIN=2"
    );
}
