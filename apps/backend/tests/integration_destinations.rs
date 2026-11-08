//! Integration tests: notification destination CRUD. Reads require `view_destinations`, writes
//! `manage_destinations` (403 otherwise); secret `options` are encrypted at rest and decrypted in
//! `to_response`. Skips without `TEST_DATABASE_URL`.

mod common;
use common::{api_key_client, default_org_id};
use common::{authed_client, make_org, spawn_app, spawn_app_with_real_mailer, user_with_role};

/// Bare client with no cookies, no CSRF.
fn bare() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(false)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// The most recent event row for `(org_id, action)`, if any.
async fn latest_event(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    action: &str,
) -> Option<arcodash_core::entities::event::Model> {
    use sea_orm::{ColumnTrait, EntityTrait, Order, QueryFilter, QueryOrder};
    arcodash_core::entities::event::Entity::find()
        .filter(arcodash_core::entities::event::Column::OrgId.eq(org_id))
        .filter(arcodash_core::entities::event::Column::Action.eq(action))
        .order_by(
            arcodash_core::entities::event::Column::CreatedAt,
            Order::Desc,
        )
        .one(db)
        .await
        .unwrap()
}

/// Invite a new user via the admin client, accept, and log in; returns `(user_id, authed_client)`.
async fn invite_and_login(
    app: &common::TestApp,
    admin: &reqwest::Client,
    email: &str,
    name: &str,
    password: &str,
    group_ids: Vec<i64>,
) -> (i64, reqwest::Client) {
    let invite_resp = admin
        .post(format!("{}/api/users", app.address))
        .json(&serde_json::json!({ "name": name, "email": email, "groups": group_ids }))
        .send()
        .await
        .unwrap();
    let status = invite_resp.status().as_u16();
    let body_text = invite_resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "invite user: {body_text}");
    let invite_body: serde_json::Value = serde_json::from_str(&body_text)
        .unwrap_or_else(|e| panic!("invite body not JSON ({e}): {body_text}"));
    let user_id = invite_body["id"].as_i64().expect("invite must return id");

    let token = {
        use arcodash_core::entities::user;
        use sea_orm::EntityTrait;
        let u = user::Entity::find_by_id(user_id)
            .one(&app.db)
            .await
            .expect("db query failed")
            .expect("invited user must exist");
        u.details["invitation_token"]
            .as_str()
            .expect("invitation_token must be in details")
            .to_string()
    };

    let accept_client = bare();
    let sess = accept_client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = common::extract_set_cookie(sess.headers(), "csrf_token").unwrap();
    let accept_resp = accept_client
        .post(format!("{}/api/users/invitation/accept", app.address))
        .header("Cookie", format!("csrf_token={csrf}"))
        .header("X-CSRF-Token", &csrf)
        .json(&serde_json::json!({ "token": token, "name": name, "password": password }))
        .send()
        .await
        .unwrap();
    let status = accept_resp.status().as_u16();
    let body_text = accept_resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "accept invitation: {body_text}");

    let (session, csrf_login) = app.login(email, password).await;
    let client = authed_client(&app.address, &session, &csrf_login);
    (user_id, client)
}

/// Create a destination via the admin client; returns the full JSON body.
async fn create_destination(
    app: &common::TestApp,
    admin: &reqwest::Client,
    body: serde_json::Value,
) -> serde_json::Value {
    let resp = admin
        .post(format!("{}/api/destinations", app.address))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "create destination: {text}");
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn get_destination_types_returns_catalog() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .get(format!("{}/api/destinations/types", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let arr = body.as_array().expect("types must be an array");
    assert!(!arr.is_empty(), "types catalog must be non-empty");

    let types: Vec<&str> = arr.iter().filter_map(|t| t["type"].as_str()).collect();
    for expected in ["email", "slack", "webhook"] {
        assert!(
            types.contains(&expected),
            "types must include {expected}: {types:?}"
        );
    }
    for t in arr {
        assert!(t["name"].is_string(), "descriptor needs name: {t}");
        assert!(t["icon"].is_string(), "descriptor needs icon: {t}");
        assert!(t["secret"].is_array(), "descriptor needs secret array: {t}");
    }

    // Per-type secret field lists drive the frontend's password inputs and blank-on-edit stripping.
    let by_type = |ty: &str| arr.iter().find(|t| t["type"].as_str() == Some(ty)).unwrap();
    assert_eq!(
        by_type("slack")["secret"],
        serde_json::json!(["url"]),
        "slack secret fields"
    );
    assert_eq!(
        by_type("datadog")["secret"],
        serde_json::json!(["api_key", "app_key"]),
        "datadog secret fields"
    );
    assert_eq!(
        by_type("email")["secret"],
        serde_json::json!([]),
        "email has no secret fields"
    );
}

#[tokio::test]
async fn create_destination_as_admin_returns_200_with_icon() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let dst = create_destination(
        &app,
        &admin,
        serde_json::json!({ "name": "Ops Slack", "type": "slack", "options": { "url": "https://hooks.slack.com/T1" } }),
    )
    .await;

    assert!(dst["id"].as_i64().is_some());
    assert_eq!(dst["name"].as_str(), Some("Ops Slack"));
    assert_eq!(dst["type"].as_str(), Some("slack"));
    assert_eq!(
        dst["icon"].as_str(),
        Some("fa-slack"),
        "icon mapped by type"
    );
}

/// POST /api/destinations with an unrecognized `type` → 400.
#[tokio::test]
async fn create_destination_with_unknown_type_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .post(format!("{}/api/destinations", app.address))
        .json(
            &serde_json::json!({ "name": "bogus", "type": "not-a-real-dest-type", "options": {} }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "an unrecognized destination type must be rejected"
    );
}

/// A secret under a field the type's schema doesn't declare must never be stored or echoed back.
#[tokio::test]
async fn create_destination_strips_a_secret_sent_under_a_bogus_field_name() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let dst = create_destination(
        &app,
        &admin,
        serde_json::json!({
            "name": "Bogus Field Slack",
            "type": "slack",
            "options": { "url": "https://hooks.slack.com/T1", "not_a_real_slack_option": "SNEAKY_SECRET" }
        }),
    )
    .await;
    let dst_id = dst["id"].as_i64().unwrap();
    assert!(
        dst["options"].get("not_a_real_slack_option").is_none(),
        "create response must not echo a key outside the type's schema: {:?}",
        dst["options"]
    );

    let raw = {
        use arcodash_core::entities::notification_destination;
        use sea_orm::EntityTrait;
        let model = notification_destination::Entity::find_by_id(dst_id)
            .one(&app.db)
            .await
            .expect("db query failed")
            .expect("destination row must exist");
        model.options.expect("options column must be set")
    };
    let needle = b"SNEAKY_SECRET";
    assert!(
        !raw.windows(needle.len()).any(|w| w == needle),
        "a key outside the schema must be stripped before encryption, never stored at all"
    );
}

#[tokio::test]
async fn create_destination_as_non_admin_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let (_uid, user_client) = invite_and_login(
        &app,
        &admin,
        "dst-create@test.local",
        "Member",
        "P@ssw0rd!",
        vec![],
    )
    .await;

    let resp = user_client
        .post(format!("{}/api/destinations", app.address))
        .json(&serde_json::json!({ "name": "nope", "type": "email", "options": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403, "non-admin create must be 403");
}

/// A bare member can't read the destinations list: reads require `view_destinations`, not mere org
/// membership.
#[tokio::test]
async fn list_destinations_without_view_destinations_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let org_id = default_org_id(&app.db).await;
    let (user, _role_id) = user_with_role(&app.db, org_id, &[]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let resp = client
        .get(format!("{}/api/destinations", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "list without view_destinations must be 403"
    );
}

/// A member holding `view_destinations` can read the org-scoped list.
#[tokio::test]
async fn list_destinations_is_org_scoped_and_readable_with_view_destinations() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let dst = create_destination(
        &app,
        &admin,
        serde_json::json!({ "name": "Team Email", "type": "email", "options": { "to": "team@x.io" } }),
    )
    .await;
    let dst_id = dst["id"].as_i64().unwrap();

    let org_id = default_org_id(&app.db).await;
    let (user, _role_id) = user_with_role(&app.db, org_id, &["view_destinations"]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let resp = client
        .get(format!("{}/api/destinations", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "member with view_destinations can read destinations"
    );
    let list: serde_json::Value = resp.json().await.unwrap();
    let arr = list.as_array().expect("list must be array");
    assert!(
        arr.iter().any(|d| d["id"].as_i64() == Some(dst_id)),
        "created destination must appear in list: {list}"
    );

    // A destination in a second org never appears in this org's list.
    let other_org_id = make_org(&app.db).await;
    {
        use arcodash_core::entities::notification_destination;
        use sea_orm::{ActiveValue::*, EntityTrait};
        notification_destination::Entity::insert(notification_destination::ActiveModel {
            id: NotSet,
            org_id: Set(other_org_id),
            user_id: Set(None),
            name: Set("Other Org Destination".to_string()),
            r#type: Set("email".to_string()),
            options: Set(None),
            created_at: NotSet,
            updated_at: NotSet,
        })
        .exec(&app.db)
        .await
        .expect("other-org destination insert failed");
    }
    let list_again: serde_json::Value = client
        .get(format!("{}/api/destinations", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let arr_again = list_again.as_array().expect("list must be array");
    assert!(
        !arr_again
            .iter()
            .any(|d| d["name"].as_str() == Some("Other Org Destination")),
        "a different org's destination must never leak into this org's list: {list_again}"
    );
}

#[tokio::test]
async fn get_destination_by_id_and_cross_org_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let dst = create_destination(
        &app,
        &admin,
        serde_json::json!({ "name": "Hook", "type": "webhook", "options": { "url": "https://x.io/h" } }),
    )
    .await;
    let dst_id = dst["id"].as_i64().unwrap();

    let ok = admin
        .get(format!("{}/api/destinations/{dst_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status().as_u16(), 200);
    let body: serde_json::Value = ok.json().await.unwrap();
    assert_eq!(body["id"].as_i64(), Some(dst_id));

    let missing = admin
        .get(format!(
            "{}/api/destinations/{}",
            app.address,
            dst_id + 999_999
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        missing.status().as_u16(),
        404,
        "unknown/cross-org id must be 404"
    );
}

/// GET /api/destinations/:id by a user with no `view_destinations` → 403.
#[tokio::test]
async fn get_destination_without_view_destinations_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let dst = create_destination(
        &app,
        &admin,
        serde_json::json!({ "name": "Gated", "type": "email", "options": { "to": "a@x.io" } }),
    )
    .await;
    let dst_id = dst["id"].as_i64().unwrap();

    let org_id = default_org_id(&app.db).await;
    let (user, _role_id) = user_with_role(&app.db, org_id, &[]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let resp = client
        .get(format!("{}/api/destinations/{dst_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "get without view_destinations must be 403"
    );
}

/// GET /api/destinations/:id by a user holding `view_destinations` → 200.
#[tokio::test]
async fn get_destination_with_view_destinations_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let dst = create_destination(
        &app,
        &admin,
        serde_json::json!({ "name": "Ungated", "type": "email", "options": { "to": "a@x.io" } }),
    )
    .await;
    let dst_id = dst["id"].as_i64().unwrap();

    let org_id = default_org_id(&app.db).await;
    let (user, _role_id) = user_with_role(&app.db, org_id, &["view_destinations"]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let resp = client
        .get(format!("{}/api/destinations/{dst_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "get with view_destinations must be 200"
    );
}

#[tokio::test]
async fn update_destination_auth_boundaries() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let dst = create_destination(
        &app,
        &admin,
        serde_json::json!({ "name": "Before", "type": "email", "options": { "to": "a@x.io" } }),
    )
    .await;
    let dst_id = dst["id"].as_i64().unwrap();

    let upd = admin
        .post(format!("{}/api/destinations/{dst_id}", app.address))
        .json(&serde_json::json!({ "name": "After" }))
        .send()
        .await
        .unwrap();
    assert_eq!(upd.status().as_u16(), 200);
    let body: serde_json::Value = upd.json().await.unwrap();
    assert_eq!(
        body["name"].as_str(),
        Some("After"),
        "update must persist name"
    );

    let (_uid, user_client) = invite_and_login(
        &app,
        &admin,
        "dst-update@test.local",
        "Member",
        "P@ssw0rd!",
        vec![],
    )
    .await;
    let resp = user_client
        .post(format!("{}/api/destinations/{dst_id}", app.address))
        .json(&serde_json::json!({ "name": "Unauthorized Rename" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403, "non-admin update must be 403");
}

#[tokio::test]
async fn delete_destination_auth_boundaries() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let dst = create_destination(
        &app,
        &admin,
        serde_json::json!({ "name": "Doomed", "type": "email", "options": { "to": "a@x.io" } }),
    )
    .await;
    let dst_id = dst["id"].as_i64().unwrap();

    let (_uid, user_client) = invite_and_login(
        &app,
        &admin,
        "dst-delete@test.local",
        "Member",
        "P@ssw0rd!",
        vec![],
    )
    .await;
    let forbidden = user_client
        .delete(format!("{}/api/destinations/{dst_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        forbidden.status().as_u16(),
        403,
        "non-admin delete must be 403"
    );

    let deleted = admin
        .delete(format!("{}/api/destinations/{dst_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status().as_u16(), 204, "admin delete must be 204");

    let gone = admin
        .get(format!("{}/api/destinations/{dst_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        gone.status().as_u16(),
        404,
        "deleted destination must be 404"
    );
}

#[tokio::test]
async fn secret_options_roundtrip_and_encrypted_at_rest() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let secret_url = "https://hooks.slack.com/services/T00/B00/SuperSecretToken123";
    let dst = create_destination(
        &app,
        &admin,
        serde_json::json!({ "name": "Secret Slack", "type": "slack", "options": { "url": secret_url } }),
    )
    .await;
    let dst_id = dst["id"].as_i64().unwrap();

    // Create must redact the secret `url` (a slack destination's url IS the secret).
    assert!(
        dst["options"]["url"].as_str().is_none(),
        "create response must redact the secret url, got {:?}",
        dst["options"]
    );

    let got: serde_json::Value = admin
        .get(format!("{}/api/destinations/{dst_id}", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        got["options"]["url"].as_str().is_none(),
        "GET must redact the secret url, got {:?}",
        got["options"]
    );

    // Encryption at rest: the raw DB `options` bytes must NOT contain the plaintext secret...
    let raw = {
        use arcodash_core::entities::notification_destination;
        use sea_orm::EntityTrait;
        let model = notification_destination::Entity::find_by_id(dst_id)
            .one(&app.db)
            .await
            .expect("db query failed")
            .expect("destination row must exist");
        model.options.expect("options column must be set")
    };
    let needle = secret_url.as_bytes();
    assert!(
        !raw.windows(needle.len()).any(|w| w == needle),
        "stored options must be encrypted — plaintext secret leaked into the DB column"
    );

    // ...but still decryptable internally (the notifier needs it); redaction only affects API
    // responses.
    let key = arcodash_core::secrets::parse_key_hex(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .unwrap();
    let decrypted = arcodash_core::secrets::decrypt(&key, &raw).unwrap();
    let decrypted: serde_json::Value = serde_json::from_slice(&decrypted).unwrap();
    assert_eq!(
        decrypted["url"].as_str(),
        Some(secret_url),
        "stored ciphertext must still decrypt to the original secret internally"
    );
}

#[tokio::test]
async fn update_destination_merges_options_preserving_secret() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let secret_url = "https://hooks.slack.com/services/T00/B00/MergeTestSecret";
    // `mattermost`, not `webhook`: only it carries the Slack-style `username`/`icon_emoji` options.
    let dst = create_destination(
        &app,
        &admin,
        serde_json::json!({
            "name": "Mattermost To Edit",
            "type": "mattermost",
            "options": { "url": secret_url, "username": "old" }
        }),
    )
    .await;
    let dst_id = dst["id"].as_i64().unwrap();

    // Update with only `username`: `url` must be preserved.
    let upd = admin
        .post(format!("{}/api/destinations/{dst_id}", app.address))
        .json(&serde_json::json!({ "options": { "username": "new" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(upd.status().as_u16(), 200);

    let raw = {
        use arcodash_core::entities::notification_destination;
        use sea_orm::EntityTrait;
        let model = notification_destination::Entity::find_by_id(dst_id)
            .one(&app.db)
            .await
            .expect("db query failed")
            .expect("destination row must exist");
        model.options.expect("options column must be set")
    };
    let key = arcodash_core::secrets::parse_key_hex(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .unwrap();
    let decrypted = arcodash_core::secrets::decrypt(&key, &raw).unwrap();
    let decrypted: serde_json::Value = serde_json::from_slice(&decrypted).unwrap();
    assert_eq!(
        decrypted["url"].as_str(),
        Some(secret_url),
        "update omitting `url` must not wipe the existing secret"
    );
    assert_eq!(
        decrypted["username"].as_str(),
        Some("new"),
        "update must still apply the field that was sent"
    );
}

#[tokio::test]
async fn webhook_schema_no_longer_carries_slack_only_fields() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .get(format!("{}/api/destinations/types", app.address))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let webhook = body
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["type"].as_str() == Some("webhook"))
        .expect("webhook type must exist");

    let keys: Vec<&str> = webhook["configuration_schema"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec!["url"],
        "generic Webhook must only carry `url` — `username`/`icon_emoji` are Slack/Mattermost \
         incoming-webhook concepts, not generic webhook: {keys:?}"
    );

    // Mattermost mimics Slack's webhook API, so these must be untouched for it.
    let mattermost = body
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["type"].as_str() == Some("mattermost"))
        .unwrap();
    let mm_keys: std::collections::HashSet<&str> = mattermost["configuration_schema"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert!(mm_keys.contains("username"));
    assert!(mm_keys.contains("icon_emoji"));
}

#[tokio::test]
async fn destination_types_expose_required_fields() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .get(format!("{}/api/destinations/types", app.address))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let arr = body.as_array().unwrap();
    let by_type = |ty: &str| arr.iter().find(|t| t["type"].as_str() == Some(ty)).unwrap();

    assert_eq!(by_type("slack")["required"], serde_json::json!(["url"]));
    assert_eq!(by_type("webhook")["required"], serde_json::json!(["url"]));
    assert_eq!(by_type("email")["required"], serde_json::json!(["to"]));
    assert_eq!(
        by_type("pagerduty")["required"],
        serde_json::json!(["integration_key"])
    );
    assert_eq!(
        by_type("datadog")["required"],
        serde_json::json!(["api_key"])
    );
}

#[tokio::test]
async fn test_destination_inline_missing_field_returns_400_not_500() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .post(format!("{}/api/destinations/test", app.address))
        .json(&serde_json::json!({ "type": "slack", "options": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "a destination missing its required field must fail cleanly, not 500"
    );
    let body = resp.text().await.unwrap_or_default();
    assert!(
        body.contains("No URL is set for this destination."),
        "error must name the unset field, not the raw JSON key: {body}"
    );
}

#[tokio::test]
async fn test_destination_unreachable_webhook_url_returns_error_not_500() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    // A public address that answers on no HTTP service: a deterministic failure with no local
    // infra.
    let resp = admin
        .post(format!("{}/api/destinations/test", app.address))
        .json(&serde_json::json!({
            "type": "webhook",
            "options": { "url": "http://8.8.8.8:1/hook" }
        }))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.status().as_u16(), 500, "must not 500 on send failure");
    assert_ne!(resp.status().as_u16(), 404, "test endpoint must exist");
}

#[tokio::test]
async fn test_destination_private_webhook_url_is_ssrf_blocked() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .post(format!("{}/api/destinations/test", app.address))
        .json(&serde_json::json!({
            "type": "webhook",
            "options": { "url": "http://127.0.0.1:1/hook" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "a loopback webhook URL must be SSRF-blocked by default"
    );
}

/// `guard_url` only sees the original URL, so a webhook host could 302 into a blocked address;
/// redirects must be disabled at the client. The local server passes the guard but 302s to a
/// link-local metadata address, which must not be followed.
#[tokio::test]
async fn test_destination_webhook_redirect_to_private_address_is_not_followed() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    async fn redirect_to_metadata_ip() -> axum::response::Response {
        axum::response::Response::builder()
            .status(axum::http::StatusCode::FOUND)
            .header("Location", "http://169.254.169.254/latest/meta-data/")
            .body(axum::body::Body::empty())
            .unwrap()
    }
    let redirector =
        axum::Router::new().route("/hook", axum::routing::post(redirect_to_metadata_ip));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, redirector).await.unwrap();
    });

    let admin = app.admin_client().await;
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let resp = admin
        .post(format!("{}/api/destinations/test", app.address))
        .json(&serde_json::json!({
            "type": "webhook",
            "options": { "url": format!("http://127.0.0.1:{port}/hook") }
        }))
        .send()
        .await
        .unwrap();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);

    assert_eq!(
        resp.status().as_u16(),
        400,
        "an unfollowed redirect must be surfaced as a failure, never silently treated as delivered"
    );
}

/// A 4xx (revoked webhook, wrong key) is a valid HTTP response for `reqwest::send()`; without a
/// status check the test-send would report success on exactly the failure it exists to catch.
#[tokio::test]
async fn test_destination_target_4xx_response_is_reported_as_failure() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    async fn not_found() -> axum::http::StatusCode {
        axum::http::StatusCode::NOT_FOUND
    }
    let gone_webhook = axum::Router::new().route("/hook", axum::routing::post(not_found));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, gone_webhook).await.unwrap();
    });

    let admin = app.admin_client().await;
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let resp = admin
        .post(format!("{}/api/destinations/test", app.address))
        .json(&serde_json::json!({
            "type": "webhook",
            "options": { "url": format!("http://127.0.0.1:{port}/hook") }
        }))
        .send()
        .await
        .unwrap();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);

    assert_eq!(
        resp.status().as_u16(),
        400,
        "a 4xx response from the destination must not be reported as a successful send"
    );
}

/// A destination tested by id must use its stored, decrypted secret, not one retyped into the
/// request.
#[tokio::test]
async fn test_destination_by_id_uses_stored_options() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    async fn ok() -> axum::http::StatusCode {
        axum::http::StatusCode::OK
    }
    let receiver = axum::Router::new().route("/hook", axum::routing::post(ok));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, receiver).await.unwrap();
    });

    let admin = app.admin_client().await;
    let dst = create_destination(
        &app,
        &admin,
        serde_json::json!({
            "name": "Local Webhook",
            "type": "webhook",
            "options": { "url": format!("http://127.0.0.1:{port}/hook") }
        }),
    )
    .await;
    let dst_id = dst["id"].as_i64().unwrap();

    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let resp = admin
        .post(format!("{}/api/destinations/test", app.address))
        .json(&serde_json::json!({ "id": dst_id }))
        .send()
        .await
        .unwrap();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);

    assert_eq!(
        resp.status().as_u16(),
        200,
        "a secret never retyped in this request must still reach the destination"
    );

    let org_id = default_org_id(&app.db).await;
    let event = latest_event(&app.db, org_id, "destination.tested").await;
    assert!(
        event.is_some(),
        "a successful test send must record a destination.tested event"
    );
    let props = event.unwrap().additional_properties;
    assert_eq!(props["destination_id"], dst_id);
    assert_eq!(props["type_name"], "webhook");
}

/// The inline-options form has no reviewed destination behind it, so sending only to the caller's
/// own address avoids an open relay.
#[tokio::test]
async fn test_destination_inline_email_to_a_third_party_is_rejected() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .post(format!("{}/api/destinations/test", app.address))
        .json(&serde_json::json!({ "type": "email", "options": { "to": "attacker@example.com" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "an inline test to an address other than the caller's own must be rejected"
    );
    let body = resp.text().await.unwrap_or_default();
    assert!(
        body.contains("your own address"),
        "error must explain the recipient restriction: {body}"
    );
}

/// The inline form sent to the caller's own address must not be rejected on recipient grounds (a
/// real send or "SMTP not configured").
#[tokio::test]
async fn test_destination_inline_email_to_own_address_is_not_rejected_on_recipient() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let resp = admin
        .post(format!("{}/api/destinations/test", app.address))
        .json(&serde_json::json!({ "type": "email", "options": { "to": common::ADMIN_EMAIL.to_uppercase() } }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if status == 400 {
        assert!(
            !body.contains("your own address"),
            "a test to the caller's own address (case-insensitively) must not fail the \
             recipient check: {body}"
        );
        assert!(
            body.contains("SMTP is not configured"),
            "the only acceptable 400 here is the no-mailer outcome, got: {body}"
        );
    }
}

/// The by-id form reaches an already-reviewed destination and must work even with a third-party
/// recipient; the own-address restriction applies only to the inline form.
#[tokio::test]
async fn test_destination_by_id_email_to_a_third_party_still_reaches_send_test() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let dst = create_destination(
        &app,
        &admin,
        serde_json::json!({
            "name": "Third Party Email",
            "type": "email",
            "options": { "to": "third-party@example.com" }
        }),
    )
    .await;
    let dst_id = dst["id"].as_i64().unwrap();

    let resp = admin
        .post(format!("{}/api/destinations/test", app.address))
        .json(&serde_json::json!({ "id": dst_id }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert!(
        !body.contains("your own address"),
        "the by-id form must never apply the inline-only recipient restriction: {body}"
    );
    assert!(
        status == 200 || body.contains("SMTP is not configured"),
        "unexpected outcome for a stored third-party destination: {status} {body}"
    );
}

/// A destination whose secret was never (re-)saved must fail the by-id test send naming the unset
/// field, not a generic validation error.
#[tokio::test]
async fn test_destination_by_id_with_no_stored_url_names_the_unset_field() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let dst = create_destination(
        &app,
        &admin,
        serde_json::json!({
            "name": "Webhook To Be Blanked",
            "type": "webhook",
            "options": { "url": "http://8.8.8.8:1/hook" }
        }),
    )
    .await;
    let dst_id = dst["id"].as_i64().unwrap();

    let update_resp = admin
        .post(format!("{}/api/destinations/{dst_id}", app.address))
        .json(&serde_json::json!({ "options": { "url": "" } }))
        .send()
        .await
        .unwrap();
    let update_status = update_resp.status().as_u16();
    let update_body = update_resp.text().await.unwrap_or_default();
    assert_eq!(update_status, 200, "update destination: {update_body}");
    // `decrypt_for_response` strips `url` as a secret, so only the by-id test send below can prove
    // the blank survived the merge.

    let resp = admin
        .post(format!("{}/api/destinations/test", app.address))
        .json(&serde_json::json!({ "id": dst_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap_or_default();
    assert!(
        body.contains("No URL is set"),
        "error must name the unset field: {body}"
    );
}

/// A test send for `type: "email"` with no SMTP configured must report the failure, not a false
/// success.
#[tokio::test]
async fn test_destination_email_with_no_smtp_configured_returns_error_not_success() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    // The caller's own address, so this hits the no-SMTP path rather than the inline-recipient
    // guard.
    let resp = admin
        .post(format!("{}/api/destinations/test", app.address))
        .json(&serde_json::json!({ "type": "email", "options": { "to": common::ADMIN_EMAIL } }))
        .send()
        .await
        .unwrap();
    assert_ne!(
        resp.status().as_u16(),
        200,
        "an unconfigured mailer must not report a successful test send"
    );
    let body = resp.text().await.unwrap_or_default();
    assert!(
        body.contains("SMTP"),
        "error must name the missing SMTP config: {body}"
    );
}

/// POST /api/destinations/test without `manage_destinations` → 403, since a test reaches a real
/// third party.
#[tokio::test]
async fn test_destination_without_manage_destinations_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let org_id = default_org_id(&app.db).await;
    let (user, _role_id) = user_with_role(&app.db, org_id, &["view_destinations"]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let resp = client
        .post(format!("{}/api/destinations/test", app.address))
        .json(&serde_json::json!({ "type": "webhook", "options": { "url": "https://x.io/h" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

/// A published query + monitor pair owned by `admin`; returns the monitor's id.
async fn create_monitor_for_escalation(app: &common::TestApp, admin: &reqwest::Client) -> i64 {
    let ds = admin
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "esc-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    let ds_id = ds.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();

    let q = admin
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "esc-query",
            "query": "SELECT 1",
            "options": {}
        }))
        .send()
        .await
        .unwrap();
    let query_id = q.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();
    admin
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();

    let m = admin
        .post(format!("{}/api/monitors", app.address))
        .json(&serde_json::json!({
            "query_id": query_id,
            "name": "esc-monitor",
            "options": { "op": ">", "value": 1, "column": "count" },
        }))
        .send()
        .await
        .unwrap();
    m.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap()
}

/// Deleting a mid-chain destination must not leave a hole in the survivors' `position` sequence:
/// the worker's escalation walk stops at the first missing one.
#[tokio::test]
async fn delete_destination_mid_chain_renumbers_surviving_escalation_steps() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let monitor_id = create_monitor_for_escalation(&app, &admin).await;
    let dest_1 = create_destination(
        &app,
        &admin,
        serde_json::json!({ "name": "rung-1", "type": "webhook", "options": { "url": "https://x.io/1" } }),
    )
    .await["id"]
        .as_i64()
        .unwrap();
    let dest_2 = create_destination(
        &app,
        &admin,
        serde_json::json!({ "name": "rung-2", "type": "webhook", "options": { "url": "https://x.io/2" } }),
    )
    .await["id"]
        .as_i64()
        .unwrap();
    let dest_3 = create_destination(
        &app,
        &admin,
        serde_json::json!({ "name": "rung-3", "type": "webhook", "options": { "url": "https://x.io/3" } }),
    )
    .await["id"]
        .as_i64()
        .unwrap();

    let put_resp = admin
        .put(format!(
            "{}/api/monitors/{monitor_id}/escalation",
            app.address
        ))
        .json(&serde_json::json!([
            { "delay_minutes": 5, "destination_id": dest_1 },
            { "delay_minutes": 10, "destination_id": dest_2 },
            { "delay_minutes": 15, "destination_id": dest_3 },
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(put_resp.status().as_u16(), 200);

    let delete_resp = admin
        .delete(format!("{}/api/destinations/{dest_2}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(delete_resp.status().as_u16(), 204);

    let get_body = admin
        .get(format!(
            "{}/api/monitors/{monitor_id}/escalation",
            app.address
        ))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let rows = get_body.as_array().unwrap();
    assert_eq!(
        rows.len(),
        2,
        "the step referencing the deleted destination must be gone: {rows:?}"
    );
    assert_eq!(rows[0]["destination_id"], dest_1);
    assert_eq!(
        rows[1]["destination_id"], dest_3,
        "rung 3 must have closed the gap left by rung 2"
    );

    // The DTO doesn't expose `position`; assert the stored column so a gap (`[1, 3]`) can't hide
    // behind array order.
    use arcodash_core::entities::escalation_step;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
    let stored: Vec<(i64, i32)> = escalation_step::Entity::find()
        .filter(escalation_step::Column::MonitorId.eq(monitor_id))
        .order_by_asc(escalation_step::Column::Position)
        .all(&app.db)
        .await
        .unwrap()
        .into_iter()
        .map(|s| (s.destination_id, s.position))
        .collect();
    assert_eq!(
        stored,
        vec![(dest_1, 1), (dest_3, 2)],
        "surviving steps must be densely renumbered 1..n, not left with a gap"
    );
}

/// Drives a real SMTP send against Mailpit and confirms the message lands and a
/// `destination.tested` audit row exists.
#[tokio::test]
async fn test_destination_email_lands_in_mailpit_and_records_an_event() {
    let app = skip_if_none!(spawn_app_with_real_mailer().await);
    app.cleanup().await;

    let mailpit_base =
        std::env::var("MAILPIT_API_BASE").unwrap_or_else(|_| "http://localhost:8026".to_string());
    let http = reqwest::Client::new();
    if http
        .get(format!("{mailpit_base}/api/v1/info"))
        .send()
        .await
        .is_err()
    {
        eprintln!("skipping: Mailpit not reachable at {mailpit_base}");
        return;
    }

    let admin = app.admin_client().await;
    // An unsaved destination may only test-send to the caller's own address, so the recipient is
    // the shared admin. Messages already in Mailpit are noted first so "found" means a new one.
    let to = common::ADMIN_EMAIL.to_string();
    let message_ids = || async {
        let search: serde_json::Value = http
            .get(format!("{mailpit_base}/api/v1/search"))
            .query(&[("query", format!("to:{to}")), ("limit", "500".to_string())])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        search["messages"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["ID"].as_str().map(String::from))
                    .collect::<std::collections::HashSet<_>>()
            })
            .unwrap_or_default()
    };
    let before = message_ids().await;

    let resp = admin
        .post(format!("{}/api/destinations/test", app.address))
        .json(&serde_json::json!({ "type": "email", "options": { "to": to } }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "real SMTP send against Mailpit must succeed"
    );

    let mut found = false;
    for _ in 0..30 {
        if message_ids().await.difference(&before).next().is_some() {
            found = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    assert!(
        found,
        "the test message must actually land in Mailpit within ~9s"
    );

    let org_id = default_org_id(&app.db).await;
    let event = latest_event(&app.db, org_id, "destination.tested").await;
    assert!(
        event.is_some(),
        "a real test send must record a destination.tested event"
    );
}

#[tokio::test]
async fn email_destination_requires_valid_recipient_on_create_and_update() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    for opts in [
        serde_json::json!({}),
        serde_json::json!({ "to": "" }),
        serde_json::json!({ "to": "   " }),
        serde_json::json!({ "to": "not-an-email" }),
        serde_json::json!({ "to": "a@x.io, bad" }),
    ] {
        let resp = admin
            .post(format!("{}/api/destinations", app.address))
            .json(&serde_json::json!({ "name": "E", "type": "email", "options": opts }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400, "create with {opts}");
    }

    let dst = create_destination(
        &app,
        &admin,
        serde_json::json!({ "name": "E", "type": "email", "options": { "to": "a@x.io, b@y.co" } }),
    )
    .await;
    let url = format!(
        "{}/api/destinations/{}",
        app.address,
        dst["id"].as_i64().unwrap()
    );

    for opts in [
        serde_json::json!({ "to": "" }),
        serde_json::json!({ "to": "nope" }),
    ] {
        let resp = admin
            .post(&url)
            .json(&serde_json::json!({ "options": opts }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400, "update with {opts}");
    }

    let ok = admin
        .post(&url)
        .json(&serde_json::json!({ "options": { "to": "c@z.io" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status().as_u16(), 200);
}
