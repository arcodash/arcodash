//! Integration tests for the daily-digest self-serve toggle (`POST /api/users/me/digest`) and the
//! public unsubscribe endpoint (`GET /api/digest/unsubscribe`), including its uniform responses and
//! per-IP rate limit.

mod common;
use common::{authed_client, spawn_app, ADMIN_EMAIL};

use arcodash_core::entities::user;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

async fn admin_row(app: &common::TestApp) -> user::Model {
    user::Entity::find()
        .filter(user::Column::Email.eq(ADMIN_EMAIL))
        .one(&app.db)
        .await
        .expect("admin query failed")
        .expect("admin user must exist")
}

/// Bare client with no cookies and no CSRF.
fn bare() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(false)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn user_row(app: &common::TestApp, user_id: i64) -> user::Model {
    user::Entity::find_by_id(user_id)
        .one(&app.db)
        .await
        .expect("user query failed")
        .expect("user must exist")
}

/// Seed a second non-admin user directly in the DB; returns a logged-in client + their id.
async fn create_second_user(
    app: &common::TestApp,
    email: &str,
    password: &str,
) -> (reqwest::Client, i64) {
    use arcodash_core::entities::{group, organization};
    use sea_orm::{sea_query::OnConflict, ActiveValue::*, ColumnTrait, QueryFilter};

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();

    let default_group = group::Entity::find()
        .filter(group::Column::OrgId.eq(org.id))
        .filter(group::Column::Name.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default group must exist after migrations");

    let hash = {
        use argon2::{Argon2, PasswordHasher};
        Argon2::default()
            .hash_password(password.as_bytes())
            .unwrap()
            .to_string()
    };
    let api_key = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );

    let result = user::Entity::insert(user::ActiveModel {
        id: NotSet,
        org_id: Set(org.id),
        name: Set("Second User".to_string()),
        email: Set(email.to_string()),
        password_hash: Set(Some(hash)),
        groups: Set(vec![default_group.id]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(&api_key)),
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
    .on_conflict(
        OnConflict::columns([user::Column::OrgId, user::Column::Email])
            .do_nothing()
            .to_owned(),
    )
    .exec(&app.db)
    .await;

    let user_id = match result {
        Ok(r) => r.last_insert_id,
        Err(sea_orm::DbErr::RecordNotInserted) => {
            user::Entity::find()
                .filter(user::Column::Email.eq(email))
                .one(&app.db)
                .await
                .unwrap()
                .unwrap()
                .id
        }
        Err(e) => panic!("second user seed failed: {e}"),
    };

    let (session, csrf) = app.login(email, password).await;
    let client = authed_client(&app.address, &session, &csrf);
    (client, user_id)
}

/// Enabling carries `hour`/`timezone` (default 8/UTC) to satisfy the "first enable must set both"
/// rule; use `set_digest_raw` to test that rule.
async fn set_digest(client: &reqwest::Client, base: &str, enabled: bool) -> reqwest::Response {
    let body = if enabled {
        serde_json::json!({ "enabled": true, "hour": 8, "timezone": "UTC" })
    } else {
        serde_json::json!({ "enabled": false })
    };
    client
        .post(format!("{base}/api/users/me/digest"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// Raw POST with an arbitrary body, for request-shape tests.
async fn set_digest_raw(
    client: &reqwest::Client,
    base: &str,
    body: serde_json::Value,
) -> reqwest::Response {
    client
        .post(format!("{base}/api/users/me/digest"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// Enabling sets `digest_enabled = true` and a non-null token, asserted against the DB row.
#[tokio::test]
async fn enable_digest_sets_flag_and_generates_token() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let resp = set_digest(&admin, &app.address, true).await;
    assert_eq!(resp.status().as_u16(), 200);

    let row = admin_row(&app).await;

    assert!(row.digest_enabled, "digest_enabled must be true");
    assert!(
        row.digest_unsubscribe_token
            .as_ref()
            .is_some_and(|t| !t.is_empty()),
        "digest_unsubscribe_token must be a non-empty real token"
    );
}

#[tokio::test]
async fn disable_digest_clears_flag_and_token() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    assert_eq!(set_digest(&admin, &app.address, true).await.status(), 200);
    assert_eq!(set_digest(&admin, &app.address, false).await.status(), 200);

    let row = admin_row(&app).await;

    assert!(!row.digest_enabled, "digest_enabled must be false");
    assert!(
        row.digest_unsubscribe_token.is_none(),
        "digest_unsubscribe_token must be nulled on disable"
    );
}

/// User A's toggle has no effect on user B's row (`set_digest` always acts on `CurrentUser`).
#[tokio::test]
async fn digest_toggle_is_self_serve_only_other_user_untouched() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let email_b = format!("digest_b_{}@test.local", uuid::Uuid::new_v4().simple());
    let (_client_b, user_b_id) = create_second_user(&app, &email_b, "S3condUser!Pass").await;

    let before = user_row(&app, user_b_id).await;
    assert!(!before.digest_enabled);
    assert!(before.digest_unsubscribe_token.is_none());

    assert_eq!(set_digest(&admin, &app.address, true).await.status(), 200);

    let after = user_row(&app, user_b_id).await;
    assert!(
        !after.digest_enabled,
        "user B's digest_enabled must not change when user A toggles theirs"
    );
    assert!(
        after.digest_unsubscribe_token.is_none(),
        "user B's token must not be generated by user A's toggle"
    );
}

/// Re-enabling regenerates the token, and the old token stops working.
#[tokio::test]
async fn re_enable_digest_regenerates_token_and_invalidates_old_one() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    assert_eq!(set_digest(&admin, &app.address, true).await.status(), 200);
    let first_token = admin_row(&app)
        .await
        .digest_unsubscribe_token
        .expect("first enable must produce a token");

    assert_eq!(set_digest(&admin, &app.address, true).await.status(), 200);
    let second_token = admin_row(&app)
        .await
        .digest_unsubscribe_token
        .expect("re-enable must still have a token");

    assert_ne!(
        first_token, second_token,
        "re-enabling must regenerate the unsubscribe token"
    );

    // The old token must hit the generic "invalid" copy and leave the row alone.
    let unsub_resp = bare()
        .get(format!(
            "{}/api/digest/unsubscribe?token={}",
            app.address, first_token
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(unsub_resp.status().as_u16(), 200);
    let body = unsub_resp.text().await.unwrap();
    assert!(
        body.contains("This link is no longer valid"),
        "stale old token must hit the generic-invalid page; got: {body}"
    );

    let still_current = admin_row(&app).await;
    assert!(
        still_current.digest_enabled,
        "hitting the stale old token must not disable the current (still valid) subscription"
    );
    assert_eq!(
        still_current.digest_unsubscribe_token.as_deref(),
        Some(second_token.as_str()),
        "current token must be untouched by the stale-token unsubscribe attempt"
    );
}

/// Unauthenticated POST returns 401 or 403.
#[tokio::test]
async fn set_digest_unauthenticated_returns_401_or_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let resp = bare()
        .post(format!("{}/api/users/me/digest", app.address))
        .json(&serde_json::json!({ "enabled": true }))
        .send()
        .await
        .unwrap();

    let status = resp.status().as_u16();
    assert!(
        status == 401 || status == 403,
        "unauthenticated POST /users/me/digest must be 401 or 403, got {status}"
    );
}

/// Valid token: 200, flag flips false, token nulled, "unsubscribed" copy.
#[tokio::test]
async fn unsubscribe_valid_token_flips_flag_and_nulls_token() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    assert_eq!(set_digest(&admin, &app.address, true).await.status(), 200);
    let admin_id = admin_row(&app).await.id;
    let token = user_row(&app, admin_id)
        .await
        .digest_unsubscribe_token
        .expect("must have a token after enabling");

    let resp = bare()
        .get(format!(
            "{}/api/digest/unsubscribe?token={}",
            app.address, token
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("You're unsubscribed"),
        "valid-token response must contain the unsubscribed copy; got: {body}"
    );

    let after = user_row(&app, admin_id).await;
    assert!(!after.digest_enabled, "digest_enabled must flip to false");
    assert!(
        after.digest_unsubscribe_token.is_none(),
        "token must be nulled after a successful unsubscribe"
    );
}

/// A successful unsubscribe must leave an audit event.
#[tokio::test]
async fn unsubscribe_valid_token_records_an_audit_event() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    assert_eq!(set_digest(&admin, &app.address, true).await.status(), 200);
    let admin_id = admin_row(&app).await.id;
    let token = user_row(&app, admin_id)
        .await
        .digest_unsubscribe_token
        .expect("must have a token after enabling");

    let resp = bare()
        .get(format!(
            "{}/api/digest/unsubscribe?token={}",
            app.address, token
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let events_resp = admin
        .get(format!(
            "{}/api/events?action=user.digest_unsubscribed",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(events_resp.status().as_u16(), 200);
    let events: serde_json::Value = events_resp.json().await.unwrap();
    let found = events.as_array().unwrap().iter().any(|e| {
        e["object_id"].as_i64() == Some(admin_id) && e["user_id"].as_i64() == Some(admin_id)
    });
    assert!(
        found,
        "no user.digest_unsubscribed event for user {admin_id}: {events:#?}"
    );
}

/// A bogus token yields the same HTML as any invalid token and writes no events row (no
/// audit-writing oracle).
#[tokio::test]
async fn unsubscribe_invalid_token_records_no_audit_event() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let resp = bare()
        .get(format!(
            "{}/api/digest/unsubscribe?token=this-token-does-not-exist-{}",
            app.address,
            uuid::Uuid::new_v4()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("This link is no longer valid"));

    let events_resp = admin
        .get(format!(
            "{}/api/events?action=user.digest_unsubscribed",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(events_resp.status().as_u16(), 200);
    let events: serde_json::Value = events_resp.json().await.unwrap();
    assert_eq!(
        events.as_array().unwrap().len(),
        0,
        "an invalid token must never write a user.digest_unsubscribed event: {events:#?}"
    );
}

/// Invalid token: same 200 shape and generic copy, no distinction between "never existed" and
/// "already used".
#[tokio::test]
async fn unsubscribe_invalid_token_returns_generic_200_no_error() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let resp = bare()
        .get(format!(
            "{}/api/digest/unsubscribe?token=this-token-does-not-exist-{}",
            app.address,
            uuid::Uuid::new_v4()
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("This link is no longer valid"),
        "invalid token must hit the generic-invalid copy; got: {body}"
    );
    assert!(
        !body.contains("You're unsubscribed"),
        "invalid token must NOT show the success copy"
    );
}

#[tokio::test]
async fn unsubscribe_missing_token_returns_generic_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let resp = bare()
        .get(format!("{}/api/digest/unsubscribe", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("This link is no longer valid"));

    let resp2 = bare()
        .get(format!("{}/api/digest/unsubscribe?token=", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status().as_u16(), 200);
    let body2 = resp2.text().await.unwrap();
    assert!(body2.contains("This link is no longer valid"));
}

/// The token lookup is global; a token only ever affects its own user's row (two users, two tokens,
/// no cross-talk).
#[tokio::test]
async fn unsubscribe_token_only_affects_its_owning_user() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let email_b = format!("digest_c_{}@test.local", uuid::Uuid::new_v4().simple());
    let (client_b, user_b_id) = create_second_user(&app, &email_b, "Th1rdUser!Pass").await;

    assert_eq!(set_digest(&admin, &app.address, true).await.status(), 200);
    assert_eq!(
        set_digest(&client_b, &app.address, true).await.status(),
        200
    );

    let admin_id = admin_row(&app).await.id;
    let admin_token = user_row(&app, admin_id)
        .await
        .digest_unsubscribe_token
        .expect("admin must have a token");
    let b_token_before = user_row(&app, user_b_id)
        .await
        .digest_unsubscribe_token
        .expect("user B must have a token");
    assert_ne!(
        admin_token, b_token_before,
        "tokens must be distinct per user"
    );

    let resp = bare()
        .get(format!(
            "{}/api/digest/unsubscribe?token={}",
            app.address, admin_token
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let admin_after = user_row(&app, admin_id).await;
    assert!(!admin_after.digest_enabled, "admin must be unsubscribed");

    let b_after = user_row(&app, user_b_id).await;
    assert!(
        b_after.digest_enabled,
        "user B must remain subscribed — admin's token must not affect B's row"
    );
    assert_eq!(
        b_after.digest_unsubscribe_token.as_deref(),
        Some(b_token_before.as_str()),
        "user B's token must be untouched by admin's unsubscribe"
    );
}

/// `DIGEST_UNSUBSCRIBE_RATE_LIMIT_PER_MIN` is enforced (429 past quota). The env var is read at
/// router-build time, so set it before `spawn_app()`.
#[tokio::test]
async fn unsubscribe_rate_limit_per_min_override_returns_429_past_quota() {
    std::env::set_var("DIGEST_UNSUBSCRIBE_RATE_LIMIT_PER_MIN", "2");
    let app = spawn_app().await;
    std::env::remove_var("DIGEST_UNSUBSCRIBE_RATE_LIMIT_PER_MIN");
    let app = skip_if_none!(app);

    let client = bare();
    let hit = || {
        client
            .get(format!(
                "{}/api/digest/unsubscribe?token=irrelevant-{}",
                app.address,
                uuid::Uuid::new_v4()
            ))
            .send()
    };

    let first = hit().await.unwrap();
    assert_ne!(
        first.status().as_u16(),
        429,
        "attempt 1 must be within quota"
    );

    let second = hit().await.unwrap();
    assert_ne!(
        second.status().as_u16(),
        429,
        "attempt 2 must be within quota"
    );

    let third = hit().await.unwrap();
    assert_eq!(
        third.status().as_u16(),
        429,
        "attempt 3 must exceed DIGEST_UNSUBSCRIBE_RATE_LIMIT_PER_MIN=2"
    );
}

/// First-ever enable without both hour and timezone is rejected and leaves the row untouched.
#[tokio::test]
async fn first_enable_without_hour_and_timezone_is_rejected() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let resp = set_digest_raw(&admin, &app.address, serde_json::json!({ "enabled": true })).await;
    assert_eq!(resp.status().as_u16(), 400);

    let row = admin_row(&app).await;
    assert!(
        !row.digest_enabled,
        "a rejected first-enable must not flip digest_enabled"
    );
    assert!(row.digest_hour.is_none());
    assert!(row.digest_timezone.is_none());
}

/// First-ever enable with only one of the two fields is rejected.
#[tokio::test]
async fn first_enable_with_only_hour_is_rejected() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let resp = set_digest_raw(
        &admin,
        &app.address,
        serde_json::json!({ "enabled": true, "hour": 9 }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    assert!(!admin_row(&app).await.digest_enabled);
}

/// First-ever enable with both fields persists exactly what was sent.
#[tokio::test]
async fn first_enable_with_hour_and_timezone_persists_both() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let resp = set_digest_raw(
        &admin,
        &app.address,
        serde_json::json!({ "enabled": true, "hour": 21, "timezone": "America/Los_Angeles" }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);

    let row = admin_row(&app).await;
    assert!(row.digest_enabled);
    assert_eq!(row.digest_hour, Some(21));
    assert_eq!(row.digest_timezone.as_deref(), Some("America/Los_Angeles"));
}

/// Once hour/timezone are stored, re-enabling may omit both.
#[tokio::test]
async fn re_enable_without_hour_and_timezone_keeps_the_stored_ones() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    assert_eq!(
        set_digest_raw(
            &admin,
            &app.address,
            serde_json::json!({ "enabled": true, "hour": 14, "timezone": "Asia/Tokyo" }),
        )
        .await
        .status(),
        200
    );
    assert_eq!(set_digest(&admin, &app.address, false).await.status(), 200);

    let resp = set_digest_raw(&admin, &app.address, serde_json::json!({ "enabled": true })).await;
    assert_eq!(resp.status().as_u16(), 200);

    let row = admin_row(&app).await;
    assert!(row.digest_enabled);
    assert_eq!(
        row.digest_hour,
        Some(14),
        "prior hour must survive a bare re-enable"
    );
    assert_eq!(row.digest_timezone.as_deref(), Some("Asia/Tokyo"));
}

/// An hour-only or timezone-only update changes just that field.
#[tokio::test]
async fn hour_only_update_leaves_timezone_and_enabled_state_untouched() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    assert_eq!(
        set_digest_raw(
            &admin,
            &app.address,
            serde_json::json!({ "enabled": true, "hour": 8, "timezone": "UTC" }),
        )
        .await
        .status(),
        200
    );

    let resp = set_digest_raw(
        &admin,
        &app.address,
        serde_json::json!({ "enabled": true, "hour": 19 }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["hour"], 19);
    assert_eq!(body["timezone"], "UTC");

    let row = admin_row(&app).await;
    assert_eq!(row.digest_hour, Some(19));
    assert_eq!(row.digest_timezone.as_deref(), Some("UTC"));
    assert!(row.digest_enabled);
}

#[tokio::test]
async fn out_of_range_hour_is_rejected() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    assert_eq!(
        set_digest_raw(
            &admin,
            &app.address,
            serde_json::json!({ "enabled": true, "hour": 8, "timezone": "UTC" }),
        )
        .await
        .status(),
        200
    );

    let resp = set_digest_raw(
        &admin,
        &app.address,
        serde_json::json!({ "enabled": true, "hour": 24 }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    assert_eq!(
        admin_row(&app).await.digest_hour,
        Some(8),
        "the row must be untouched by a rejected update"
    );
}

#[tokio::test]
async fn unrecognized_timezone_is_rejected() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let resp = set_digest_raw(
        &admin,
        &app.address,
        serde_json::json!({ "enabled": true, "hour": 8, "timezone": "Not/AZone" }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    assert!(admin_row(&app).await.digest_timezone.is_none());
}
