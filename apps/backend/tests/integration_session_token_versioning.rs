//! Integration tests for `users.session_token_version`: `POST /api/reset_password` bumps it,
//! which invalidates every session cookie issued before the reset. `change_password`'s own
//! version-bump behavior (including reissuing the calling session's own cookie) is covered in
//! `integration_change_password.rs`, next to the rest of that endpoint's tests.

mod common;
use common::default_org_id;
use common::{authed_client, spawn_app, test_api_key, user_with_role, ADMIN_EMAIL, ADMIN_PASSWORD};

use arcodash_core::entities::{api_key, user};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use serde_json::Value;

/// A session cookie issued before a mailed-token password reset must stop resolving once the
/// reset completes — `reset_password` has no session of its own to preserve (unauthenticated
/// call site), so unlike `change_password` it simply bumps the version with no reissue.
#[tokio::test]
async fn reset_password_invalidates_a_previously_issued_session() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let (session, csrf) = app.login(ADMIN_EMAIL, ADMIN_PASSWORD).await;
    let existing_session_client = authed_client(&app.address, &session, &csrf);

    let pre_check = existing_session_client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let pre_body: Value = pre_check.json().await.unwrap();
    assert!(
        !pre_body["user"].is_null(),
        "the session must be valid before the reset"
    );

    let bare = reqwest::Client::new();
    let forgot_resp = bare
        .post(format!("{}/api/forgot_password", app.address))
        .json(&serde_json::json!({ "email": ADMIN_EMAIL }))
        .send()
        .await
        .unwrap();
    assert_eq!(forgot_resp.status().as_u16(), 204);

    let u = user::Entity::find()
        .filter(user::Column::Email.eq(ADMIN_EMAIL))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let token = u.details["reset_token"].as_str().unwrap().to_string();

    let reset_resp = bare
        .post(format!("{}/api/reset_password", app.address))
        .json(&serde_json::json!({ "token": token, "password": "Br4ndNewPassword!" }))
        .send()
        .await
        .unwrap();
    assert_eq!(reset_resp.status().as_u16(), 204);

    let post_check = existing_session_client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let post_body: Value = post_check.json().await.unwrap();
    assert!(
        post_body["user"].is_null(),
        "a session issued before the reset must no longer resolve to a user"
    );
}

/// A password reset must revoke every API key the account can authenticate with — both the
/// dedicated `api_keys` table row (`Authorization: Key`) and the legacy `users.api_key_hash`
/// column (`?api_key=`) — closing the gap where a stolen key survives the one recovery action a
/// compromised user is told to take.
#[tokio::test]
async fn reset_password_revokes_existing_api_keys() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (target, _) = user_with_role(&app.db, org_id, &["view_query"]).await;

    // `user_with_role` seeds an SSO-style account (no password); give it one plus a reset
    // token, matching what `forgot_password` would have stamped onto `details`.
    let password_hash = arcodash_backend::auth::password::hash_password("OldPassw0rd!1")
        .await
        .unwrap();
    let mut active: user::ActiveModel = target.clone().into();
    active.password_hash = Set(Some(password_hash));
    active.details = Set(serde_json::json!({
        "reset_token": "reset-token-for-api-key-revocation-test",
        "reset_token_expires_at": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp(),
    }));
    active.update(&app.db).await.unwrap();

    // A dedicated `api_keys` row — the `Authorization: Key` path.
    let dedicated_key = format!("dedicated-{}", uuid::Uuid::new_v4().simple());
    api_key::Entity::insert(api_key::ActiveModel {
        org_id: Set(org_id),
        object_type: Set(arcodash_backend::auth::api_key::OBJECT_TYPE_USER.to_string()),
        object_id: Set(target.id),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(&dedicated_key)),
        active: Set(true),
        created_by_id: Set(None),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap();

    let bare = reqwest::Client::new();
    let legacy_url = format!(
        "{}/api/queries?api_key={}",
        app.address,
        test_api_key(&target.email)
    );

    let pre_dedicated = bare
        .get(format!("{}/api/queries", app.address))
        .header("Authorization", format!("Key {dedicated_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        pre_dedicated.status().as_u16(),
        200,
        "the dedicated api_keys row must authenticate before the reset"
    );

    let pre_legacy = bare.get(&legacy_url).send().await.unwrap();
    assert_eq!(
        pre_legacy.status().as_u16(),
        200,
        "the legacy users.api_key_hash column must authenticate before the reset"
    );

    let reset_resp = bare
        .post(format!("{}/api/reset_password", app.address))
        .json(&serde_json::json!({
            "token": "reset-token-for-api-key-revocation-test",
            "password": "Br4ndNewPassword!2"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(reset_resp.status().as_u16(), 204, "reset_password");

    let post_dedicated = bare
        .get(format!("{}/api/queries", app.address))
        .header("Authorization", format!("Key {dedicated_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        post_dedicated.status().as_u16(),
        401,
        "the dedicated api_keys row must be revoked by the reset"
    );

    let post_legacy = bare.get(&legacy_url).send().await.unwrap();
    assert_eq!(
        post_legacy.status().as_u16(),
        401,
        "the legacy users.api_key_hash column must be revoked by the reset"
    );

    let row = api_key::Entity::find()
        .filter(api_key::Column::ObjectId.eq(target.id))
        .filter(api_key::Column::ObjectType.eq(arcodash_backend::auth::api_key::OBJECT_TYPE_USER))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(!row.active, "the api_keys row must be marked inactive");
}

/// `regenerate_api_key` must revoke a previously-issued dedicated `api_keys` row the same way
/// `reset_password` does above — otherwise a key from before a rotation keeps authenticating even
/// though the caller believes "regenerate my key" replaced it. Unlike the reset_password test
/// above, `session_token_version` is untouched here: regenerating an API key isn't a
/// session-cookie event.
#[tokio::test]
async fn regenerate_api_key_revokes_a_previously_issued_dedicated_api_keys_row() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (target, _) = user_with_role(&app.db, org_id, &["view_query"]).await;

    // A dedicated `api_keys` row predating the regeneration — the `Authorization: Key` path.
    let dedicated_key = format!("dedicated-{}", uuid::Uuid::new_v4().simple());
    api_key::Entity::insert(api_key::ActiveModel {
        org_id: Set(org_id),
        object_type: Set(arcodash_backend::auth::api_key::OBJECT_TYPE_USER.to_string()),
        object_id: Set(target.id),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(&dedicated_key)),
        active: Set(true),
        created_by_id: Set(None),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .unwrap();

    let bare = reqwest::Client::new();
    let pre_dedicated = bare
        .get(format!("{}/api/queries", app.address))
        .header("Authorization", format!("Key {dedicated_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        pre_dedicated.status().as_u16(),
        200,
        "the dedicated api_keys row must authenticate before the regeneration"
    );

    let (session, csrf) = app.login(ADMIN_EMAIL, ADMIN_PASSWORD).await;
    let admin_client = authed_client(&app.address, &session, &csrf);
    let regen_resp = admin_client
        .post(format!(
            "{}/api/users/{}/regenerate_api_key",
            app.address, target.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(regen_resp.status().as_u16(), 200, "regenerate_api_key");
    let regen_body: Value = regen_resp.json().await.unwrap();
    let new_key = regen_body["api_key"]
        .as_str()
        .expect("regenerate_api_key must return the new key")
        .to_string();

    let post_dedicated = bare
        .get(format!("{}/api/queries", app.address))
        .header("Authorization", format!("Key {dedicated_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        post_dedicated.status().as_u16(),
        401,
        "the dedicated api_keys row must be revoked by regenerate_api_key"
    );

    let row = api_key::Entity::find()
        .filter(api_key::Column::ObjectId.eq(target.id))
        .filter(api_key::Column::ObjectType.eq(arcodash_backend::auth::api_key::OBJECT_TYPE_USER))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(!row.active, "the api_keys row must be marked inactive");

    // The freshly regenerated legacy key must itself work — proving the rotation didn't just
    // revoke the old row without leaving a usable replacement.
    let with_new_legacy_key = bare
        .get(format!("{}/api/queries?api_key={new_key}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        with_new_legacy_key.status().as_u16(),
        200,
        "the freshly regenerated legacy key must authenticate"
    );
}
