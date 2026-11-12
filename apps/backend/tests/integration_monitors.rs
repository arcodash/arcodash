//! Integration tests for monitors: CRUD, mute/unmute, subscriptions, and the access rules in
//! `monitor_service` (404 for no access/non-owner, 400 for bad options). Single-org config means
//! "cross-org" reduces to a nonexistent id, which hits the same org-scoped 404 path.

mod common;
use common::ok_json;
use common::{authed_client, spawn_app, TestApp};

/// Create a data source (required before creating a query).
async fn create_ds(client: &reqwest::Client, base: &str) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/data_sources"))
            .json(&serde_json::json!({ "name": "monitor-test-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

/// Create a query, publish it (monitors only target published queries), and return its id.
async fn create_query(client: &reqwest::Client, base: &str, ds_id: i64) -> i64 {
    let query_id = ok_json(
        client
            .post(format!("{base}/api/queries"))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Monitor Test Query",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await["id"]
        .as_i64()
        .unwrap();

    ok_json(
        client
            .post(format!("{base}/api/queries/{query_id}/publish"))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish query",
    )
    .await;

    query_id
}

/// Create a notification destination (admin-only) and return its id.
async fn create_destination(client: &reqwest::Client, base: &str, name: &str) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/destinations"))
            .json(&serde_json::json!({
                "name": name,
                "type": "email",
                "options": { "to": "ops@test.local" }
            }))
            .send()
            .await
            .unwrap(),
        "create destination",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

/// Standard monitor options that pass `validate_options`.
fn valid_options() -> serde_json::Value {
    serde_json::json!({ "op": ">", "value": 100, "column": "count" })
}

/// Create a monitor as `client` over `query_id`; returns the full response body.
async fn create_monitor(
    client: &reqwest::Client,
    base: &str,
    query_id: i64,
    name: &str,
) -> serde_json::Value {
    ok_json(
        client
            .post(format!("{base}/api/monitors"))
            .json(&serde_json::json!({
                "query_id": query_id,
                "name": name,
                "options": valid_options(),
            }))
            .send()
            .await
            .unwrap(),
        "create monitor",
    )
    .await
}

/// Invite a user (admin call) and return their id.
async fn invite_user(client: &reqwest::Client, base: &str, email: &str) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/users"))
            .json(&serde_json::json!({ "name": "Other User", "email": email, "groups": [] }))
            .send()
            .await
            .unwrap(),
        "invite user",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

/// Accept an invitation (via DB-extracted token) and log in; returns `(user_id, authed client)`.
async fn onboard_user(
    app: &TestApp,
    admin: &reqwest::Client,
    email: &str,
    password: &str,
) -> (i64, reqwest::Client) {
    let user_id = invite_user(admin, &app.address, email).await;

    use sea_orm::EntityTrait;
    let user = arcodash_core::entities::user::Entity::find_by_id(user_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let token = user.details["invitation_token"]
        .as_str()
        .unwrap()
        .to_string();

    let bare = reqwest::Client::builder()
        .cookie_store(false)
        .build()
        .unwrap();
    let accept = bare
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(&serde_json::json!({ "token": token, "name": "Other User", "password": password }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        accept.status().as_u16(),
        200,
        "accept invitation: {}",
        accept.text().await.unwrap_or_default()
    );

    let (session, csrf) = app.login(email, password).await;
    (user_id, authed_client(&app.address, &session, &csrf))
}

/// Grant `user_id` view access to `query_id` without making them owner or admin.
async fn grant_query_view(admin: &reqwest::Client, base: &str, query_id: i64, user_id: i64) {
    let resp = admin
        .post(format!("{base}/api/queries/{query_id}/acl"))
        .json(&serde_json::json!({ "user_id": user_id, "access_type": "view" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "grant query view");
}

#[tokio::test]
async fn create_monitor_valid_returns_200_with_stubs() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;

    let body = create_monitor(&client, &app.address, query_id, "High count monitor").await;

    assert!(!body["id"].is_null(), "monitor must have an id");
    assert_eq!(
        body["state"], "unknown",
        "fresh monitor state must be 'unknown'"
    );
    assert_eq!(body["name"], "High count monitor");
    assert_eq!(body["options"]["op"], ">");
    assert_eq!(body["query"]["id"], query_id, "query stub id");
    assert!(!body["query"]["name"].is_null(), "query stub name");
    assert!(!body["user"]["id"].is_null(), "user stub id");
    assert!(!body["user"]["email"].is_null(), "user stub email");
}

#[tokio::test]
async fn create_monitor_invalid_op_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;

    let resp = client
        .post(format!("{}/api/monitors", app.address))
        .json(&serde_json::json!({
            "query_id": query_id,
            "name": "Bad op",
            "options": { "op": "gt", "value": 1, "column": "c" },
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 400, "invalid op must be 400: {body}");
    assert!(
        body.contains("invalid op"),
        "error must mention 'invalid op'; got: {body}"
    );
}

/// Grant `user_id` `create_monitor` via a fresh role+group, isolating a test to the query-access
/// gate. Permissions are recomputed per request, so no re-login is needed.
async fn grant_create_monitor(admin: &reqwest::Client, base: &str, user_id: i64) {
    let role = ok_json(
        admin
            .post(format!("{base}/api/roles"))
            .json(&serde_json::json!({ "name": "Monitor Creator", "permissions": ["create_monitor"] }))
            .send()
            .await
            .unwrap(),
        "create create_monitor role",
    )
    .await;
    let role_id = role["id"].as_i64().unwrap();

    let group = ok_json(
        admin
            .post(format!("{base}/api/groups"))
            .json(&serde_json::json!({ "name": "monitor-creators" }))
            .send()
            .await
            .unwrap(),
        "create monitor-creators group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let assign = admin
        .post(format!("{base}/api/groups/{group_id}/roles"))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(assign.status().as_u16(), 200);

    let add_member = admin
        .post(format!("{base}/api/groups/{group_id}/members"))
        .json(&serde_json::json!({ "user_id": user_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(add_member.status().as_u16(), 200);
}

#[tokio::test]
async fn create_monitor_inaccessible_query_returns_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id).await;

    let (uid, other) = onboard_user(&app, &admin, "noaccess@test.local", "N0Acc3ss!Pass").await;
    // Grant create_monitor so this test isolates the query-access 404 path.
    grant_create_monitor(&admin, &app.address, uid).await;

    let resp = other
        .post(format!("{}/api/monitors", app.address))
        .json(&serde_json::json!({
            "query_id": query_id,
            "name": "Sneaky",
            "options": valid_options(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "monitor on inaccessible query must 404"
    );
}

/// A user without `create_monitor` gets 403 before the query-access check.
#[tokio::test]
async fn create_monitor_without_permission_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id).await;

    let (uid, other) = onboard_user(&app, &admin, "nocreate@test.local", "N0Cr3at3!Pass").await;
    grant_query_view(&admin, &app.address, query_id, uid).await;

    let resp = other
        .post(format!("{}/api/monitors", app.address))
        .json(&serde_json::json!({
            "query_id": query_id,
            "name": "Should Fail",
            "options": valid_options(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "monitor create without create_monitor permission must 403"
    );
}

#[tokio::test]
async fn list_monitors_filters_by_query_access() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id).await;
    create_monitor(&admin, &app.address, query_id, "Admin monitor").await;

    // Non-admin with no access to the query sees none.
    let (_uid, other) = onboard_user(&app, &admin, "lister@test.local", "L1st3r!Pass").await;
    let other_list: serde_json::Value = other
        .get(format!("{}/api/monitors", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        other_list.as_array().map(|a| a.len()).unwrap_or(usize::MAX),
        0,
        "non-admin without query access must see no monitors; got: {other_list}"
    );

    let admin_list: serde_json::Value = admin
        .get(format!("{}/api/monitors", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        admin_list.as_array().map(|a| a.len()).unwrap_or(0),
        1,
        "admin must see the monitor; got: {admin_list}"
    );
}

#[tokio::test]
async fn get_monitor_nonexistent_returns_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let resp = client
        .get(format!("{}/api/monitors/999999", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn update_monitor_by_non_owner_returns_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id).await;
    let monitor_id = create_monitor(&admin, &app.address, query_id, "Owned by admin").await["id"]
        .as_i64()
        .unwrap();

    // Can SEE the query but doesn't own the monitor and isn't admin: exercises
    // require_owner_or_admin.
    let (uid, other) = onboard_user(&app, &admin, "nonowner@test.local", "N0nOwn3r!Pass").await;
    grant_query_view(&admin, &app.address, query_id, uid).await;

    let resp = other
        .post(format!("{}/api/monitors/{monitor_id}", app.address))
        .json(&serde_json::json!({ "name": "Hijacked" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "non-owner non-admin update must 404"
    );
}

#[tokio::test]
async fn update_monitor_by_owner_persists_and_validates_op() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "Original").await["id"]
        .as_i64()
        .unwrap();

    let updated = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}", app.address))
            .json(&serde_json::json!({
                "name": "Renamed",
                "options": { "op": "<", "value": 5, "column": "errors" },
                "rearm": 600,
            }))
            .send()
            .await
            .unwrap(),
        "owner update",
    )
    .await;
    assert_eq!(updated["name"], "Renamed");
    assert_eq!(updated["options"]["op"], "<");
    assert_eq!(updated["rearm"], 600);

    let fetched = ok_json(
        client
            .get(format!("{}/api/monitors/{monitor_id}", app.address))
            .send()
            .await
            .unwrap(),
        "refetch after update",
    )
    .await;
    assert_eq!(fetched["name"], "Renamed");
    assert_eq!(fetched["options"]["op"], "<");

    let bad = client
        .post(format!("{}/api/monitors/{monitor_id}", app.address))
        .json(&serde_json::json!({ "options": { "op": "=>", "value": 1 } }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status().as_u16(), 400, "invalid op on update must 400");
}

#[tokio::test]
async fn update_monitor_options_array_returns_400_and_does_not_store() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "Options type guard").await
        ["id"]
        .as_i64()
        .unwrap();
    let original = ok_json(
        client
            .get(format!("{}/api/monitors/{monitor_id}", app.address))
            .send()
            .await
            .unwrap(),
        "initial fetch",
    )
    .await;

    let resp = client
        .post(format!("{}/api/monitors/{monitor_id}", app.address))
        .json(&serde_json::json!({ "options": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "an array options patch must be rejected, not silently stored"
    );

    let fetched = ok_json(
        client
            .get(format!("{}/api/monitors/{monitor_id}", app.address))
            .send()
            .await
            .unwrap(),
        "refetch after rejected update",
    )
    .await;
    assert_eq!(
        fetched["options"], original["options"],
        "rejected patch must not have changed the stored options"
    );
}

#[tokio::test]
async fn update_monitor_rearm_distinguishes_omitted_null_and_set() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "Rearm test").await["id"]
        .as_i64()
        .unwrap();

    let updated = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}", app.address))
            .json(&serde_json::json!({ "rearm": 900 }))
            .send()
            .await
            .unwrap(),
        "set rearm",
    )
    .await;
    assert_eq!(updated["rearm"], 900);

    let unchanged = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}", app.address))
            .json(&serde_json::json!({ "name": "Rearm test renamed" }))
            .send()
            .await
            .unwrap(),
        "omit rearm",
    )
    .await;
    assert_eq!(
        unchanged["rearm"], 900,
        "omitting rearm must leave the existing value unchanged"
    );

    let cleared = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}", app.address))
            .json(&serde_json::json!({ "rearm": null }))
            .send()
            .await
            .unwrap(),
        "clear rearm",
    )
    .await;
    assert_eq!(
        cleared["rearm"],
        serde_json::Value::Null,
        "explicit null must clear rearm, not leave it unchanged"
    );
}

#[tokio::test]
async fn mute_and_unmute_flips_muted_flag() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let created = create_monitor(&client, &app.address, query_id, "Mutable").await;
    let monitor_id = created["id"].as_i64().unwrap();
    let org_id = created["org_id"].as_i64().unwrap();

    let muted = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}/mute", app.address))
            .send()
            .await
            .unwrap(),
        "mute",
    )
    .await;
    assert_eq!(muted["options"]["muted"], true, "mute must set muted=true");
    assert!(
        latest_event(&app.db, org_id, "monitor.muted", monitor_id)
            .await
            .is_some(),
        "muting must record a monitor.muted event"
    );

    let unmuted = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}/unmute", app.address))
            .send()
            .await
            .unwrap(),
        "unmute",
    )
    .await;
    assert_eq!(
        unmuted["options"]["muted"], false,
        "unmute must set muted=false"
    );
    assert!(
        latest_event(&app.db, org_id, "monitor.unmuted", monitor_id)
            .await
            .is_some(),
        "unmuting must record a monitor.unmuted event"
    );
}

#[tokio::test]
async fn mute_repairs_an_options_column_already_corrupted_to_a_non_object() {
    use sea_orm::{ConnectionTrait, Statement};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "Corrupted options").await
        ["id"]
        .as_i64()
        .unwrap();

    // Simulates a corrupted row (non-object `options`).
    app.db
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE monitors SET options = '[]'::jsonb WHERE id = $1",
            [monitor_id.into()],
        ))
        .await
        .unwrap();

    let muted = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}/mute", app.address))
            .send()
            .await
            .unwrap(),
        "mute over a corrupted options column",
    )
    .await;
    assert_eq!(
        muted["options"],
        serde_json::json!({ "muted": true }),
        "the CASE guard must heal the corrupted array back into an object on this write"
    );
}

#[tokio::test]
async fn snooze_sets_and_clears_muted_until() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let created = create_monitor(&client, &app.address, query_id, "Snoozable").await;
    let monitor_id = created["id"].as_i64().unwrap();
    let org_id = created["org_id"].as_i64().unwrap();

    let snoozed = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}/snooze", app.address))
            .json(&serde_json::json!({ "minutes": 60 }))
            .send()
            .await
            .unwrap(),
        "snooze",
    )
    .await;
    let muted_until = snoozed["options"]["muted_until"]
        .as_str()
        .expect("muted_until must be a string after snoozing");
    let parsed =
        chrono::DateTime::parse_from_rfc3339(muted_until).expect("muted_until must be RFC3339");
    assert!(
        parsed > chrono::Utc::now(),
        "muted_until must be in the future"
    );
    assert!(
        latest_event(&app.db, org_id, "monitor.snoozed", monitor_id)
            .await
            .is_some(),
        "snoozing must record a monitor.snoozed event"
    );

    let cancelled = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}/snooze", app.address))
            .json(&serde_json::json!({ "minutes": 0 }))
            .send()
            .await
            .unwrap(),
        "cancel snooze",
    )
    .await;
    assert!(
        cancelled["options"]["muted_until"].is_null(),
        "minutes: 0 must clear muted_until back to null"
    );
    assert!(
        latest_event(&app.db, org_id, "monitor.snooze_cleared", monitor_id)
            .await
            .is_some(),
        "clearing a snooze must record a monitor.snooze_cleared event"
    );
}

/// A metadata edit with an `options` body silent on snooze/mute must not clobber a concurrent
/// snooze or mute: `options` is merged into the live row.
#[tokio::test]
async fn update_does_not_clobber_a_concurrent_snooze_or_mute() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "Clobber-check").await["id"]
        .as_i64()
        .unwrap();

    ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}/snooze", app.address))
            .json(&serde_json::json!({ "minutes": 60 }))
            .send()
            .await
            .unwrap(),
        "snooze",
    )
    .await;

    let updated = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}", app.address))
            .json(&serde_json::json!({
                "options": { "op": ">", "value": 999, "column": "count" },
            }))
            .send()
            .await
            .unwrap(),
        "metadata-only update",
    )
    .await;
    assert_eq!(
        updated["options"]["value"], 999,
        "the threshold edit itself must still take effect"
    );
    assert!(
        !updated["options"]["muted_until"].is_null(),
        "a metadata-only PATCH must not silently clear an active snooze"
    );

    let muted = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}/mute", app.address))
            .send()
            .await
            .unwrap(),
        "mute",
    )
    .await;
    assert_eq!(muted["options"]["muted"], true);

    let updated_again = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}", app.address))
            .json(&serde_json::json!({
                "options": { "op": "<", "value": 1, "column": "count" },
            }))
            .send()
            .await
            .unwrap(),
        "second metadata-only update",
    )
    .await;
    assert_eq!(
        updated_again["options"]["muted"], true,
        "a metadata-only PATCH must not silently un-mute"
    );

    // `muted`/`muted_until` are system-owned: a metadata PATCH echoing them back is dropped so it
    // can't un-mute behind the caller's back.
    let updated_with_mute_key = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}", app.address))
            .json(&serde_json::json!({
                "options": { "op": "<", "value": 1, "column": "count", "muted": false },
            }))
            .send()
            .await
            .unwrap(),
        "update that tries to set muted",
    )
    .await;
    assert_eq!(
        updated_with_mute_key["options"]["muted"], true,
        "a caller-supplied `muted` in a metadata PATCH must be ignored, not honored"
    );
}

/// A JSON `null` in an `options` PATCH deletes that key (merging alone can't remove one).
#[tokio::test]
async fn update_with_a_null_valued_option_deletes_the_key() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "Clearable-option").await
        ["id"]
        .as_i64()
        .unwrap();

    let with_wait = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}", app.address))
            .json(&serde_json::json!({
                "options": { "op": ">", "value": 100, "column": "count", "max_fresh_wait_secs": 30 },
            }))
            .send()
            .await
            .unwrap(),
        "set max_fresh_wait_secs",
    )
    .await;
    assert_eq!(with_wait["options"]["max_fresh_wait_secs"], 30);

    let cleared = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}", app.address))
            .json(&serde_json::json!({
                "options": { "op": ">", "value": 100, "column": "count", "max_fresh_wait_secs": null },
            }))
            .send()
            .await
            .unwrap(),
        "clear max_fresh_wait_secs",
    )
    .await;
    assert!(
        cleared["options"].get("max_fresh_wait_secs").is_none(),
        "a null-valued patch key must delete the stored key, not store a literal null: {cleared:?}"
    );

    let fetched = ok_json(
        client
            .get(format!("{}/api/monitors/{monitor_id}", app.address))
            .send()
            .await
            .unwrap(),
        "re-fetch after clear",
    )
    .await;
    assert!(fetched["options"].get("max_fresh_wait_secs").is_none());
}

/// Mute and snooze own disjoint `options` keys and write via JSONB merge, so concurrent requests
/// both survive.
#[tokio::test]
async fn concurrent_mute_and_snooze_both_survive() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "Concurrent-mute-snooze")
        .await["id"]
        .as_i64()
        .unwrap();

    let mute_req = client
        .post(format!("{}/api/monitors/{monitor_id}/mute", app.address))
        .send();
    let snooze_req = client
        .post(format!("{}/api/monitors/{monitor_id}/snooze", app.address))
        .json(&serde_json::json!({ "minutes": 60 }))
        .send();
    let (mute_resp, snooze_resp) = tokio::join!(mute_req, snooze_req);
    assert_eq!(mute_resp.unwrap().status().as_u16(), 200, "mute");
    assert_eq!(snooze_resp.unwrap().status().as_u16(), 200, "snooze");

    let current = ok_json(
        client
            .get(format!("{}/api/monitors/{monitor_id}", app.address))
            .send()
            .await
            .unwrap(),
        "get monitor after concurrent mute+snooze",
    )
    .await;
    assert_eq!(
        current["options"]["muted"], true,
        "the concurrent mute must have survived"
    );
    assert!(
        !current["options"]["muted_until"].is_null(),
        "the concurrent snooze must have survived, not been clobbered by the mute's write"
    );
}

/// Two concurrent ack requests on the same triggered monitor: exactly one succeeds (200), the other
/// loses (409).
#[tokio::test]
async fn concurrent_ack_exactly_one_succeeds() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "Concurrent-ack").await["id"]
        .as_i64()
        .unwrap();
    set_monitor_state(&app.db, monitor_id, "triggered").await;

    let ack_req_1 = client
        .post(format!("{}/api/monitors/{monitor_id}/ack", app.address))
        .send();
    let ack_req_2 = client
        .post(format!("{}/api/monitors/{monitor_id}/ack", app.address))
        .send();
    let (resp1, resp2) = tokio::join!(ack_req_1, ack_req_2);
    let statuses = [
        resp1.unwrap().status().as_u16(),
        resp2.unwrap().status().as_u16(),
    ];
    let successes = statuses.iter().filter(|&&s| s == 200).count();
    let conflicts = statuses.iter().filter(|&&s| s == 409).count();
    assert_eq!(
        (successes, conflicts),
        (1, 1),
        "exactly one concurrent ack must win (200), the other must lose (409); got {statuses:?}"
    );

    let current = ok_json(
        client
            .get(format!("{}/api/monitors/{monitor_id}", app.address))
            .send()
            .await
            .unwrap(),
        "get monitor after concurrent ack",
    )
    .await;
    assert!(
        !current["acked_at"].is_null(),
        "the winning ack must have actually recorded acked_at"
    );
}

/// `minutes: i64::MAX` must 400 (naming the bound) rather than panic the handler.
#[tokio::test]
async fn snooze_rejects_i64_max_without_panicking_and_keeps_serving() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "Overflow-check").await["id"]
        .as_i64()
        .unwrap();

    let resp = client
        .post(format!("{}/api/monitors/{monitor_id}/snooze", app.address))
        .json(&serde_json::json!({ "minutes": i64::MAX }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("1440") || message.to_lowercase().contains("day"),
        "the 400 must name the bound: {message}"
    );

    // A subsequent request succeeding proves the process is still alive.
    let followup = client
        .get(format!("{}/api/monitors/{monitor_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        followup.status().as_u16(),
        200,
        "the server must still be serving after the oversized snooze request"
    );
}

#[tokio::test]
async fn delete_monitor_owner_204_non_owner_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id).await;
    let monitor_id = create_monitor(&admin, &app.address, query_id, "ToDelete").await["id"]
        .as_i64()
        .unwrap();

    let (uid, other) = onboard_user(&app, &admin, "deleter@test.local", "D3l3t3r!Pass").await;
    grant_query_view(&admin, &app.address, query_id, uid).await;
    let non_owner = other
        .delete(format!("{}/api/monitors/{monitor_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        non_owner.status().as_u16(),
        404,
        "non-owner delete must 404"
    );

    let owner = admin
        .delete(format!("{}/api/monitors/{monitor_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(owner.status().as_u16(), 204, "owner delete must 204");

    let gone = admin
        .get(format!("{}/api/monitors/{monitor_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        gone.status().as_u16(),
        404,
        "monitor must be gone after delete"
    );
}

#[tokio::test]
async fn subscribe_with_bad_destination_404_null_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "Subbable").await["id"]
        .as_i64()
        .unwrap();

    // destination_id that does not exist in this org -> 404 'destination not found'.
    let bad = client
        .post(format!(
            "{}/api/monitors/{monitor_id}/subscriptions",
            app.address
        ))
        .json(&serde_json::json!({ "destination_id": 999999 }))
        .send()
        .await
        .unwrap();
    let bad_status = bad.status().as_u16();
    let bad_body = bad.text().await.unwrap_or_default();
    assert_eq!(bad_status, 404, "unknown destination must 404: {bad_body}");
    assert!(
        bad_body.contains("destination not found"),
        "error must mention 'destination not found'; got: {bad_body}"
    );

    let ok = ok_json(
        client
            .post(format!(
                "{}/api/monitors/{monitor_id}/subscriptions",
                app.address
            ))
            .json(&serde_json::json!({ "destination_id": null }))
            .send()
            .await
            .unwrap(),
        "subscribe with null destination",
    )
    .await;
    assert_eq!(ok["monitor_id"], monitor_id);
    assert!(
        ok["destination"].is_null(),
        "null destination -> no destination stub"
    );
    assert!(
        !ok["user"]["id"].is_null(),
        "subscription user stub populated"
    );
}

#[tokio::test]
async fn list_subscriptions_returns_existing_subs() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "WithSubs").await["id"]
        .as_i64()
        .unwrap();
    let dest_id = create_destination(&client, &app.address, "ops-email").await;

    let sub = ok_json(
        client
            .post(format!(
                "{}/api/monitors/{monitor_id}/subscriptions",
                app.address
            ))
            .json(&serde_json::json!({ "destination_id": dest_id }))
            .send()
            .await
            .unwrap(),
        "subscribe with destination",
    )
    .await;
    assert_eq!(
        sub["destination"]["id"], dest_id,
        "destination stub populated"
    );

    let list: serde_json::Value = client
        .get(format!(
            "{}/api/monitors/{monitor_id}/subscriptions",
            app.address
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        list.as_array().map(|a| a.len()).unwrap_or(0),
        1,
        "list must contain the one subscription; got: {list}"
    );
}

#[tokio::test]
async fn delete_subscription_nested_route_non_subscriber_404_subscriber_204() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id).await;
    let monitor_id = create_monitor(&admin, &app.address, query_id, "SubDelete").await["id"]
        .as_i64()
        .unwrap();

    let sub_id = ok_json(
        admin
            .post(format!(
                "{}/api/monitors/{monitor_id}/subscriptions",
                app.address
            ))
            .json(&serde_json::json!({ "destination_id": null }))
            .send()
            .await
            .unwrap(),
        "admin subscribe",
    )
    .await["id"]
        .as_i64()
        .unwrap();

    let (uid, other) = onboard_user(&app, &admin, "unsub@test.local", "Un5ub!Pass1").await;
    grant_query_view(&admin, &app.address, query_id, uid).await;
    let forbidden = other
        .delete(format!(
            "{}/api/monitors/{monitor_id}/subscriptions/{sub_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        forbidden.status().as_u16(),
        404,
        "non-subscriber delete must 404"
    );

    let ok = admin
        .delete(format!(
            "{}/api/monitors/{monitor_id}/subscriptions/{sub_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status().as_u16(), 204, "subscriber delete must 204");
}

#[tokio::test]
async fn delete_subscription_flat_route_204() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "FlatUnsub").await["id"]
        .as_i64()
        .unwrap();

    let sub_id = ok_json(
        client
            .post(format!(
                "{}/api/monitors/{monitor_id}/subscriptions",
                app.address
            ))
            .json(&serde_json::json!({ "destination_id": null }))
            .send()
            .await
            .unwrap(),
        "subscribe",
    )
    .await["id"]
        .as_i64()
        .unwrap();

    let resp = client
        .delete(format!(
            "{}/api/monitor_subscriptions/{sub_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204, "flat unsubscribe must 204");
}

#[tokio::test]
async fn duplicate_subscribe_returns_409_and_does_not_create_a_second_row() {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "DupeSub").await["id"]
        .as_i64()
        .unwrap();
    let dest_id = create_destination(&client, &app.address, "dupe-dest").await;

    // null-destination case (the default "Email" option): re-subscribing must not create duplicates
    let first_null = client
        .post(format!(
            "{}/api/monitors/{monitor_id}/subscriptions",
            app.address
        ))
        .json(&serde_json::json!({ "destination_id": null }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        first_null.status().as_u16(),
        200,
        "first null-dest subscribe must 200"
    );

    let second_null = client
        .post(format!(
            "{}/api/monitors/{monitor_id}/subscriptions",
            app.address
        ))
        .json(&serde_json::json!({ "destination_id": null }))
        .send()
        .await
        .unwrap();
    let second_null_status = second_null.status().as_u16();
    let second_null_body = second_null.text().await.unwrap_or_default();
    assert_eq!(
        second_null_status, 409,
        "duplicate null-dest subscribe must 409: {second_null_body}"
    );
    assert!(
        second_null_body.contains("already subscribed"),
        "409 body must explain why; got: {second_null_body}"
    );

    // -- explicit-destination case --
    let first_dest = client
        .post(format!(
            "{}/api/monitors/{monitor_id}/subscriptions",
            app.address
        ))
        .json(&serde_json::json!({ "destination_id": dest_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        first_dest.status().as_u16(),
        200,
        "first dest subscribe must 200"
    );

    let second_dest = client
        .post(format!(
            "{}/api/monitors/{monitor_id}/subscriptions",
            app.address
        ))
        .json(&serde_json::json!({ "destination_id": dest_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        second_dest.status().as_u16(),
        409,
        "duplicate dest subscribe must 409"
    );

    // Exactly one row per (monitor, user, destination) combination in the DB.
    let null_dest_rows = arcodash_core::entities::monitor_subscription::Entity::find()
        .filter(arcodash_core::entities::monitor_subscription::Column::MonitorId.eq(monitor_id))
        .filter(arcodash_core::entities::monitor_subscription::Column::DestinationId.is_null())
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        null_dest_rows.len(),
        1,
        "exactly one null-destination subscription row must exist"
    );

    let dest_rows = arcodash_core::entities::monitor_subscription::Entity::find()
        .filter(arcodash_core::entities::monitor_subscription::Column::MonitorId.eq(monitor_id))
        .filter(arcodash_core::entities::monitor_subscription::Column::DestinationId.eq(dest_id))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        dest_rows.len(),
        1,
        "exactly one destination-scoped subscription row must exist"
    );
}

/// `TEST_REDIS_URL`, same default as `common::build_test_state`.
fn redis_url() -> String {
    std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6380".to_string())
}

/// Drain (read + delete) every job on the `default` queue as JSON. Destructive by design so each
/// caller sees only its own jobs.
async fn drain_default_queue() -> Vec<serde_json::Value> {
    use redis::AsyncCommands;
    let mut conn = arcodash_core::valkey_client::connect(&redis_url())
        .await
        .expect("connect to test redis");
    let key = "arcodash:queue:default";
    let items: Vec<String> = conn.lrange(key, 0, -1).await.unwrap();
    let _: () = conn.del(key).await.unwrap();
    items
        .iter()
        .map(|s| serde_json::from_str(s).unwrap_or_else(|e| panic!("job not JSON ({e}): {s}")))
        .collect()
}

/// Give `user_id` a fresh role+group carrying exactly `perms`; reads need the matching `view_<obj>`
/// capability in addition to reach.
async fn grant_capability(db: &sea_orm::DatabaseConnection, user_id: i64, perms: &[&str]) {
    use arcodash_core::entities::{group, group_role, role, user};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

    let u = user::Entity::find_by_id(user_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(u.org_id),
        name: Set(format!("cap-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(perms.iter().map(|s| s.to_string()).collect()),
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id;
    let group_id = group::Entity::insert(group::ActiveModel {
        org_id: Set(u.org_id),
        name: Set(format!("cap-group-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec![]),
        r#type: Set("regular".to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id;
    group_role::Entity::insert(group_role::ActiveModel {
        group_id: Set(group_id),
        role_id: Set(role_id),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap();

    let mut groups = u.groups.clone();
    groups.push(group_id);
    let mut active: user::ActiveModel = u.into();
    active.groups = Set(groups);
    active.update(db).await.unwrap();
}

/// Set a monitor's `state` directly via the DB (no HTTP path sets it).
async fn set_monitor_state(db: &sea_orm::DatabaseConnection, monitor_id: i64, state: &str) {
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    let m = arcodash_core::entities::monitor::Entity::find_by_id(monitor_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut active: arcodash_core::entities::monitor::ActiveModel = m.into();
    active.state = Set(state.to_string());
    active.update(db).await.unwrap();
}

/// Flip a monitor's `options.muted` via the DB, preserving other option keys.
async fn set_monitor_muted(db: &sea_orm::DatabaseConnection, monitor_id: i64, muted: bool) {
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    let m = arcodash_core::entities::monitor::Entity::find_by_id(monitor_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut opts = m.options.clone();
    if let Some(obj) = opts.as_object_mut() {
        obj.insert("muted".to_string(), serde_json::json!(muted));
    }
    let mut active: arcodash_core::entities::monitor::ActiveModel = m.into();
    active.options = Set(opts);
    active.update(db).await.unwrap();
}

/// Set a monitor's `options.muted_until` to a future timestamp via the DB, preserving other option
/// keys.
async fn set_monitor_snoozed(db: &sea_orm::DatabaseConnection, monitor_id: i64) {
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    let m = arcodash_core::entities::monitor::Entity::find_by_id(monitor_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut opts = m.options.clone();
    let future = (chrono::Utc::now() + chrono::Duration::minutes(30)).to_rfc3339();
    if let Some(obj) = opts.as_object_mut() {
        obj.insert("muted_until".to_string(), serde_json::json!(future));
    }
    let mut active: arcodash_core::entities::monitor::ActiveModel = m.into();
    active.options = Set(opts);
    active.update(db).await.unwrap();
}

#[tokio::test]
async fn subscribe_to_triggered_monitor_enqueues_one_notification_for_the_new_sub() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "FiringNotify").await["id"]
        .as_i64()
        .unwrap();
    set_monitor_state(&app.db, monitor_id, "triggered").await;
    drain_default_queue().await; // clear anything stray before the action under test

    let sub = ok_json(
        client
            .post(format!(
                "{}/api/monitors/{monitor_id}/subscriptions",
                app.address
            ))
            .json(&serde_json::json!({ "destination_id": null }))
            .send()
            .await
            .unwrap(),
        "subscribe to triggered monitor",
    )
    .await;
    let sub_id = sub["id"].as_i64().unwrap();

    let jobs = drain_default_queue().await;
    assert_eq!(
        jobs.len(),
        1,
        "exactly one job must be enqueued for the new subscription; got: {jobs:?}"
    );
    let job = &jobs[0];
    assert_eq!(job["type"], "SendNotification");
    assert_eq!(job["monitor_id"], monitor_id);
    assert_eq!(job["subscription_id"], sub_id);
    assert_eq!(job["new_state"], "triggered");
    assert_eq!(job["attempt"], 1);
}

#[tokio::test]
async fn subscribe_to_non_triggered_monitor_enqueues_nothing() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;

    let unknown_id = create_monitor(&client, &app.address, query_id, "UnknownNotify").await["id"]
        .as_i64()
        .unwrap();
    let ok_id = create_monitor(&client, &app.address, query_id, "OkNotify").await["id"]
        .as_i64()
        .unwrap();
    set_monitor_state(&app.db, ok_id, "ok").await;
    drain_default_queue().await;

    for monitor_id in [unknown_id, ok_id] {
        let resp = client
            .post(format!(
                "{}/api/monitors/{monitor_id}/subscriptions",
                app.address
            ))
            .json(&serde_json::json!({ "destination_id": null }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "subscribe must still succeed");
    }

    let jobs = drain_default_queue().await;
    assert!(
        jobs.is_empty(),
        "ok/unknown monitors must never auto-notify on subscribe; got: {jobs:?}"
    );
}

#[tokio::test]
async fn subscribe_to_muted_triggered_monitor_enqueues_nothing() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "MutedFiring").await["id"]
        .as_i64()
        .unwrap();
    set_monitor_state(&app.db, monitor_id, "triggered").await;
    set_monitor_muted(&app.db, monitor_id, true).await;
    drain_default_queue().await;

    let resp = client
        .post(format!(
            "{}/api/monitors/{monitor_id}/subscriptions",
            app.address
        ))
        .json(&serde_json::json!({ "destination_id": null }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "subscribe must still succeed");

    let jobs = drain_default_queue().await;
    assert!(
        jobs.is_empty(),
        "a muted triggered monitor must not auto-notify on subscribe; got: {jobs:?}"
    );
}

#[tokio::test]
async fn subscribe_to_snoozed_triggered_monitor_enqueues_nothing() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "SnoozedFiring").await["id"]
        .as_i64()
        .unwrap();
    set_monitor_state(&app.db, monitor_id, "triggered").await;
    set_monitor_snoozed(&app.db, monitor_id).await;
    drain_default_queue().await;

    let resp = client
        .post(format!(
            "{}/api/monitors/{monitor_id}/subscriptions",
            app.address
        ))
        .json(&serde_json::json!({ "destination_id": null }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "subscribe must still succeed");

    let jobs = drain_default_queue().await;
    assert!(
        jobs.is_empty(),
        "a snoozed triggered monitor must not auto-notify on subscribe; got: {jobs:?}"
    );
}

#[tokio::test]
async fn notify_now_enqueues_for_the_monitors_current_state_any_state() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "SendNowOk").await["id"]
        .as_i64()
        .unwrap();
    // "ok" — deliberately NOT triggered, proving "Send now" isn't state-gated like subscribe.
    set_monitor_state(&app.db, monitor_id, "ok").await;

    let sub_id = ok_json(
        client
            .post(format!(
                "{}/api/monitors/{monitor_id}/subscriptions",
                app.address
            ))
            .json(&serde_json::json!({ "destination_id": null }))
            .send()
            .await
            .unwrap(),
        "subscribe",
    )
    .await["id"]
        .as_i64()
        .unwrap();
    drain_default_queue().await; // clear defensively

    let resp = client
        .post(format!(
            "{}/api/monitors/{monitor_id}/subscriptions/{sub_id}/notify",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204, "send now must 204");

    let jobs = drain_default_queue().await;
    assert_eq!(
        jobs.len(),
        1,
        "send now must enqueue exactly one job; got: {jobs:?}"
    );
    let job = &jobs[0];
    assert_eq!(job["type"], "SendNotification");
    assert_eq!(job["monitor_id"], monitor_id);
    assert_eq!(job["subscription_id"], sub_id);
    assert_eq!(
        job["new_state"], "ok",
        "must use the monitor's actual current state"
    );
}

#[tokio::test]
async fn notify_now_404s_for_nonexistent_monitor_or_subscription() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "SendNow404").await["id"]
        .as_i64()
        .unwrap();
    let sub_id = ok_json(
        client
            .post(format!(
                "{}/api/monitors/{monitor_id}/subscriptions",
                app.address
            ))
            .json(&serde_json::json!({ "destination_id": null }))
            .send()
            .await
            .unwrap(),
        "subscribe",
    )
    .await["id"]
        .as_i64()
        .unwrap();

    let bad_monitor = client
        .post(format!(
            "{}/api/monitors/999999/subscriptions/{sub_id}/notify",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        bad_monitor.status().as_u16(),
        404,
        "nonexistent monitor must 404"
    );

    let bad_sub = client
        .post(format!(
            "{}/api/monitors/{monitor_id}/subscriptions/999999/notify",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        bad_sub.status().as_u16(),
        404,
        "nonexistent subscription must 404"
    );

    // A subscription id belonging to a DIFFERENT monitor (the single-org stand-in for another org's
    // id).
    let other_monitor_id = create_monitor(&client, &app.address, query_id, "SendNowOtherMonitor")
        .await["id"]
        .as_i64()
        .unwrap();
    let cross = client
        .post(format!(
            "{}/api/monitors/{other_monitor_id}/subscriptions/{sub_id}/notify",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        cross.status().as_u16(),
        404,
        "a subscription belonging to a different monitor must 404, not send"
    );

    drain_default_queue().await;
    // Drain so nothing leaks into later tests.
}

/// Insert a `notification_deliveries` row directly; `created_at` is spread seconds apart so
/// newest-first ordering is unambiguous.
async fn seed_delivery(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    monitor_id: i64,
    status: &str,
    seconds_ago: i64,
) -> i64 {
    use sea_orm::ActiveModelTrait;
    use sea_orm::ActiveValue::Set;

    let created_at = sea_orm::prelude::DateTimeWithTimeZone::from(
        chrono::Utc::now() - chrono::Duration::seconds(seconds_ago),
    );
    let row = arcodash_core::entities::notification_delivery::ActiveModel {
        org_id: Set(org_id),
        monitor_id: Set(monitor_id),
        subscription_id: Set(None),
        destination_id: Set(None),
        state: Set("triggered".to_string()),
        status: Set(status.to_string()),
        error: Set(if status == "failed" {
            Some("destination configuration error".to_string())
        } else {
            None
        }),
        attempt_number: Set(1),
        created_at: Set(created_at),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    row.id
}

#[tokio::test]
async fn list_deliveries_org_scoped_and_shaped() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor = create_monitor(&client, &app.address, query_id, "WithDeliveries").await;
    let monitor_id = monitor["id"].as_i64().unwrap();
    let org_id = monitor["org_id"].as_i64().unwrap();

    // Nonexistent monitor id -> 404.
    let missing = client
        .get(format!("{}/api/monitors/999999/deliveries", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        missing.status().as_u16(),
        404,
        "an inaccessible/nonexistent monitor id must 404, not leak delivery rows"
    );

    let empty = ok_json(
        client
            .get(format!(
                "{}/api/monitors/{monitor_id}/deliveries",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list deliveries (empty)",
    )
    .await;
    assert_eq!(empty.as_array().unwrap().len(), 0);

    seed_delivery(&app.db, org_id, monitor_id, "sent", 120).await;
    let newer_id = seed_delivery(&app.db, org_id, monitor_id, "failed", 5).await;

    let list = ok_json(
        client
            .get(format!(
                "{}/api/monitors/{monitor_id}/deliveries",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list deliveries",
    )
    .await;
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 2, "both seeded deliveries returned");
    assert_eq!(
        rows[0]["id"], newer_id,
        "newest-first: the more recent 'failed' row comes first"
    );
    assert_eq!(rows[0]["status"], "failed");
    assert_eq!(rows[0]["error"], "destination configuration error");
    assert_eq!(rows[0]["attempt_number"], 1);
    assert_eq!(rows[0]["state"], "triggered");
    assert!(rows[0]["destination_id"].is_null());
    assert!(rows[0]["subscription_id"].is_null());
    assert!(!rows[0]["created_at"].as_str().unwrap().is_empty());
    assert_eq!(rows[1]["status"], "sent");
    assert!(rows[1]["error"].is_null());
}

/// A run's `notified[].destination_name` must distinguish the null-`destination_id` sentinel
/// (email-to-self) from a real destination that was later deleted.
#[tokio::test]
async fn run_notified_label_distinguishes_null_from_deleted_destination() {
    use sea_orm::ActiveModelTrait;
    use sea_orm::ActiveValue::Set;

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor = create_monitor(&client, &app.address, query_id, "RunNotifiedLabel").await;
    let monitor_id = monitor["id"].as_i64().unwrap();
    let org_id = monitor["org_id"].as_i64().unwrap();
    let dst_id = create_destination(&client, &app.address, "SoonGone").await;

    let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
    let run = arcodash_core::entities::run::ActiveModel {
        org_id: Set(org_id),
        entity_type: Set("monitor".to_string()),
        entity_id: Set(monitor_id),
        triggered_by: Set("manual".to_string()),
        status: Set("success".to_string()),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();

    // Email-to-self sentinel: destination_id NULL.
    arcodash_core::entities::notification_delivery::ActiveModel {
        org_id: Set(org_id),
        monitor_id: Set(monitor_id),
        subscription_id: Set(None),
        destination_id: Set(None),
        state: Set("triggered".to_string()),
        status: Set("sent".to_string()),
        attempt_number: Set(1),
        run_id: Set(Some(run.id)),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();

    // Real destination, deleted after the delivery was recorded.
    arcodash_core::entities::notification_delivery::ActiveModel {
        org_id: Set(org_id),
        monitor_id: Set(monitor_id),
        subscription_id: Set(None),
        destination_id: Set(Some(dst_id)),
        state: Set("triggered".to_string()),
        status: Set("sent".to_string()),
        attempt_number: Set(1),
        run_id: Set(Some(run.id)),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();
    let deleted = client
        .delete(format!("{}/api/destinations/{dst_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status().as_u16(), 204, "destination delete");

    let runs = ok_json(
        client
            .get(format!(
                "{}/api/runs?entity_type=monitor&entity_id={monitor_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list runs",
    )
    .await;
    let row = runs
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"].as_i64() == Some(run.id))
        .expect("seeded run present");
    let names: Vec<&str> = row["notified"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["destination_name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"Email (you)"),
        "null destination_id must render as 'Email (you)', got {names:?}"
    );
    assert!(
        names.contains(&"(deleted destination)"),
        "a deleted destination must still render as '(deleted destination)', got {names:?}"
    );
}

/// The most recent event row for `(org_id, action, object_id)`, if any.
async fn latest_event(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    action: &str,
    object_id: i64,
) -> Option<arcodash_core::entities::event::Model> {
    use sea_orm::{ColumnTrait, EntityTrait, Order, QueryFilter, QueryOrder};
    arcodash_core::entities::event::Entity::find()
        .filter(arcodash_core::entities::event::Column::OrgId.eq(org_id))
        .filter(arcodash_core::entities::event::Column::Action.eq(action))
        .filter(arcodash_core::entities::event::Column::ObjectId.eq(object_id))
        .order_by(
            arcodash_core::entities::event::Column::CreatedAt,
            Order::Desc,
        )
        .one(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn ack_requires_triggered_state_409_otherwise() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "AckNotTriggered").await["id"]
        .as_i64()
        .unwrap();

    let resp = client
        .post(format!("{}/api/monitors/{monitor_id}/ack", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        409,
        "ack must 409 when the monitor isn't currently triggered"
    );
}

#[tokio::test]
async fn ack_sets_acked_fields_and_records_event() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let created = create_monitor(&client, &app.address, query_id, "AckTriggered").await;
    let monitor_id = created["id"].as_i64().unwrap();
    let org_id = created["org_id"].as_i64().unwrap();
    set_monitor_state(&app.db, monitor_id, "triggered").await;

    let body = ok_json(
        client
            .post(format!("{}/api/monitors/{monitor_id}/ack", app.address))
            .send()
            .await
            .unwrap(),
        "ack",
    )
    .await;
    assert!(!body["acked_at"].is_null(), "acked_at must be set");
    assert!(
        body["acked_by_email"].as_str().is_some(),
        "acked_by_email must resolve to the acker's email, not a raw id"
    );

    let event = latest_event(&app.db, org_id, "monitor.acknowledged", monitor_id).await;
    assert!(
        event.is_some(),
        "acknowledging must record a monitor.acknowledged event"
    );
}

#[tokio::test]
async fn ack_twice_409s_on_second_call() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "AckTwice").await["id"]
        .as_i64()
        .unwrap();
    set_monitor_state(&app.db, monitor_id, "triggered").await;

    let first = client
        .post(format!("{}/api/monitors/{monitor_id}/ack", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status().as_u16(), 200, "first ack must succeed");

    let second = client
        .post(format!("{}/api/monitors/{monitor_id}/ack", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        second.status().as_u16(),
        409,
        "acking an already-acked monitor must 409"
    );
}

#[tokio::test]
async fn ack_is_visibility_scoped_not_owner_or_admin_only() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id).await;
    let monitor_id = create_monitor(&admin, &app.address, query_id, "AckVisibility").await["id"]
        .as_i64()
        .unwrap();
    set_monitor_state(&app.db, monitor_id, "triggered").await;

    // A non-owner with plain view access to the query can still ack (visibility mirrors
    // `get_for_user`, unlike mute/delete).
    let (other_id, other) =
        onboard_user(&app, &admin, "ack-viewer@test.local", "password123").await;
    grant_query_view(&admin, &app.address, query_id, other_id).await;
    grant_capability(&app.db, other_id, &["view_query", "view_monitor"]).await;

    let resp = other
        .post(format!("{}/api/monitors/{monitor_id}/ack", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "a non-owner with query view access must be able to ack: {}",
        resp.text().await.unwrap_or_default()
    );
}

#[tokio::test]
async fn ack_on_inaccessible_monitor_404s() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id).await;
    let monitor_id = create_monitor(&admin, &app.address, query_id, "AckInaccessible").await["id"]
        .as_i64()
        .unwrap();
    set_monitor_state(&app.db, monitor_id, "triggered").await;

    let (_other_id, other) =
        onboard_user(&app, &admin, "ack-blind@test.local", "password123").await;

    let resp = other
        .post(format!("{}/api/monitors/{monitor_id}/ack", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn get_escalation_empty_then_put_replaces_and_get_reflects_it() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let created = create_monitor(&client, &app.address, query_id, "EscalationChain").await;
    let monitor_id = created["id"].as_i64().unwrap();
    let org_id = created["org_id"].as_i64().unwrap();
    let dest_a = create_destination(&client, &app.address, "esc-dest-a").await;
    let dest_b = create_destination(&client, &app.address, "esc-dest-b").await;

    let empty = ok_json(
        client
            .get(format!(
                "{}/api/monitors/{monitor_id}/escalation",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "get escalation (empty)",
    )
    .await;
    assert_eq!(empty.as_array().unwrap().len(), 0);

    let put_resp = client
        .put(format!(
            "{}/api/monitors/{monitor_id}/escalation",
            app.address
        ))
        .json(&serde_json::json!([
            { "delay_minutes": 5, "destination_id": dest_a },
            { "delay_minutes": 15, "destination_id": dest_b },
        ]))
        .send()
        .await
        .unwrap();
    let put_body = ok_json(put_resp, "put escalation").await;
    let put_rows = put_body.as_array().unwrap();
    assert_eq!(put_rows.len(), 2);
    assert_eq!(put_rows[0]["delay_minutes"], 5);
    assert_eq!(put_rows[0]["destination_id"], dest_a);
    assert_eq!(put_rows[1]["delay_minutes"], 15);
    assert_eq!(put_rows[1]["destination_id"], dest_b);
    assert!(
        latest_event(&app.db, org_id, "monitor.escalation_updated", monitor_id)
            .await
            .is_some(),
        "replacing the escalation chain must record a monitor.escalation_updated event"
    );

    let get_body = ok_json(
        client
            .get(format!(
                "{}/api/monitors/{monitor_id}/escalation",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "get escalation (after put)",
    )
    .await;
    let get_rows = get_body.as_array().unwrap();
    assert_eq!(get_rows.len(), 2);
    assert_eq!(get_rows[0]["destination_id"], dest_a);
    assert_eq!(get_rows[1]["destination_id"], dest_b);

    let detail = ok_json(
        client
            .get(format!("{}/api/monitors/{monitor_id}", app.address))
            .send()
            .await
            .unwrap(),
        "get monitor detail",
    )
    .await;
    assert_eq!(detail["escalation"].as_array().unwrap().len(), 2);
    assert_eq!(detail["escalation_step"], 0);

    let cleared = ok_json(
        client
            .put(format!(
                "{}/api/monitors/{monitor_id}/escalation",
                app.address
            ))
            .json(&serde_json::json!([]))
            .send()
            .await
            .unwrap(),
        "put escalation (clear)",
    )
    .await;
    assert_eq!(cleared.as_array().unwrap().len(), 0);

    let get_after_clear = ok_json(
        client
            .get(format!(
                "{}/api/monitors/{monitor_id}/escalation",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "get escalation (after clear)",
    )
    .await;
    assert_eq!(get_after_clear.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn put_escalation_rejects_nonexistent_destination() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "EscalationBadDest").await
        ["id"]
        .as_i64()
        .unwrap();

    // A nonexistent destination id exercises the same org-scoped lookup a cross-org id would.
    let resp = client
        .put(format!(
            "{}/api/monitors/{monitor_id}/escalation",
            app.address
        ))
        .json(&serde_json::json!([{ "delay_minutes": 5, "destination_id": 999_999_999 }]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    let get_body = ok_json(
        client
            .get(format!(
                "{}/api/monitors/{monitor_id}/escalation",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "get escalation after rejected put",
    )
    .await;
    assert_eq!(get_body.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn create_monitor_auto_creates_a_default_schedule() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "AutoSchedule").await["id"]
        .as_i64()
        .unwrap();

    let schedules = ok_json(
        client
            .get(format!(
                "{}/api/schedules?entity_type=monitor&entity_id={monitor_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "get monitor's auto-created schedule",
    )
    .await;
    let rows = schedules.as_array().unwrap();
    assert_eq!(rows.len(), 1, "a new monitor must get exactly one schedule");
    assert_eq!(rows[0]["enabled"], true);
    assert_eq!(
        rows[0]["spec"],
        serde_json::json!({ "kind": "interval", "every_secs": 900 })
    );
    assert!(!rows[0]["next_run_at"].is_null());
}

#[tokio::test]
async fn deleting_a_monitor_deletes_its_schedule() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id, "ScheduleCleanup").await["id"]
        .as_i64()
        .unwrap();

    let del = client
        .delete(format!("{}/api/monitors/{monitor_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status().as_u16(), 204);

    // The schedule row is gone too, not orphaned.
    let resp = client
        .get(format!(
            "{}/api/schedules?entity_type=monitor&entity_id={monitor_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    use arcodash_core::entities::schedule;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let still_there = schedule::Entity::find()
        .filter(schedule::Column::EntityType.eq("monitor"))
        .filter(schedule::Column::EntityId.eq(monitor_id))
        .one(&app.db)
        .await
        .unwrap();
    assert!(
        still_there.is_none(),
        "schedule row must actually be deleted"
    );
}

#[tokio::test]
async fn list_monitors_query_id_filter_and_query_name_are_populated() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let q1 = create_query(&client, &app.address, ds_id).await;
    let q2 = create_query(&client, &app.address, ds_id).await;
    let m1 = create_monitor(&client, &app.address, q1, "OnQ1").await["id"]
        .as_i64()
        .unwrap();
    let _m2 = create_monitor(&client, &app.address, q2, "OnQ2").await["id"]
        .as_i64()
        .unwrap();

    let filtered = ok_json(
        client
            .get(format!("{}/api/monitors?query_id={q1}", app.address))
            .send()
            .await
            .unwrap(),
        "list monitors filtered by query_id",
    )
    .await;
    let rows = filtered.as_array().unwrap();
    assert_eq!(rows.len(), 1, "only Q1's monitor must be returned");
    assert_eq!(rows[0]["id"], m1);
    assert_eq!(rows[0]["query"]["id"], q1);
    assert!(
        !rows[0]["query"]["name"].is_null(),
        "query stub name must be populated"
    );
}

// `?data_source_id=` sits next to `?query_id=` as adjacent `Option<i64>` params of
// `monitor_service::list_for_user`; a swap compiles clean but returns every monitor. These tests
// exercise both filters independently and together.

#[tokio::test]
async fn list_monitors_data_source_id_filter_returns_only_that_data_sources_monitors() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    // Decoy data sources push ds ids away from query ids so a positional swap can't pass by numeric
    // coincidence.
    create_ds(&client, &app.address).await;
    create_ds(&client, &app.address).await;
    let ds_a = create_ds(&client, &app.address).await;
    let ds_b = create_ds(&client, &app.address).await;
    let q_a = create_query(&client, &app.address, ds_a).await;
    let q_b = create_query(&client, &app.address, ds_b).await;
    let m_a = create_monitor(&client, &app.address, q_a, "OnDsA").await["id"]
        .as_i64()
        .unwrap();
    let _m_b = create_monitor(&client, &app.address, q_b, "OnDsB").await["id"]
        .as_i64()
        .unwrap();

    let filtered = ok_json(
        client
            .get(format!(
                "{}/api/monitors?data_source_id={ds_a}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list monitors filtered by data_source_id",
    )
    .await;
    let rows = filtered.as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "only ds_a's monitor must be returned; got: {rows:?}"
    );
    assert_eq!(rows[0]["id"], m_a);
}

/// Guards the positional-swap hazard: `?query_id=` returns the expected set on a fixture that also
/// has a `data_source_id` in play.
#[tokio::test]
async fn list_monitors_query_id_filter_still_works_alongside_data_source_id_param() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_a = create_ds(&client, &app.address).await;
    let q1 = create_query(&client, &app.address, ds_a).await;
    let q2 = create_query(&client, &app.address, ds_a).await;
    let m1 = create_monitor(&client, &app.address, q1, "OnQ1Again").await["id"]
        .as_i64()
        .unwrap();
    let _m2 = create_monitor(&client, &app.address, q2, "OnQ2Again").await["id"]
        .as_i64()
        .unwrap();

    let filtered = ok_json(
        client
            .get(format!("{}/api/monitors?query_id={q1}", app.address))
            .send()
            .await
            .unwrap(),
        "list monitors filtered by query_id only",
    )
    .await;
    let rows = filtered.as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "query_id must still filter correctly; got: {rows:?}"
    );
    assert_eq!(rows[0]["id"], m1);
}

#[tokio::test]
async fn list_monitors_query_id_and_data_source_id_together_intersect() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    // Offset ds ids from query ids (see above) so a positional swap can't hide behind numeric
    // coincidence.
    create_ds(&client, &app.address).await;
    create_ds(&client, &app.address).await;
    let ds_a = create_ds(&client, &app.address).await;
    let ds_b = create_ds(&client, &app.address).await;
    let q_a1 = create_query(&client, &app.address, ds_a).await;
    let q_a2 = create_query(&client, &app.address, ds_a).await;
    let q_b1 = create_query(&client, &app.address, ds_b).await;
    let m_a1 = create_monitor(&client, &app.address, q_a1, "A1").await["id"]
        .as_i64()
        .unwrap();
    create_monitor(&client, &app.address, q_a2, "A2").await;
    create_monitor(&client, &app.address, q_b1, "B1").await;

    // query_id narrows to one monitor; data_source_id is a superset; the intersection must be
    // exactly one row.
    let filtered = ok_json(
        client
            .get(format!(
                "{}/api/monitors?query_id={q_a1}&data_source_id={ds_a}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list monitors filtered by both params",
    )
    .await;
    let rows = filtered.as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "both filters together must intersect; got: {rows:?}"
    );
    assert_eq!(rows[0]["id"], m_a1);

    // data_source_id alone (ds_b) must not include ds_a's monitors.
    let ds_b_only = ok_json(
        client
            .get(format!(
                "{}/api/monitors?data_source_id={ds_b}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list monitors filtered by data_source_id b",
    )
    .await;
    let rows = ds_b_only.as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "only ds_b's monitor must be returned; got: {rows:?}"
    );
}

#[tokio::test]
async fn list_monitors_unknown_data_source_id_returns_empty_not_error() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let q_id = create_query(&client, &app.address, ds_id).await;
    create_monitor(&client, &app.address, q_id, "SomeMonitor").await;

    let resp = client
        .get(format!(
            "{}/api/monitors?data_source_id=999999",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let rows: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 0);
}

/// Per-user monitor visibility still applies on top of the `data_source_id` filter.
#[tokio::test]
async fn list_monitors_data_source_id_filter_respects_caller_visibility() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id).await;
    create_monitor(&admin, &app.address, query_id, "AdminOnlyMonitor").await;

    let (_uid, other) =
        onboard_user(&app, &admin, "ds-filter-viewer@test.local", "V1ewer!Pass").await;
    let other_list: serde_json::Value = other
        .get(format!(
            "{}/api/monitors?data_source_id={ds_id}",
            app.address
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        other_list.as_array().map(|a| a.len()).unwrap_or(usize::MAX),
        0,
        "non-admin without query access must see no monitors even when data_source_id matches; got: {other_list}"
    );

    let admin_list: serde_json::Value = admin
        .get(format!(
            "{}/api/monitors?data_source_id={ds_id}",
            app.address
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(admin_list.as_array().map(|a| a.len()).unwrap_or(0), 1);
}

/// Create a role with `perms` and a group, add `user_id`, and restrict the group to `ds_ids` (empty
/// slice = unrestricted).
async fn grant_role_scoped_to(
    admin: &reqwest::Client,
    base: &str,
    user_id: i64,
    perms: &[&str],
    ds_ids: &[i64],
) {
    let role = ok_json(
        admin
            .post(format!("{base}/api/roles"))
            .json(&serde_json::json!({
                "name": format!("role-{}", uuid::Uuid::new_v4().simple()),
                "permissions": perms,
            }))
            .send()
            .await
            .unwrap(),
        "create role",
    )
    .await;
    let role_id = role["id"].as_i64().unwrap();

    let group = ok_json(
        admin
            .post(format!("{base}/api/groups"))
            .json(
                &serde_json::json!({ "name": format!("group-{}", uuid::Uuid::new_v4().simple()) }),
            )
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let assign = admin
        .post(format!("{base}/api/groups/{group_id}/roles"))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(assign.status().as_u16(), 200, "attach role to group");

    let add_member = admin
        .post(format!("{base}/api/groups/{group_id}/members"))
        .json(&serde_json::json!({ "user_id": user_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(add_member.status().as_u16(), 200, "add member to group");

    // New groups default to restricted with no allowlist (zero reach): widen for unrestricted, add
    // allowlist rows for restricted.
    let restrict = admin
        .put(format!("{base}/api/groups/{group_id}/data_source_access"))
        .json(&serde_json::json!({ "restricted": !ds_ids.is_empty() }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        restrict.status().as_u16(),
        200,
        "set group data-source posture"
    );
    for ds_id in ds_ids {
        let assign_ds = admin
            .post(format!("{base}/api/groups/{group_id}/data_sources"))
            .json(&serde_json::json!({ "data_source_id": ds_id }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            assign_ds.status().as_u16(),
            200,
            "assign data source to group"
        );
    }
}

/// `monitor_service::list_for_user` resolves query visibility in a batched pass that must match
/// per-row `query_service::get_for_user` for every reach shape (owned, workspace-published, hidden
/// by data source, shared via grant, restricted without grant).
#[tokio::test]
async fn monitor_list_visibility_is_unchanged_by_batching() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds1 = create_ds(&admin, &app.address).await;
    let ds2 = create_ds(&admin, &app.address).await;

    let (user_id, member) = onboard_user(&app, &admin, "monvis@test.local", "M0nV1s!Pass").await;
    grant_role_scoped_to(
        &admin,
        &app.address,
        user_id,
        &["view_monitor", "view_query", "create_monitor"],
        &[ds1],
    )
    .await;

    let q_a = create_query(&admin, &app.address, ds1).await;
    let monitor_a_id = create_monitor(&member, &app.address, q_a, "Owned By Caller").await["id"]
        .as_i64()
        .unwrap();

    let q_b = create_query(&admin, &app.address, ds1).await;
    let monitor_b_id = create_monitor(&admin, &app.address, q_b, "Workspace Published").await["id"]
        .as_i64()
        .unwrap();

    let q_c = create_query(&admin, &app.address, ds2).await;
    let monitor_c_id = create_monitor(&admin, &app.address, q_c, "DS Axis Hidden").await["id"]
        .as_i64()
        .unwrap();

    let q_d = create_query(&admin, &app.address, ds1).await;
    let monitor_d_id = create_monitor(&admin, &app.address, q_d, "Shared By Grant").await["id"]
        .as_i64()
        .unwrap();
    let set_vis_d = admin
        .put(format!(
            "{}/api/monitors/{monitor_d_id}/visibility",
            app.address
        ))
        .json(&serde_json::json!({ "visibility": "restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(set_vis_d.status().as_u16(), 204, "restrict monitor D");
    let grant_d = admin
        .post(format!("{}/api/monitors/{monitor_d_id}/acl", app.address))
        .json(&serde_json::json!({ "grantee_id": user_id, "access_type": "viewer" }))
        .send()
        .await
        .unwrap();
    assert_eq!(grant_d.status().as_u16(), 200, "grant monitor D to user");

    let q_e = create_query(&admin, &app.address, ds1).await;
    let monitor_e_id = create_monitor(&admin, &app.address, q_e, "Restricted Hidden").await["id"]
        .as_i64()
        .unwrap();
    let set_vis_e = admin
        .put(format!(
            "{}/api/monitors/{monitor_e_id}/visibility",
            app.address
        ))
        .json(&serde_json::json!({ "visibility": "restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(set_vis_e.status().as_u16(), 204, "restrict monitor E");

    let list: serde_json::Value = member
        .get(format!("{}/api/monitors", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<i64> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_i64().unwrap())
        .collect();

    assert!(
        ids.contains(&monitor_a_id),
        "monitor owned by the caller must be visible: {ids:?}"
    );
    assert!(
        ids.contains(&monitor_b_id),
        "workspace-published monitor with reachable query must be visible: {ids:?}"
    );
    assert!(
        !ids.contains(&monitor_c_id),
        "monitor whose query is out of data-source reach must be hidden: {ids:?}"
    );
    assert!(
        ids.contains(&monitor_d_id),
        "monitor shared via an explicit grant must be visible despite restricted visibility: {ids:?}"
    );
    assert!(
        !ids.contains(&monitor_e_id),
        "restricted monitor with no grant must be hidden: {ids:?}"
    );
}

/// The batched `caller` block must report exactly what a direct `GET /api/monitors/{id}` reports,
/// for owned and merely-visible monitors.
#[tokio::test]
async fn monitor_list_caller_blocks_are_unchanged_by_batching() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds1 = create_ds(&admin, &app.address).await;
    let (user_id, member) = onboard_user(&app, &admin, "monblocks@test.local", "M0nBlk!Pass").await;
    grant_role_scoped_to(
        &admin,
        &app.address,
        user_id,
        &[
            "view_monitor",
            "edit_monitor",
            "view_query",
            "create_monitor",
        ],
        &[],
    )
    .await;

    let q_owned = create_query(&admin, &app.address, ds1).await;
    let monitor_owned_id = create_monitor(&member, &app.address, q_owned, "User Owned").await["id"]
        .as_i64()
        .unwrap();

    let q_shared = create_query(&admin, &app.address, ds1).await;
    let monitor_shared_id = create_monitor(&admin, &app.address, q_shared, "Admin Owned Workspace")
        .await["id"]
        .as_i64()
        .unwrap();

    for id in [monitor_owned_id, monitor_shared_id] {
        let direct = ok_json(
            member
                .get(format!("{}/api/monitors/{id}", app.address))
                .send()
                .await
                .unwrap(),
            "direct get monitor",
        )
        .await;
        let list: serde_json::Value = member
            .get(format!("{}/api/monitors", app.address))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let row = list
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"].as_i64() == Some(id))
            .unwrap_or_else(|| panic!("monitor {id} present in list"));
        assert_eq!(
            direct["caller"], row["caller"],
            "list row caller must match direct GET for monitor {id}"
        );
        assert_eq!(
            direct["user"], row["user"],
            "list row owner stub must match direct GET for monitor {id}"
        );
    }

    // Sanity: owned vs visible must differ in `can_edit`, or the equality checks would pass
    // vacuously.
    let owned_direct = ok_json(
        member
            .get(format!("{}/api/monitors/{monitor_owned_id}", app.address))
            .send()
            .await
            .unwrap(),
        "direct get owned monitor",
    )
    .await;
    let shared_direct = ok_json(
        member
            .get(format!("{}/api/monitors/{monitor_shared_id}", app.address))
            .send()
            .await
            .unwrap(),
        "direct get shared monitor",
    )
    .await;
    assert_ne!(
        owned_direct["caller"]["can_edit"], shared_direct["caller"]["can_edit"],
        "ownership and a bare workspace-view reach must produce different can_edit values"
    );
}

/// `?filter=by-me` / `?filter=shared-with-me` must still narrow correctly after the visibility
/// fold.
#[tokio::test]
async fn monitor_list_scope_filters_still_work() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds1 = create_ds(&admin, &app.address).await;
    let (user_id, member) = onboard_user(&app, &admin, "monscope@test.local", "M0nScp!Pass").await;
    grant_role_scoped_to(
        &admin,
        &app.address,
        user_id,
        &["view_monitor", "view_query", "create_monitor"],
        &[],
    )
    .await;

    let q_mine = create_query(&admin, &app.address, ds1).await;
    let mine_id = create_monitor(&member, &app.address, q_mine, "Mine").await["id"]
        .as_i64()
        .unwrap();

    let q_shared = create_query(&admin, &app.address, ds1).await;
    let shared_id = create_monitor(&admin, &app.address, q_shared, "Shared").await["id"]
        .as_i64()
        .unwrap();
    let grant = admin
        .post(format!("{}/api/monitors/{shared_id}/acl", app.address))
        .json(&serde_json::json!({ "grantee_id": user_id, "access_type": "viewer" }))
        .send()
        .await
        .unwrap();
    assert_eq!(grant.status().as_u16(), 200, "grant shared monitor");

    let q_other = create_query(&admin, &app.address, ds1).await;
    let other_id = create_monitor(&admin, &app.address, q_other, "Admin's Own").await["id"]
        .as_i64()
        .unwrap();

    let by_me: serde_json::Value = member
        .get(format!("{}/api/monitors?filter=by-me", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let by_me_ids: Vec<i64> = by_me
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_i64().unwrap())
        .collect();
    assert_eq!(
        by_me_ids,
        vec![mine_id],
        "by-me must contain only the user's own monitor: {by_me_ids:?}"
    );

    let shared_with_me: serde_json::Value = member
        .get(format!(
            "{}/api/monitors?filter=shared-with-me",
            app.address
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let shared_ids: Vec<i64> = shared_with_me
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_i64().unwrap())
        .collect();
    assert_eq!(
        shared_ids,
        vec![shared_id],
        "shared-with-me must contain only the explicitly-granted monitor, not admin's own \
         unshared one: {shared_ids:?}"
    );
    assert!(
        !shared_ids.contains(&other_id),
        "an admin-owned monitor with no grant must never appear in shared-with-me"
    );
}

#[tokio::test]
async fn observations_route_returns_ascending_and_clamps_limit() {
    use sea_orm::ActiveModelTrait;
    use sea_orm::ActiveValue::{NotSet, Set};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor = create_monitor(&client, &app.address, query_id, "Obs").await;
    let monitor_id = monitor["id"].as_i64().unwrap();
    let org_id = monitor["org_id"].as_i64().unwrap();

    for i in 0..5i64 {
        arcodash_core::entities::monitor_observation::ActiveModel {
            id: NotSet,
            org_id: Set(org_id),
            monitor_id: Set(monitor_id),
            observed_at: Set(sea_orm::prelude::DateTimeWithTimeZone::from(
                chrono::Utc::now() - chrono::Duration::minutes(10 - i),
            )),
            value: Set(Some(i as f64)),
            state: Set("ok".to_string()),
            run_id: Set(None),
            query_result_id: Set(None),
            explanation: Set(None),
        }
        .insert(&app.db)
        .await
        .unwrap();
    }

    let url = format!("{}/api/monitors/{monitor_id}/observations", app.address);
    let all = ok_json(client.get(&url).send().await.unwrap(), "observations").await;
    let values: Vec<f64> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["value"].as_f64().unwrap())
        .collect();
    assert_eq!(values, vec![0.0, 1.0, 2.0, 3.0, 4.0], "oldest first");

    let two = ok_json(
        client.get(format!("{url}?limit=2")).send().await.unwrap(),
        "observations limit",
    )
    .await;
    let values: Vec<f64> = two
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["value"].as_f64().unwrap())
        .collect();
    assert_eq!(values, vec![3.0, 4.0], "newest N, still ascending");

    let zero = ok_json(
        client.get(format!("{url}?limit=0")).send().await.unwrap(),
        "observations limit 0",
    )
    .await;
    assert_eq!(zero.as_array().unwrap().len(), 1, "limit clamps up to 1");
}

#[tokio::test]
async fn create_baseline_monitor_round_trips_kind_and_sensitivity() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;

    let created = ok_json(
        client
            .post(format!("{}/api/monitors", app.address))
            .json(&serde_json::json!({
                "query_id": query_id,
                "name": "Baseline",
                "options": { "kind": "baseline", "column": "count", "sensitivity": "high" },
            }))
            .send()
            .await
            .unwrap(),
        "create baseline monitor",
    )
    .await;
    assert_eq!(created["options"]["kind"], "baseline");
    assert_eq!(created["options"]["sensitivity"], "high");

    let bad = client
        .post(format!("{}/api/monitors", app.address))
        .json(&serde_json::json!({
            "query_id": query_id,
            "name": "BadBaseline",
            "options": { "kind": "baseline", "op": ">", "value": 1, "column": "count" },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status().as_u16(), 400);
}

async fn insert_observation(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    monitor_id: i64,
    value: Option<f64>,
    state: &str,
    explanation: Option<&str>,
    age_mins: i64,
) {
    use arcodash_core::entities::monitor_observation;
    use sea_orm::{ActiveModelTrait, ActiveValue::NotSet, ActiveValue::Set};
    monitor_observation::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        monitor_id: Set(monitor_id),
        observed_at: Set((chrono::Utc::now() - chrono::Duration::minutes(age_mins)).into()),
        value: Set(value),
        state: Set(state.to_string()),
        run_id: Set(None),
        query_result_id: Set(None),
        explanation: Set(explanation.map(String::from)),
    }
    .insert(db)
    .await
    .unwrap();
}

async fn create_with_options(
    app: &TestApp,
    client: &reqwest::Client,
    options: serde_json::Value,
) -> serde_json::Value {
    let ds = create_ds(client, &app.address).await;
    let q = create_query(client, &app.address, ds).await;
    ok_json(
        client
            .post(format!("{}/api/monitors", app.address))
            .json(&serde_json::json!({ "query_id": q, "name": "m", "options": options }))
            .send()
            .await
            .unwrap(),
        "create monitor",
    )
    .await
}

#[tokio::test]
async fn baseline_monitor_reports_learning_until_seven() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let created = create_with_options(
        &app,
        &admin,
        serde_json::json!({ "column": "c", "kind": "baseline" }),
    )
    .await;
    let id = created["id"].as_i64().unwrap();
    let org_id = created["org_id"].as_i64().unwrap();
    assert_eq!(created["kind"], "baseline");
    assert_eq!(
        created["learning"],
        serde_json::json!({ "have": 0, "need": 7 })
    );

    for i in 0..3 {
        insert_observation(&app.db, org_id, id, Some(i as f64), "ok", None, 100 - i).await;
    }
    insert_observation(&app.db, org_id, id, None, "unevaluable", None, 50).await;
    let got = ok_json(
        admin
            .get(format!("{}/api/monitors/{id}", app.address))
            .send()
            .await
            .unwrap(),
        "get monitor",
    )
    .await;
    assert_eq!(got["learning"], serde_json::json!({ "have": 3, "need": 7 }));
    let list = ok_json(
        admin
            .get(format!("{}/api/monitors", app.address))
            .send()
            .await
            .unwrap(),
        "list monitors",
    )
    .await;
    let row = list["results"]
        .as_array()
        .map(|a| a.iter().find(|m| m["id"] == id).cloned())
        .unwrap_or_else(|| {
            list.as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == id)
                .cloned()
        })
        .unwrap();
    assert_eq!(row["learning"], serde_json::json!({ "have": 3, "need": 7 }));

    for i in 3..8 {
        insert_observation(&app.db, org_id, id, Some(i as f64), "ok", None, 100 - i).await;
    }
    let got = ok_json(
        admin
            .get(format!("{}/api/monitors/{id}", app.address))
            .send()
            .await
            .unwrap(),
        "get monitor",
    )
    .await;
    assert_eq!(got["kind"], "baseline");
    assert!(got["learning"].is_null());
}

#[tokio::test]
async fn threshold_monitor_has_no_learning() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let created = create_with_options(&app, &admin, valid_options()).await;
    assert_eq!(created["kind"], "threshold");
    assert!(created["learning"].is_null());
}

#[tokio::test]
async fn tile_incident_summary_first_line() {
    use arcodash_backend::services::monitor_service::tile_data;
    use sea_orm::EntityTrait;
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let created = create_with_options(
        &app,
        &admin,
        serde_json::json!({ "column": "c", "kind": "baseline" }),
    )
    .await;
    let id = created["id"].as_i64().unwrap();
    let org_id = created["org_id"].as_i64().unwrap();
    insert_observation(
        &app.db,
        org_id,
        id,
        Some(1.0),
        "triggered",
        Some("old\nline"),
        30,
    )
    .await;
    insert_observation(
        &app.db,
        org_id,
        id,
        Some(9.0),
        "triggered",
        Some("Spike to 9\n(z=4.1)"),
        5,
    )
    .await;

    let load = || async {
        arcodash_core::entities::monitor::Entity::find_by_id(id)
            .one(&app.db)
            .await
            .unwrap()
            .unwrap()
    };
    let tile = tile_data(&app.db, app.state.result_reader(), &[load().await])
        .await
        .unwrap()
        .remove(&id)
        .unwrap();
    assert_eq!(tile.incident_summary, None, "state is not triggered");
    assert_eq!(tile.kind, "baseline");

    set_monitor_state(&app.db, id, "triggered").await;
    let tile = tile_data(&app.db, app.state.result_reader(), &[load().await])
        .await
        .unwrap()
        .remove(&id)
        .unwrap();
    assert_eq!(tile.incident_summary.as_deref(), Some("Spike to 9"));
    assert_eq!(tile.learning.map(|l| l.have), Some(2));

    insert_observation(&app.db, org_id, id, None, "triggered", None, 1).await;
    let tile = tile_data(&app.db, app.state.result_reader(), &[load().await])
        .await
        .unwrap()
        .remove(&id)
        .unwrap();
    assert_eq!(tile.incident_summary.as_deref(), Some("Spike to 9"));
}
