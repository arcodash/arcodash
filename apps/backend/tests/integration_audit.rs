//! Integration tests: every Access-Control / delegatable-object mutation must leave an `events` row
//! (`GET /api/events`) with the acting user's id and the affected object's id.

mod common;
use common::ok_json;
use common::{extract_set_cookie, spawn_app, spawn_app_with, ADMIN_EMAIL};

/// Real IdP metadata (samael's SP/IdP interop vector) so `build_service_provider` can build an SP
/// without a live IdP; the signing cert is never exercised.
const SAML_IDP_METADATA_XML: &str = include_str!("fixtures/saml_idp_metadata.xml");

/// Fetches every `events` row for `action` via the admin-only `GET /api/events`, asserting at least
/// one exists.
async fn events_for_action(
    client: &reqwest::Client,
    base: &str,
    action: &str,
) -> Vec<serde_json::Value> {
    let list = ok_json(
        client
            .get(format!("{base}/api/events?action={action}"))
            .send()
            .await
            .unwrap(),
        &format!("list events for action={action}"),
    )
    .await;
    list.as_array()
        .unwrap_or_else(|| panic!("events response for {action} was not an array: {list}"))
        .clone()
}

/// Assert at least one `events` row for `action` matches `object_id` (and, if given, `user_id`).
async fn assert_event(
    client: &reqwest::Client,
    base: &str,
    action: &str,
    object_type: &str,
    object_id: i64,
    user_id: Option<i64>,
) {
    let events = events_for_action(client, base, action).await;
    let found = events.iter().any(|e| {
        e["object_type"] == object_type
            && e["object_id"].as_i64() == Some(object_id)
            && (user_id.is_none() || e["user_id"].as_i64() == user_id)
    });
    assert!(
        found,
        "expected an events row for action={action} object_type={object_type} object_id={object_id} user_id={user_id:?}; got {events:#?}"
    );
}

/// End-to-end: invite, group, role, data source and monitor mutations each leave the expected
/// `events` row.
#[tokio::test]
async fn mutations_across_access_control_objects_are_audited() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    // Admin user id, for asserting the acting user on each event.
    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let admin_id = session["user"]["id"].as_i64().expect("admin id");

    let email = format!("audit-{}@test.local", uuid::Uuid::new_v4().simple());
    let invited = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": "Audit Invitee", "email": email }))
            .send()
            .await
            .unwrap(),
        "invite user",
    )
    .await;
    let invited_id = invited["id"].as_i64().unwrap();
    assert_event(
        &admin,
        &app.address,
        "user.invited",
        "user",
        invited_id,
        Some(admin_id),
    )
    .await;

    let update_resp = admin
        .post(format!("{}/api/users/{invited_id}", app.address))
        .json(&serde_json::json!({ "name": "Audit Invitee Renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(update_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "user.updated",
        "user",
        invited_id,
        Some(admin_id),
    )
    .await;

    let disable_resp = admin
        .post(format!("{}/api/users/{invited_id}", app.address))
        .json(&serde_json::json!({ "disabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(disable_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "user.disabled",
        "user",
        invited_id,
        Some(admin_id),
    )
    .await;

    let enable_resp = admin
        .post(format!("{}/api/users/{invited_id}", app.address))
        .json(&serde_json::json!({ "disabled": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(enable_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "user.enabled",
        "user",
        invited_id,
        Some(admin_id),
    )
    .await;

    let regen_resp = admin
        .post(format!(
            "{}/api/users/{invited_id}/regenerate_api_key",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(regen_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "user.api_key_regenerated",
        "user",
        invited_id,
        Some(admin_id),
    )
    .await;

    let delete_resp = admin
        .delete(format!("{}/api/users/{invited_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(delete_resp.status().as_u16(), 204);
    assert_event(
        &admin,
        &app.address,
        "user.disabled",
        "user",
        invited_id,
        Some(admin_id),
    )
    .await;

    let group = ok_json(
        admin
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "audit-test-group" }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();
    assert_event(
        &admin,
        &app.address,
        "group.created",
        "group",
        group_id,
        Some(admin_id),
    )
    .await;

    let group_update_resp = admin
        .post(format!("{}/api/groups/{group_id}", app.address))
        .json(&serde_json::json!({ "name": "audit-test-group-renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(group_update_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "group.updated",
        "group",
        group_id,
        Some(admin_id),
    )
    .await;

    // Re-invite a fresh member (the previous invitee is now disabled).
    let member_email = format!("audit-member-{}@test.local", uuid::Uuid::new_v4().simple());
    let member = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": "Audit Member", "email": member_email }))
            .send()
            .await
            .unwrap(),
        "invite member",
    )
    .await;
    let member_id = member["id"].as_i64().unwrap();

    let add_member_resp = admin
        .post(format!("{}/api/groups/{group_id}/members", app.address))
        .json(&serde_json::json!({ "user_id": member_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(add_member_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "group.member_added",
        "group",
        group_id,
        Some(admin_id),
    )
    .await;

    let remove_member_resp = admin
        .delete(format!(
            "{}/api/groups/{group_id}/members/{member_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(remove_member_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "group.member_removed",
        "group",
        group_id,
        Some(admin_id),
    )
    .await;

    let role = ok_json(
        admin
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({ "name": "audit-test-role", "permissions": ["view_query"] }))
            .send()
            .await
            .unwrap(),
        "create role",
    )
    .await;
    let role_id = role["id"].as_i64().unwrap();
    assert_event(
        &admin,
        &app.address,
        "role.created",
        "role",
        role_id,
        Some(admin_id),
    )
    .await;

    let role_update_resp = admin
        .post(format!("{}/api/roles/{role_id}", app.address))
        .json(&serde_json::json!({ "name": "audit-test-role-renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(role_update_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "role.updated",
        "role",
        role_id,
        Some(admin_id),
    )
    .await;

    let attach_resp = admin
        .post(format!("{}/api/groups/{group_id}/roles", app.address))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(attach_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "group.role_attached",
        "group",
        group_id,
        Some(admin_id),
    )
    .await;

    let detach_resp = admin
        .delete(format!(
            "{}/api/groups/{group_id}/roles/{role_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(detach_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "group.role_detached",
        "group",
        group_id,
        Some(admin_id),
    )
    .await;

    let role_delete_resp = admin
        .delete(format!("{}/api/roles/{role_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(role_delete_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "role.deleted",
        "role",
        role_id,
        Some(admin_id),
    )
    .await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "audit-test-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();
    assert_event(
        &admin,
        &app.address,
        "data_source.created",
        "data_source",
        ds_id,
        Some(admin_id),
    )
    .await;

    let assign_ds_resp = admin
        .post(format!(
            "{}/api/groups/{group_id}/data_sources",
            app.address
        ))
        .json(&serde_json::json!({ "data_source_id": ds_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(assign_ds_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "group.data_source_assigned",
        "group",
        group_id,
        Some(admin_id),
    )
    .await;

    let remove_ds_resp = admin
        .delete(format!(
            "{}/api/groups/{group_id}/data_sources/{ds_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(remove_ds_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "group.data_source_removed",
        "group",
        group_id,
        Some(admin_id),
    )
    .await;

    let ds_update_resp = admin
        .post(format!("{}/api/data_sources/{ds_id}", app.address))
        .json(&serde_json::json!({ "name": "audit-test-ds-renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(ds_update_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "data_source.updated",
        "data_source",
        ds_id,
        Some(admin_id),
    )
    .await;

    let query = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Audit Test Query",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = query["id"].as_i64().unwrap();
    admin
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();

    let monitor = ok_json(
        admin
            .post(format!("{}/api/monitors", app.address))
            .json(&serde_json::json!({
                "query_id": query_id,
                "name": "audit-test-monitor",
                "options": { "op": ">", "value": 1, "column": "c" },
            }))
            .send()
            .await
            .unwrap(),
        "create monitor",
    )
    .await;
    let monitor_id = monitor["id"].as_i64().unwrap();
    assert_event(
        &admin,
        &app.address,
        "monitor.created",
        "monitor",
        monitor_id,
        Some(admin_id),
    )
    .await;

    let monitor_update_resp = admin
        .post(format!("{}/api/monitors/{monitor_id}", app.address))
        .json(&serde_json::json!({ "name": "audit-test-monitor-renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(monitor_update_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "monitor.updated",
        "monitor",
        monitor_id,
        Some(admin_id),
    )
    .await;

    let monitor_delete_resp = admin
        .delete(format!("{}/api/monitors/{monitor_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(monitor_delete_resp.status().as_u16(), 204);
    assert_event(
        &admin,
        &app.address,
        "monitor.deleted",
        "monitor",
        monitor_id,
        Some(admin_id),
    )
    .await;

    let destination = ok_json(
        admin
            .post(format!("{}/api/destinations", app.address))
            .json(&serde_json::json!({
                "name": "audit-test-destination",
                "type": "email",
                "options": { "to": "ops@test.local" }
            }))
            .send()
            .await
            .unwrap(),
        "create destination",
    )
    .await;
    let destination_id = destination["id"].as_i64().unwrap();
    assert_event(
        &admin,
        &app.address,
        "destination.created",
        "destination",
        destination_id,
        Some(admin_id),
    )
    .await;

    let dest_update_resp = admin
        .post(format!("{}/api/destinations/{destination_id}", app.address))
        .json(&serde_json::json!({ "name": "audit-test-destination-renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(dest_update_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "destination.updated",
        "destination",
        destination_id,
        Some(admin_id),
    )
    .await;

    let dest_delete_resp = admin
        .delete(format!("{}/api/destinations/{destination_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(dest_delete_resp.status().as_u16(), 204);
    assert_event(
        &admin,
        &app.address,
        "destination.deleted",
        "destination",
        destination_id,
        Some(admin_id),
    )
    .await;

    let query_delete_resp = admin
        .delete(format!("{}/api/queries/{query_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(query_delete_resp.status().as_u16(), 204);
    assert_event(
        &admin,
        &app.address,
        "query.deleted",
        "query",
        query_id,
        Some(admin_id),
    )
    .await;

    let ds_delete_resp = admin
        .delete(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(ds_delete_resp.status().as_u16(), 204);
    assert_event(
        &admin,
        &app.address,
        "data_source.deleted",
        "data_source",
        ds_id,
        Some(admin_id),
    )
    .await;
}

/// A user update that reassigns `groups` (the privilege-escalation surface) must record
/// before/after group lists in `user.updated`; a name-only change stays `{}`.
#[tokio::test]
async fn user_update_changing_groups_is_audited_with_before_after() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let admin_id = session["user"]["id"].as_i64().expect("admin id");

    let group = ok_json(
        admin
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "audit-groups-test-group" }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let email = format!("audit-groups-{}@test.local", uuid::Uuid::new_v4().simple());
    let invited = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": "Audit Groups Invitee", "email": email }))
            .send()
            .await
            .unwrap(),
        "invite user",
    )
    .await;
    let invited_id = invited["id"].as_i64().unwrap();
    assert_eq!(
        invited["groups"].as_array().map(|g| g.len()),
        Some(0),
        "freshly invited user must start with no groups"
    );

    let update_resp = admin
        .post(format!("{}/api/users/{invited_id}", app.address))
        .json(&serde_json::json!({ "groups": [group_id] }))
        .send()
        .await
        .unwrap();
    assert_eq!(update_resp.status().as_u16(), 200);

    let events = events_for_action(&admin, &app.address, "user.updated").await;
    let event = events
        .iter()
        .find(|e| e["object_type"] == "user" && e["object_id"].as_i64() == Some(invited_id))
        .unwrap_or_else(|| panic!("no user.updated event for user {invited_id}: {events:#?}"));
    assert_eq!(event["user_id"].as_i64(), Some(admin_id));

    let details = &event["additional_properties"];
    let before = details["before"]
        .as_array()
        .unwrap_or_else(|| panic!("missing/non-array 'before' in {details:#?}"));
    let after = details["after"]
        .as_array()
        .unwrap_or_else(|| panic!("missing/non-array 'after' in {details:#?}"));
    assert!(
        before.is_empty(),
        "before must reflect the pre-change (empty) group list: {before:?}"
    );
    assert_eq!(
        after,
        &vec![serde_json::json!(group_id)],
        "after must reflect the newly assigned group list"
    );
}

/// Each AI-settings mutation (`POST /api/organization/ai`) leaves its own audit row, and the
/// details never carry the key value.
#[tokio::test]
async fn ai_settings_mutations_are_audited_without_key_material() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let admin_id = session["user"]["id"].as_i64().expect("admin id");
    let org_id = session["org"]["id"].as_i64().expect("org id in session");

    // AI settings live on the shared org's `settings` JSONB, which `cleanup()` doesn't reset;
    // normalize to a known baseline first.
    let reset_resp = admin
        .post(format!("{}/api/organization/ai", app.address))
        .json(&serde_json::json!({
            "providers": { "openai": { "api_key": null, "model": null } },
            "provider_order": [],
            "fallback_enabled": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(reset_resp.status().as_u16(), 200);

    let secret_key = "sk-test-audit-secret-key-1234567890";

    let set_resp = admin
        .post(format!("{}/api/organization/ai", app.address))
        .json(&serde_json::json!({
            "providers": { "openai": { "api_key": secret_key, "model": "gpt-5.6-terra" } }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(set_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "ai_settings.provider_key_set",
        "organization",
        org_id,
        Some(admin_id),
    )
    .await;
    assert_event(
        &admin,
        &app.address,
        "ai_settings.model_changed",
        "organization",
        org_id,
        Some(admin_id),
    )
    .await;

    // No event's details ever contain the raw key.
    for action in ["ai_settings.provider_key_set", "ai_settings.model_changed"] {
        for e in events_for_action(&admin, &app.address, action).await {
            let details = e["additional_properties"].to_string();
            assert!(
                !details.contains(secret_key),
                "action={action} leaked key material: {details}"
            );
        }
    }

    let order_resp = admin
        .post(format!("{}/api/organization/ai", app.address))
        .json(&serde_json::json!({
            "provider_order": ["openai"],
            "fallback_enabled": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(order_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "ai_settings.provider_order_changed",
        "organization",
        org_id,
        Some(admin_id),
    )
    .await;
    assert_event(
        &admin,
        &app.address,
        "ai_settings.fallback_toggled",
        "organization",
        org_id,
        Some(admin_id),
    )
    .await;

    let clear_resp = admin
        .post(format!("{}/api/organization/ai", app.address))
        .json(&serde_json::json!({
            "providers": { "openai": { "api_key": null } }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(clear_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "ai_settings.provider_key_cleared",
        "organization",
        org_id,
        Some(admin_id),
    )
    .await;
}

/// `GET /api/events` is admin-only: a non-admin gets 403.
#[tokio::test]
async fn events_endpoint_is_admin_only() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let email = format!(
        "audit-nonadmin-{}@test.local",
        uuid::Uuid::new_v4().simple()
    );
    let invited = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": "Non Admin", "email": email }))
            .send()
            .await
            .unwrap(),
        "invite non-admin",
    )
    .await;
    let user_id = invited["id"].as_i64().unwrap();
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

    let accept_client = reqwest::Client::builder()
        .cookie_store(false)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let sess = accept_client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = common::extract_set_cookie(sess.headers(), "csrf_token").unwrap();
    let password = "P@ssw0rd!";
    let accept_resp = accept_client
        .post(format!("{}/api/users/invitation/accept", app.address))
        .header("Cookie", format!("csrf_token={csrf}"))
        .header("X-CSRF-Token", &csrf)
        .json(&serde_json::json!({ "token": token, "name": "Non Admin", "password": password }))
        .send()
        .await
        .unwrap();
    assert_eq!(accept_resp.status().as_u16(), 200);

    let (session, csrf_login) = app.login(&email, password).await;
    let non_admin = common::authed_client(&app.address, &session, &csrf_login);

    let resp = non_admin
        .get(format!("{}/api/events", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

/// `GET /api/events` resolves ids to display names/links at read time (`data_source.created` gets
/// name and link, `group.created` a name only); stored columns are untouched.
#[tokio::test]
async fn events_resolve_user_and_object_display_names() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let admin_id = session["user"]["id"].as_i64().expect("admin id");
    let admin_name = session["user"]["name"]
        .as_str()
        .expect("admin name")
        .to_string();

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "audit-names-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let group = ok_json(
        admin
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "audit-names-group" }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    let events = events_for_action(&admin, &app.address, "data_source.created").await;
    let ds_event = events
        .iter()
        .find(|e| e["object_id"].as_i64() == Some(ds_id))
        .unwrap_or_else(|| panic!("no data_source.created event for ds {ds_id}: {events:#?}"));
    assert_eq!(ds_event["user_id"].as_i64(), Some(admin_id));
    assert_eq!(ds_event["user_name"].as_str(), Some(admin_name.as_str()));
    assert_eq!(
        ds_event["object"]["name"].as_str(),
        Some("audit-names-ds"),
        "expected the data source's own name, not a raw id: {ds_event:#?}"
    );
    assert_eq!(
        ds_event["object"]["path"].as_str(),
        Some(format!("/data_sources/{ds_id}").as_str()),
        "data sources have a real per-id page, so a link must be present: {ds_event:#?}"
    );

    let group_events = events_for_action(&admin, &app.address, "group.created").await;
    let group_event = group_events
        .iter()
        .find(|e| e["object_id"].as_i64() == Some(group_id))
        .unwrap_or_else(|| {
            panic!("no group.created event for group {group_id}: {group_events:#?}")
        });
    assert_eq!(
        group_event["object"]["name"].as_str(),
        Some("audit-names-group")
    );
    assert!(
        group_event["object"]["path"].is_null(),
        "groups have no per-id page, so no link should be resolved: {group_event:#?}"
    );
}

/// The audit filter must accept the text the page shows via `formatAction` (`Data Source Created`),
/// not just the raw action.
#[tokio::test]
async fn events_action_filter_accepts_the_displayed_label() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "audit-filter-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let events = ok_json(
        admin
            .get(format!(
                "{}/api/events?action=Data%20Source%20Created",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list events filtered by the displayed label",
    )
    .await;
    let events = events.as_array().unwrap();
    assert!(
        !events.is_empty(),
        "expected at least one event for the displayed-label filter"
    );
    assert!(events.iter().all(|e| e["action"] == "data_source.created"));
    assert!(events
        .iter()
        .any(|e| e["object_id"].as_i64() == Some(ds_id)));
}

/// A partial word narrows the audit log without requiring the exact displayed label.
#[tokio::test]
async fn events_action_filter_matches_a_partial_word() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    admin
        .post(format!("{}/api/data_sources", app.address))
        .json(
            &serde_json::json!({ "name": "audit-filter-partial-ds", "type": "pg", "options": {} }),
        )
        .send()
        .await
        .unwrap();

    let events = ok_json(
        admin
            .get(format!("{}/api/events?action=source", app.address))
            .send()
            .await
            .unwrap(),
        "list events filtered by a partial word",
    )
    .await;
    let events = events.as_array().unwrap();
    assert!(
        !events.is_empty(),
        "expected at least one event matching the partial word 'source'"
    );
    assert!(events.iter().any(|e| e["action"] == "data_source.created"));
}

/// A `query_result` object resolves to the owning query's name and link, not `query_result#<id>`.
#[tokio::test]
async fn events_resolve_query_result_to_owning_query() {
    use arcodash_core::entities::{organization, query_result};
    use arcodash_core::events::record_event;
    use sea_orm::{
        ActiveModelTrait, ActiveValue::*, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter,
    };

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let admin_id = session["user"]["id"].as_i64().expect("admin id");

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "audit-qr-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let query = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id, "name": "audit-qr-query", "query": "select 1", "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = query["id"].as_i64().unwrap();

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();

    // No HTTP endpoint creates a `query_results` row outside the worker; seed one directly.
    let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
    let qr = query_result::ActiveModel {
        id: NotSet,
        org_id: Set(org.id),
        data_source_id: Set(ds_id),
        query_hash: Set("audit-test-hash".to_string()),
        query: Set("select 1".to_string()),
        data: Set(Some(serde_json::json!({ "columns": [], "rows": [] }))),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(Some(0.01)),
        retrieved_at: Set(now),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(&app.db)
    .await
    .expect("query_result insert failed");

    app.db
        .execute_unprepared(&format!(
            "UPDATE queries SET latest_query_data_id = {} WHERE id = {}",
            qr.id, query_id
        ))
        .await
        .expect("failed to point query at its latest result");

    record_event(
        &app.db,
        org.id,
        Some(admin_id),
        "query.executed",
        Some("query_result"),
        Some(qr.id),
        serde_json::json!({ "job_id": "test-job", "data_source_id": ds_id }),
    )
    .await
    .expect("record_event failed");

    let events = events_for_action(&admin, &app.address, "query.executed").await;
    let event = events
        .iter()
        .find(|e| e["object_id"].as_i64() == Some(qr.id))
        .unwrap_or_else(|| {
            panic!(
                "no query.executed event for query_result {}: {events:#?}",
                qr.id
            )
        });
    assert_eq!(
        event["object"]["name"].as_str(),
        Some("audit-qr-query"),
        "expected the owning query's name, not query_result#{}: {event:#?}",
        qr.id
    );
    assert_eq!(
        event["object"]["path"].as_str(),
        Some(format!("/queries/{query_id}").as_str()),
        "expected a link to the owning query: {event:#?}"
    );
}

/// A failed ad-hoc run records `execute_query_failed` with `object_type: "query"` and a real null
/// `object_id` (not an id rendered as `Query #null`).
#[tokio::test]
async fn events_report_null_object_id_for_ad_hoc_query_failure() {
    use arcodash_core::entities::organization;
    use arcodash_core::events::record_event;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let admin_id = session["user"]["id"].as_i64().expect("admin id");

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();

    // Mirrors the worker's failure arm when `job.query_id` is `None` (ad-hoc run).
    record_event(
        &app.db,
        org.id,
        Some(admin_id),
        "execute_query_failed",
        Some("query"),
        None,
        serde_json::json!({ "job_id": "test-job", "error": "connection refused" }),
    )
    .await
    .expect("record_event failed");

    let events = events_for_action(&admin, &app.address, "query.execution_failed").await;
    let event = events
        .iter()
        .find(|e| e["additional_properties"]["job_id"].as_str() == Some("test-job"))
        .unwrap_or_else(|| panic!("no execute_query_failed event recorded: {events:#?}"));

    assert_eq!(
        event["object_type"].as_str(),
        Some("query"),
        "object_type must still be reported: {event:#?}"
    );
    assert!(
        event["object_id"].is_null(),
        "object_id must be a real null, not coerced to a number: {event:#?}"
    );
    assert!(
        event["object"].is_null(),
        "there is no row to resolve: {event:#?}"
    );
}

/// An `embed` object resolves to the dashboard's name/page, or for a visualization embed its owning
/// query's.
#[tokio::test]
async fn events_resolve_embed_to_owning_dashboard_or_query() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "audit-embed-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let query = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id, "name": "audit-embed-query", "query": "select 1", "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = query["id"].as_i64().unwrap();
    let publish_resp = admin
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(publish_resp.status().as_u16(), 200);

    let viz = ok_json(
        admin
            .post(format!("{}/api/visualizations", app.address))
            .json(&serde_json::json!({ "query_id": query_id, "type": "chart", "name": "V", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create visualization",
    )
    .await;
    let viz_id = viz["id"].as_i64().unwrap();

    let dashboard = ok_json(
        admin
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "audit-embed-dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    let dashboard_id = dashboard["id"].as_i64().unwrap();
    let dashboard_slug = dashboard["slug"].as_str().unwrap().to_string();

    let widget_resp = admin
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": dashboard_id, "visualization_id": viz_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(widget_resp.status().as_u16(), 200);

    let publish_dash_resp = admin
        .post(format!(
            "{}/api/dashboards/{dashboard_slug}/publish",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(publish_dash_resp.status().as_u16(), 200);

    let dash_embed = ok_json(
        admin
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({ "object_type": "dashboard", "object_id": dashboard_id, "anonymous": true }))
            .send()
            .await
            .unwrap(),
        "create dashboard embed",
    )
    .await;
    let dash_embed_id = dash_embed["id"].as_i64().unwrap();

    let viz_embed = ok_json(
        admin
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({ "object_type": "visualization", "object_id": viz_id, "anonymous": true }))
            .send()
            .await
            .unwrap(),
        "create visualization embed",
    )
    .await;
    let viz_embed_id = viz_embed["id"].as_i64().unwrap();

    let events = events_for_action(&admin, &app.address, "embed.created").await;

    let dash_event = events
        .iter()
        .find(|e| e["object_id"].as_i64() == Some(dash_embed_id))
        .unwrap_or_else(|| panic!("no embed.created event for embed {dash_embed_id}: {events:#?}"));
    assert_eq!(
        dash_event["object"]["name"].as_str(),
        Some("audit-embed-dashboard")
    );
    assert_eq!(
        dash_event["object"]["path"].as_str(),
        Some(format!("/dashboard/{dashboard_slug}").as_str())
    );

    let viz_event = events
        .iter()
        .find(|e| e["object_id"].as_i64() == Some(viz_embed_id))
        .unwrap_or_else(|| panic!("no embed.created event for embed {viz_embed_id}: {events:#?}"));
    assert_eq!(
        viz_event["object"]["name"].as_str(),
        Some("audit-embed-query"),
        "a visualization embed resolves to its owning query's name: {viz_event:#?}"
    );
    assert_eq!(
        viz_event["object"]["path"].as_str(),
        Some(format!("/queries/{query_id}").as_str())
    );
}

/// `object.visibility_changed` on a dashboard resolves to the dashboard's name and page.
#[tokio::test]
async fn events_resolve_dashboard_visibility_change_to_live_dashboard() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let dashboard = ok_json(
        admin
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "audit-visibility-dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    let dashboard_id = dashboard["id"].as_i64().unwrap();
    let dashboard_slug = dashboard["slug"].as_str().unwrap().to_string();

    let visibility_resp = admin
        .put(format!(
            "{}/api/dashboards/{dashboard_id}/visibility",
            app.address
        ))
        .json(&serde_json::json!({ "visibility": "restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(visibility_resp.status().as_u16(), 204);

    let events = events_for_action(&admin, &app.address, "object.visibility_changed").await;
    let event = events
        .iter()
        .find(|e| e["object_id"].as_i64() == Some(dashboard_id))
        .unwrap_or_else(|| {
            panic!("no object.visibility_changed event for dashboard {dashboard_id}: {events:#?}")
        });
    assert_eq!(event["object_type"].as_str(), Some("dashboard"));
    assert_eq!(
        event["object"]["name"].as_str(),
        Some("audit-visibility-dashboard"),
        "a live dashboard must resolve to its real name, not null: {event:#?}"
    );
    assert_eq!(
        event["object"]["path"].as_str(),
        Some(format!("/dashboard/{dashboard_slug}").as_str()),
        "expected a link to the dashboard's own page: {event:#?}"
    );
}

/// `visualization.created` resolves to the visualization's name, linked to its query page.
#[tokio::test]
async fn events_resolve_visualization_to_own_name_and_owning_query_page() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "audit-viz-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let query = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id, "name": "audit-viz-query", "query": "select 1", "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = query["id"].as_i64().unwrap();

    let viz = ok_json(
        admin
            .post(format!("{}/api/visualizations", app.address))
            .json(&serde_json::json!({
                "query_id": query_id, "type": "table", "name": "audit-viz-chart", "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create visualization",
    )
    .await;
    let viz_id = viz["id"].as_i64().unwrap();

    let events = events_for_action(&admin, &app.address, "visualization.created").await;
    let event = events
        .iter()
        .find(|e| e["object_id"].as_i64() == Some(viz_id))
        .unwrap_or_else(|| panic!("no visualization.created event for {viz_id}: {events:#?}"));
    assert_eq!(event["object"]["name"].as_str(), Some("audit-viz-chart"));
    assert_eq!(
        event["object"]["path"].as_str(),
        Some(format!("/queries/{query_id}").as_str()),
        "expected a link to the owning query's page: {event:#?}"
    );
}

/// `widget.created` resolves to the hosting dashboard's name and page.
#[tokio::test]
async fn events_resolve_widget_to_hosting_dashboard() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let dashboard = ok_json(
        admin
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "audit-widget-dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    let dashboard_id = dashboard["id"].as_i64().unwrap();
    let dashboard_slug = dashboard["slug"].as_str().unwrap().to_string();

    let widget = ok_json(
        admin
            .post(format!("{}/api/widgets", app.address))
            .json(&serde_json::json!({ "dashboard_id": dashboard_id, "text": "audit widget" }))
            .send()
            .await
            .unwrap(),
        "create widget",
    )
    .await;
    let widget_id = widget["id"].as_i64().unwrap();

    let events = events_for_action(&admin, &app.address, "widget.created").await;
    let event = events
        .iter()
        .find(|e| e["object_id"].as_i64() == Some(widget_id))
        .unwrap_or_else(|| panic!("no widget.created event for {widget_id}: {events:#?}"));
    assert_eq!(
        event["object"]["name"].as_str(),
        Some("audit-widget-dashboard")
    );
    assert_eq!(
        event["object"]["path"].as_str(),
        Some(format!("/dashboard/{dashboard_slug}").as_str()),
        "expected a link to the hosting dashboard: {event:#?}"
    );
}

/// `snippet.created` resolves to the snippet's `trigger` with no page link.
#[tokio::test]
async fn events_resolve_snippet_to_own_trigger_with_no_path() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let snippet = ok_json(
        admin
            .post(format!("{}/api/query_snippets", app.address))
            .json(&serde_json::json!({ "trigger": "audit-top10", "snippet": "limit 10" }))
            .send()
            .await
            .unwrap(),
        "create snippet",
    )
    .await;
    let snippet_id = snippet["id"].as_i64().unwrap();

    let events = events_for_action(&admin, &app.address, "snippet.created").await;
    let event = events
        .iter()
        .find(|e| e["object_id"].as_i64() == Some(snippet_id))
        .unwrap_or_else(|| panic!("no snippet.created event for {snippet_id}: {events:#?}"));
    assert_eq!(event["object"]["name"].as_str(), Some("audit-top10"));
    assert_eq!(event["object"]["path"].as_str(), None);
}

/// A failed login records the attempted email and source IP, never a blank cell.
#[tokio::test]
async fn failed_login_records_attempted_email_and_ip() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap();
    let sess_resp = client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf_token = extract_set_cookie(sess_resp.headers(), "csrf_token").unwrap();

    let bad_login = client
        .post(format!("{}/api/login", app.address))
        .header("X-CSRF-Token", &csrf_token)
        .json(&serde_json::json!({ "email": ADMIN_EMAIL, "password": "definitely-wrong" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_login.status().as_u16(), 401);

    let admin = app.admin_client().await;
    let events = events_for_action(&admin, &app.address, "user.login_failed").await;
    let event = events
        .iter()
        .find(|e| e["additional_properties"]["attempted_email"].as_str() == Some(ADMIN_EMAIL))
        .unwrap_or_else(|| {
            panic!("no user.login_failed event recorded the attempted email: {events:#?}")
        });

    assert!(
        event["object_type"].is_null(),
        "a failed login has no object: {event:#?}"
    );
    let ip = event["additional_properties"]["ip"]
        .as_str()
        .unwrap_or_else(|| panic!("user.login_failed event carries no ip: {event:#?}"));
    assert!(!ip.is_empty(), "ip must not be blank: {event:#?}");
}

/// An unauthenticated caller controls `attempted_email`, so a 320-char cap keeps login attempts
/// from writing unbounded content into `events`.
#[tokio::test]
async fn failed_login_truncates_an_oversized_attempted_email() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap();
    let sess_resp = client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf_token = extract_set_cookie(sess_resp.headers(), "csrf_token").unwrap();

    let huge_email = format!("{}@example.com", "a".repeat(5000));
    let bad_login = client
        .post(format!("{}/api/login", app.address))
        .header("X-CSRF-Token", &csrf_token)
        .json(&serde_json::json!({ "email": huge_email, "password": "definitely-wrong" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_login.status().as_u16(), 401);

    let admin = app.admin_client().await;
    let events = events_for_action(&admin, &app.address, "user.login_failed").await;
    let event = events
        .iter()
        .find(|e| {
            e["additional_properties"]["attempted_email"]
                .as_str()
                .is_some_and(|s| s.starts_with("aaa"))
        })
        .unwrap_or_else(|| {
            panic!("no user.login_failed event for the oversized email: {events:#?}")
        });
    let stored = event["additional_properties"]["attempted_email"]
        .as_str()
        .unwrap();
    assert_eq!(
        stored.chars().count(),
        320,
        "attempted_email must be truncated to exactly 320 chars, got {} chars",
        stored.chars().count()
    );
}

/// A SAML ACS call with an unknown `RelayState` (never issued, expired, or consumed) leaves an
/// audit row with its own `reason`.
#[tokio::test]
async fn saml_acs_unknown_relay_state_records_login_failed() {
    let app = skip_if_none!(
        spawn_app_with(|c| c.saml_idp_metadata_xml = Some(SAML_IDP_METADATA_XML.to_string())).await
    );
    app.cleanup().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/saml/acs", app.address))
        .form(&[
            ("SAMLResponse", "irrelevant-never-parsed"),
            ("RelayState", "never-issued-relay-state"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    let admin = app.admin_client().await;
    let events = events_for_action(&admin, &app.address, "user.login_failed").await;
    let event = events
        .iter()
        .find(|e| e["additional_properties"]["reason"].as_str() == Some("replay_or_unknown_state"))
        .unwrap_or_else(|| {
            panic!("no user.login_failed event for unknown SAML relay state: {events:#?}")
        });
    assert_eq!(event["additional_properties"]["via"].as_str(), Some("saml"));
}

/// A SAML ACS call that clears the RelayState check but carries an unparseable `SAMLResponse`
/// leaves an audit row with a distinct `reason`.
#[tokio::test]
async fn saml_acs_invalid_response_records_login_failed() {
    let app = skip_if_none!(
        spawn_app_with(|c| c.saml_idp_metadata_xml = Some(SAML_IDP_METADATA_XML.to_string())).await
    );
    app.cleanup().await;

    // Stash a one-time state key under the Redis key format `acs` looks up so the call reaches
    // response parsing.
    let request_id = "test-relay-state-invalid-response";
    let mut redis = app.redis.clone();
    let _: () = redis::AsyncCommands::set_ex(
        &mut redis,
        format!("arcodash:saml:req:{request_id}"),
        "1",
        300,
    )
    .await
    .unwrap();

    let bad_response = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        "not a SAML response",
    );
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/saml/acs", app.address))
        .form(&[
            ("SAMLResponse", bad_response.as_str()),
            ("RelayState", request_id),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    let admin = app.admin_client().await;
    let events = events_for_action(&admin, &app.address, "user.login_failed").await;
    let event = events
        .iter()
        .find(|e| e["additional_properties"]["reason"].as_str() == Some("invalid_response"))
        .unwrap_or_else(|| {
            panic!("no user.login_failed event for invalid SAML response: {events:#?}")
        });
    assert_eq!(event["additional_properties"]["via"].as_str(), Some("saml"));
}

/// `GET /api/events` reports the total via `X-Total-Count` (body stays a plain array) and honors
/// `from`/`to` filters.
#[tokio::test]
async fn events_total_count_header_and_date_filters() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    // The admin's own login (from `admin_client()`) already left at least one `events` row.
    let resp = admin
        .get(format!("{}/api/events", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let total: u64 = resp
        .headers()
        .get("x-total-count")
        .expect("X-Total-Count header must be present")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        total >= 1,
        "expected at least the login event to be counted"
    );

    // A date range entirely in the future must exclude every existing event.
    let future_resp = admin
        .get(format!("{}/api/events?from=2999-01-01", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(future_resp.status().as_u16(), 200);
    let future_total: u64 = future_resp
        .headers()
        .get("x-total-count")
        .expect("X-Total-Count header must be present")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        future_total, 0,
        "a future `from` date must exclude every event"
    );

    let bad_date_resp = admin
        .get(format!("{}/api/events?from=not-a-date", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_date_resp.status().as_u16(), 400);
}

/// `query.created` / `query.updated` each leave exactly one row, and `query.updated` never carries
/// the SQL text (the audit table is org-readable).
#[tokio::test]
async fn query_created_and_updated_are_audited_without_leaking_sql() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let admin_id = session["user"]["id"].as_i64().expect("admin id");

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "audit-query-lifecycle-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let secret_sql = "SELECT ssn FROM sensitive_customer_table";
    let query = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "audit-query-lifecycle",
                "query": secret_sql,
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = query["id"].as_i64().unwrap();
    assert_event(
        &admin,
        &app.address,
        "query.created",
        "query",
        query_id,
        Some(admin_id),
    )
    .await;
    let created_events = events_for_action(&admin, &app.address, "query.created").await;
    let created_event = created_events
        .iter()
        .find(|e| e["object_id"].as_i64() == Some(query_id))
        .unwrap_or_else(|| {
            panic!("no query.created event for query {query_id}: {created_events:#?}")
        });
    assert_eq!(
        created_event["additional_properties"]["data_source_id"].as_i64(),
        Some(ds_id),
        "query.created must carry data_source_id so a reader can tell which source a query targets: {created_event:#?}"
    );

    // Name-only edit: `changed` must name the field that moved and nothing else.
    let update_resp = admin
        .post(format!("{}/api/queries/{query_id}", app.address))
        .json(&serde_json::json!({ "name": "audit-query-lifecycle-renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(update_resp.status().as_u16(), 200);

    let updated_events = events_for_action(&admin, &app.address, "query.updated").await;
    let updated_event = updated_events
        .iter()
        .find(|e| e["object_id"].as_i64() == Some(query_id))
        .unwrap_or_else(|| {
            panic!("no query.updated event for query {query_id}: {updated_events:#?}")
        });
    assert_eq!(updated_event["user_id"].as_i64(), Some(admin_id));
    let props = updated_event["additional_properties"].to_string();
    assert!(
        !props.contains(secret_sql) && !props.contains("sensitive_customer_table"),
        "query.updated properties must never carry the query's SQL text: {props}"
    );
    let name_only_changed: Vec<&str> = updated_event["additional_properties"]["changed"]
        .as_array()
        .unwrap_or_else(|| panic!("query.updated missing changed array: {updated_event:#?}"))
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        name_only_changed.contains(&"name") && !name_only_changed.contains(&"query"),
        "a name-only edit's changed list must name \"name\" and not \"query\": {name_only_changed:?}"
    );

    // SQL-only edit: `changed` must say so, without ever carrying the SQL text itself.
    let sql_update_resp = admin
        .post(format!("{}/api/queries/{query_id}", app.address))
        .json(&serde_json::json!({ "query": "SELECT 2" }))
        .send()
        .await
        .unwrap();
    assert_eq!(sql_update_resp.status().as_u16(), 200);

    let updated_events = events_for_action(&admin, &app.address, "query.updated").await;
    let sql_updated_event = updated_events
        .iter()
        .find(|e| e["object_id"].as_i64() == Some(query_id))
        .unwrap_or_else(|| {
            panic!("no query.updated event for query {query_id}: {updated_events:#?}")
        });
    let sql_only_changed: Vec<&str> = sql_updated_event["additional_properties"]["changed"]
        .as_array()
        .unwrap_or_else(|| panic!("query.updated missing changed array: {sql_updated_event:#?}"))
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        sql_only_changed.contains(&"query") && !sql_only_changed.contains(&"name"),
        "a SQL-only edit's changed list must name \"query\" and not \"name\": {sql_only_changed:?}"
    );

    let publish_resp = admin
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 2 }))
        .send()
        .await
        .unwrap();
    assert_eq!(publish_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "query.published",
        "query",
        query_id,
        Some(admin_id),
    )
    .await;
}

/// `schedule.created` / `.updated` / `.deleted` each leave exactly one row as a schedule is
/// attached to and detached from a query.
#[tokio::test]
async fn schedule_mutations_are_audited() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let admin_id = session["user"]["id"].as_i64().expect("admin id");

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "audit-schedule-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let query = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "audit-schedule-query",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = query["id"].as_i64().unwrap();
    admin
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();

    let create_resp = admin
        .post(format!("{}/api/schedules", app.address))
        .json(&serde_json::json!({
            "entity_type": "query",
            "entity_id": query_id,
            "spec": { "kind": "interval", "every_secs": 900 },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(create_resp.status().as_u16(), 201, "create schedule");
    let schedule: serde_json::Value = create_resp.json().await.unwrap();
    let schedule_id = schedule["id"].as_i64().unwrap();
    assert_event(
        &admin,
        &app.address,
        "schedule.created",
        "schedule",
        schedule_id,
        Some(admin_id),
    )
    .await;

    let update_resp = admin
        .patch(format!("{}/api/schedules/{schedule_id}", app.address))
        .json(&serde_json::json!({ "spec": { "kind": "interval", "every_secs": 1800 } }))
        .send()
        .await
        .unwrap();
    assert_eq!(update_resp.status().as_u16(), 200);
    assert_event(
        &admin,
        &app.address,
        "schedule.updated",
        "schedule",
        schedule_id,
        Some(admin_id),
    )
    .await;

    let delete_resp = admin
        .delete(format!("{}/api/schedules/{schedule_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(delete_resp.status().as_u16(), 204);
    assert_event(
        &admin,
        &app.address,
        "schedule.deleted",
        "schedule",
        schedule_id,
        Some(admin_id),
    )
    .await;
}

/// `organization.updated` records one row per request naming every touched top-level field (names
/// only, no values).
#[tokio::test]
async fn organization_update_is_audited_with_changed_keys() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let admin_id = session["user"]["id"].as_i64().expect("admin id");
    let org_id = session["org"]["id"].as_i64().expect("org id");
    let current_name = session["org"]["name"]
        .as_str()
        .expect("org name")
        .to_string();
    // `cleanup()` keeps `organizations`, so a fixed rename target could be a no-op; always send a
    // different name.
    let new_name = format!("{current_name}-renamed");

    let update_resp = admin
        .post(format!("{}/api/organization", app.address))
        .json(&serde_json::json!({
            "name": new_name,
            "settings": { "some_custom_key": "value" },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(update_resp.status().as_u16(), 200);

    let events = events_for_action(&admin, &app.address, "organization.updated").await;
    let event = events
        .iter()
        .find(|e| e["object_id"].as_i64() == Some(org_id))
        .unwrap_or_else(|| panic!("no organization.updated event: {events:#?}"));
    assert_eq!(event["user_id"].as_i64(), Some(admin_id));
    let keys = event["additional_properties"]["keys"]
        .as_array()
        .expect("keys must be an array")
        .iter()
        .map(|v| v.as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert!(keys.contains(&"name"), "keys must include name: {keys:?}");
    assert!(
        keys.contains(&"some_custom_key"),
        "keys must include the caller-supplied settings key: {keys:?}"
    );
}

/// A legacy raw action (`"login_failed"`) is found when filtering by its dotted name; the
/// translation lives in the SQL filter.
#[tokio::test]
async fn events_action_filter_finds_a_legacy_named_row_by_its_settled_name() {
    use arcodash_core::entities::event;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let org_id = session["org"]["id"].as_i64().expect("org id");

    // Directly seeds a row with a legacy raw action string; never produced by current code.
    event::ActiveModel {
        org_id: Set(org_id),
        user_id: Set(None),
        action: Set("login_failed".to_string()),
        object_type: Set(None),
        object_id: Set(None),
        additional_properties: Set(serde_json::json!({ "attempted_email": "legacy@test.local" })),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .expect("seed legacy event row");

    let events = events_for_action(&admin, &app.address, "user.login_failed").await;
    let found = events.iter().any(|e| {
        e["additional_properties"]["attempted_email"].as_str() == Some("legacy@test.local")
    });
    assert!(
        found,
        "filtering by the settled 'user.login_failed' name must also return a row still holding \
         the legacy 'login_failed' string: {events:#?}"
    );
}

/// Every legacy bare-snake action renamed alongside `embed_created` → `embed.created` has a
/// `LEGACY_ACTION_MAP` entry, so old rows resolve under the new name.
#[tokio::test]
async fn legacy_action_names_all_resolve_to_their_dotted_form() {
    use arcodash_core::entities::event;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let session = ok_json(
        admin
            .get(format!("{}/api/session", app.address))
            .send()
            .await
            .unwrap(),
        "admin session",
    )
    .await;
    let org_id = session["org"]["id"].as_i64().expect("org id");

    for (legacy, dotted) in [
        ("embed_revoked", "embed.revoked"),
        ("embed_public_paused", "embed.public_paused"),
        ("embed_public_resumed", "embed.public_resumed"),
        ("monitor_escalated", "monitor.escalated"),
        ("execute_query_failed", "query.execution_failed"),
        ("cancel_query_execution", "query.execution_cancelled"),
        (
            "notification_delivery_failed",
            "notification.delivery_failed",
        ),
    ] {
        let marker = format!("marker-{legacy}");
        event::ActiveModel {
            org_id: Set(org_id),
            user_id: Set(None),
            action: Set(legacy.to_string()),
            object_type: Set(None),
            object_id: Set(None),
            additional_properties: Set(serde_json::json!({ "marker": marker })),
            ..Default::default()
        }
        .insert(&app.db)
        .await
        .unwrap_or_else(|e| panic!("seed legacy {legacy} event row: {e}"));

        let events = events_for_action(&admin, &app.address, dotted).await;
        let found = events
            .iter()
            .any(|e| e["additional_properties"]["marker"].as_str() == Some(marker.as_str()));
        assert!(
            found,
            "filtering by '{dotted}' must return a row still holding the legacy '{legacy}' \
             string: {events:#?}"
        );
        let displayed = events
            .iter()
            .find(|e| e["additional_properties"]["marker"].as_str() == Some(marker.as_str()))
            .unwrap();
        assert_eq!(
            displayed["action"], dotted,
            "the response must show the settled dotted name, not the legacy one"
        );
    }
}
