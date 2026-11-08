//! Integration tests for security hardening: email immutability, SSO
//! password-reset refusal, invitation-pending login rejection, and the data-source-update SSRF
//! guard. Each test targets the actual HTTP-reachable behavior, not just the underlying service
//! function.

mod common;
use common::default_org_id;
use common::{spawn_app, spawn_app_with, ADMIN_EMAIL, ADMIN_PASSWORD};

use arcodash_core::entities::user;
use sea_orm::{ActiveModelTrait, ActiveValue::*, EntityTrait};

/// Insert a user row directly, bypassing every app-level invariant, for fixtures that need a
/// combination the app's own code paths never produce on their own (e.g. a password hash on a
/// still-pending invitation).
#[allow(clippy::too_many_arguments)]
async fn insert_user(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    email: &str,
    password_hash: Option<String>,
    is_invitation_pending: bool,
) -> user::Model {
    let id = user::Entity::insert(user::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        name: Set("Fixture User".to_string()),
        email: Set(email.to_string()),
        password_hash: Set(password_hash),
        groups: Set(vec![]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(&format!(
            "fixture-key-{}",
            uuid::Uuid::new_v4().simple()
        ))),
        disabled_at: Set(None),
        is_invitation_pending: Set(is_invitation_pending),
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
    .expect("user insert failed")
    .last_insert_id;

    user::Entity::find_by_id(id).one(db).await.unwrap().unwrap()
}

// ── email immutability ──────────────────────────────────────────────────

/// `POST /api/users/:id` (self) with an `email` key in the body must silently ignore it —
/// email can only ever be set at account-creation time (invite/SSO), never edited afterward.
#[tokio::test]
async fn update_ignores_email_field_in_request_body() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let self_id: i64 = {
        let session = app.login(ADMIN_EMAIL, ADMIN_PASSWORD).await;
        let _ = session; // login() already asserted success; id fetched below via /session
        let resp = admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        body["user"]["id"].as_i64().unwrap()
    };

    let resp = admin
        .post(format!("{}/api/users/{self_id}", app.address))
        .json(&serde_json::json!({
            "name": "New Name",
            "email": "attacker-controlled@evil.example"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "update must still succeed");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "New Name", "name must still be editable");
    assert_eq!(
        body["email"], ADMIN_EMAIL,
        "email must be left completely unchanged by the update call"
    );

    // Re-fetch straight from the DB to confirm this isn't just a stale response artifact.
    let refetched = user::Entity::find_by_id(self_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(refetched.email, ADMIN_EMAIL);
}

// ── SSO password-hash refusal ───────────────────────────────────────────

/// `POST /api/forgot_password` for an SSO-provisioned account (no `password_hash`) must be a
/// silent no-op — same 204 as any other case (no enumeration), but no reset token gets stamped
/// onto the row, so a mailed link could never work anyway.
#[tokio::test]
async fn forgot_password_is_a_noop_for_sso_provisioned_account() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let email = format!("sso-user-{}@example.com", uuid::Uuid::new_v4().simple());
    let sso_user = insert_user(&app.db, org_id, &email, None, false).await;

    let bare = reqwest::Client::new();
    let resp = bare
        .post(format!("{}/api/forgot_password", app.address))
        .json(&serde_json::json!({ "email": email }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204, "always 204, no enumeration");

    let refetched = user::Entity::find_by_id(sso_user.id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        refetched.details.get("reset_token").is_none(),
        "an SSO account must never get a reset token stamped, got: {:?}",
        refetched.details
    );
}

/// `POST /api/reset_password` against a token that (somehow) exists on an SSO-provisioned
/// account's row is rejected with the same message `change_password` uses — defense in depth on
/// top of `forgot_password` not issuing such a token in the first place.
#[tokio::test]
async fn reset_password_rejects_token_for_sso_account() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let email = format!("sso-user-{}@example.com", uuid::Uuid::new_v4().simple());
    let sso_user = insert_user(&app.db, org_id, &email, None, false).await;

    // Stamp a reset token directly (bypassing forgot_password, which refuses to do so for this
    // account) to exercise reset_password's own independent guard.
    let token = uuid::Uuid::new_v4().to_string();
    let mut active: user::ActiveModel = sso_user.into();
    active.details = Set(serde_json::json!({
        "reset_token": token,
        "reset_token_expires_at": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp(),
    }));
    active.update(&app.db).await.unwrap();

    let bare = reqwest::Client::new();
    let resp = bare
        .post(format!("{}/api/reset_password", app.address))
        .json(&serde_json::json!({ "token": token, "password": "N3wPassword!Pass" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "an SSO account's password can never be reset via a mailed token"
    );
}

// ── invitation-pending login rejection ──────────────────────────────────

/// A row that is somehow both `is_invitation_pending = true` AND carries a `password_hash`
/// (never produced by this codebase's own invite/accept flow, but not structurally impossible)
/// must still be refused at login — the account isn't "live" until the invite is accepted.
#[tokio::test]
async fn login_rejects_a_pending_invitation_even_with_a_password_hash_set() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let email = format!("pending-{}@example.com", uuid::Uuid::new_v4().simple());
    let password = "Pending!Pass123";
    let hash = arcodash_backend::auth::password::hash_password(password)
        .await
        .unwrap();
    insert_user(&app.db, org_id, &email, Some(hash), true).await;

    let bare = reqwest::Client::new();
    let resp = bare
        .post(format!("{}/api/login", app.address))
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        401,
        "a still-pending invitation must never be able to log in"
    );
}

// ── SSRF guard on data-source update ────────────────────────────────────

/// `POST /api/data_sources/:id` (update) must re-run the SSRF guard against the merged options,
/// not just accept whatever the client sends — a metadata-IP `host` field must be blocked on
/// update exactly as it already is on create.
#[tokio::test]
async fn update_data_source_blocks_a_metadata_ip_target() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let create_resp = admin
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "SSRF Update Probe",
            "type": "pg",
            "options": {
                "host": "8.8.8.8",
                "port": 5432,
                "dbname": "d",
                "user": "u",
                "password": "p"
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(create_resp.status().as_u16(), 200);
    let created: serde_json::Value = create_resp.json().await.unwrap();
    let id = created["id"].as_i64().unwrap();

    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    let update_resp = admin
        .post(format!("{}/api/data_sources/{id}", app.address))
        .json(&serde_json::json!({
            "options": { "host": "169.254.169.254" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        update_resp.status().as_u16(),
        400,
        "an update pointing the host at the cloud metadata IP must be blocked"
    );
}

// ── internal errors are logged and sanitized before reaching the client ────

/// An unreachable LDAP host produces a real connection-error string containing the host —
/// `ldap.rs`'s `ApiError::internal(format!("LDAP connection failed: {{e}}"))`, a pre-auth leak
/// site. Sanitization lives centrally in `error.rs`'s `IntoResponse for ApiError`, not at this call
/// site, so this exercises that central sanitization through one real leak.
#[tokio::test]
async fn ldap_bind_failure_never_leaks_the_ldap_host_to_the_client() {
    let app = skip_if_none!(
        spawn_app_with(|c| {
            c.ldap_url = Some("ldap://127.0.0.1:1".to_string());
            c.ldap_search_base_dn = Some("dc=example,dc=com".to_string());
        })
        .await
    );

    let bare = reqwest::Client::new();
    let resp = bare
        .post(format!("{}/api/ldap/login", app.address))
        .json(&serde_json::json!({ "username": "someone", "password": "whatever" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 500);
    let body: serde_json::Value = resp.json().await.unwrap();
    let message = body["message"].as_str().unwrap_or_default();
    assert_eq!(
        message, "internal server error",
        "a 500 must never carry the real error text: {message}"
    );
    assert!(
        !message.contains("127.0.0.1"),
        "must never leak the LDAP host: {message}"
    );
}

/// Contrast case: a 400's specific, user-facing message must survive untouched — sanitization
/// only strips detail from a bare 500, never from a deliberate 4xx.
#[tokio::test]
async fn a_400_still_carries_its_specific_message() {
    let app = skip_if_none!(spawn_app().await);

    let bare = reqwest::Client::new();
    let resp = bare
        .post(format!("{}/api/reset_password", app.address))
        .json(&serde_json::json!({ "token": "not-a-real-token", "password": "N3wPassword!Pass" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["message"], "invalid or expired reset token");
}
