//! Integration tests for Embedding & Snippets in the OSS (single-org,
//! `PublicDashboardAccess::Anonymous`) harness. Cross-org behavior and signed embeds are not
//! covered here.

mod common;
use common::ok_json;
use common::{authed_client, spawn_app, spawn_app_with, TestApp};

use sea_orm::{
    ActiveModelTrait, ActiveValue::*, ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait,
    QueryFilter,
};

async fn create_ds(client: &reqwest::Client, base: &str) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/data_sources"))
            .json(&serde_json::json!({ "name": "embed-test-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

/// Create a query and publish it (a visualization embed can only target a published query).
async fn create_query(client: &reqwest::Client, base: &str, ds_id: i64) -> i64 {
    let query_id = ok_json(
        client
            .post(format!("{base}/api/queries"))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Embed Test Query -- SELECT customer_secrets FROM accounts",
                "query": "SELECT customer_secrets_marker_xyz FROM accounts",
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

async fn create_dashboard(client: &reqwest::Client, base: &str) -> (i64, String) {
    let body = ok_json(
        client
            .post(format!("{base}/api/dashboards"))
            .json(&serde_json::json!({ "name": "Embed Test Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    (
        body["id"].as_i64().unwrap(),
        body["slug"].as_str().unwrap().to_string(),
    )
}

async fn create_widget(
    client: &reqwest::Client,
    base: &str,
    dashboard_id: i64,
    viz_id: i64,
) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/widgets"))
            .json(&serde_json::json!({ "dashboard_id": dashboard_id, "visualization_id": viz_id }))
            .send()
            .await
            .unwrap(),
        "create widget",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

/// A dashboard is a viewer surface once published; a never-published one can't get a share link or
/// embed.
async fn publish_dashboard(client: &reqwest::Client, base: &str, slug: &str) {
    let resp = client
        .post(format!("{base}/api/dashboards/{slug}/publish"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "publish must succeed: {}",
        resp.text().await.unwrap_or_default()
    );
}

async fn create_visualization(client: &reqwest::Client, base: &str, query_id: i64) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/visualizations"))
            .json(&serde_json::json!({
                "query_id": query_id, "type": "chart", "name": "Embed Test Viz", "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create visualization",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

/// Insert a cached `query_results` row directly and point `queries.latest_query_data_id` at it, so
/// the public serializer has real data to strip.
async fn seed_query_result(app: &TestApp, org_id: i64, ds_id: i64, query_id: i64) -> i64 {
    use arcodash_core::entities::query_result;
    let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
    let qr = query_result::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        data_source_id: Set(ds_id),
        query_hash: Set("test-hash".to_string()),
        query: Set("SELECT customer_secrets_marker_xyz FROM accounts".to_string()),
        data: Set(Some(serde_json::json!({
            "columns": [{"name": "n", "friendly_type": "integer"}],
            "rows": [{"n": 1}]
        }))),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(Some(0.01)),
        retrieved_at: Set(now),
        last_used_at: Set(now),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(&app.db)
    .await
    .expect("query_result insert failed");

    // Also repoints `published_latest_query_data_id`: the serve path reads the published pointer,
    // and leaving it unset would serve `data: null`.
    app.db
        .execute_unprepared(&format!(
            "UPDATE queries SET latest_query_data_id = {0}, published_latest_query_data_id = {0} WHERE id = {1}",
            qr.id, query_id
        ))
        .await
        .expect("failed to point query at its latest result");
    qr.id
}

/// Create a monitor via the API over `query_id`; returns its id. Visibility stays at the
/// `workspace` default; restrict with [`set_monitor_visibility`].
async fn create_monitor(client: &reqwest::Client, base: &str, query_id: i64) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/monitors"))
            .json(&serde_json::json!({
                "query_id": query_id,
                "name": "Embed Test Monitor",
                "options": { "op": ">", "value": 100, "column": "n" },
            }))
            .send()
            .await
            .unwrap(),
        "create monitor",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

/// Create a widget carrying only a `monitors` tile list (no visualization).
async fn create_monitor_widget(
    client: &reqwest::Client,
    base: &str,
    dashboard_id: i64,
    monitor_ids: &[i64],
) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/widgets"))
            .json(&serde_json::json!({
                "dashboard_id": dashboard_id,
                "options": { "monitors": monitor_ids }
            }))
            .send()
            .await
            .unwrap(),
        "create monitor widget",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

/// No HTTP path sets a monitor's visibility (unlike queries/dashboards) — write it directly.
async fn set_monitor_visibility(app: &TestApp, monitor_id: i64, visibility: &str) {
    app.db
        .execute_unprepared(&format!(
            "UPDATE monitors SET visibility = '{visibility}' WHERE id = {monitor_id}"
        ))
        .await
        .expect("failed to set monitor visibility");
}

/// Terminal `runs` row for a monitor, read by `monitor_service::tile_data` for the tile's
/// `observed` cell.
async fn insert_monitor_run(app: &TestApp, org_id: i64, monitor_id: i64, query_result_id: i64) {
    use arcodash_core::entities::run;
    let now = chrono::Utc::now();
    run::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        entity_type: Set("monitor".to_string()),
        entity_id: Set(monitor_id),
        schedule_id: Set(None),
        data_source_id: Set(None),
        scheduled_for: Set(None),
        triggered_by: Set("manual".to_string()),
        status: Set("success".to_string()),
        waiting_on_run_id: Set(None),
        job_id: Set(None),
        worker_id: Set(None),
        lease_expires_at: Set(None),
        started_at: Set(Some(now.into())),
        finished_at: Set(Some(now.into())),
        duration_ms: Set(Some(1)),
        row_count: Set(None),
        error: Set(None),
        data_age_secs: Set(None),
        query_result_id: Set(Some(query_result_id)),
        condition_met: Set(None),
        created_at: Set(now.into()),
        enqueued_at: NotSet,
    }
    .insert(&app.db)
    .await
    .expect("run insert failed");
}

/// Insert a second org with its own published query and monitor, to assert a monitor id from
/// outside the embed's org never resolves.
async fn seed_cross_org_public_monitor(app: &TestApp) -> i64 {
    use arcodash_core::entities::{monitor, organization};
    use arcodash_core::test_support;

    let other_org = organization::ActiveModel {
        id: NotSet,
        name: Set("Cross-Org Embed Test".to_string()),
        slug: Set(format!("embed-cross-org-{}", uuid::Uuid::new_v4().simple())),
        settings: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    }
    .insert(&app.db)
    .await
    .unwrap();

    let owner = test_support::new_user(other_org.id)
        .insert(&app.db)
        .await
        .unwrap();
    let ds = test_support::new_data_source(other_org.id)
        .insert(&app.db)
        .await
        .unwrap();
    // Published state is irrelevant: the monitor is dropped on the org check alone.
    let q = test_support::new_query(other_org.id, ds.id, owner.id)
        .insert(&app.db)
        .await
        .unwrap();

    monitor::ActiveModel {
        id: NotSet,
        org_id: Set(other_org.id),
        user_id: Set(owner.id),
        query_id: Set(q.id),
        name: Set("Cross-Org Monitor".to_string()),
        options: Set(serde_json::json!({ "op": ">", "value": 1 })),
        state: Set("unknown".to_string()),
        visibility: Set("workspace".to_string()),
        last_triggered_at: Set(None),
        rearm: Set(None),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap()
    .id
}

/// Full fixture: dashboard with one widget/visualization/query, a cached result, and a public
/// embed for the dashboard. Returns `(dashboard_id, token)`.
async fn create_dashboard_embed_fixture(app: &TestApp, client: &reqwest::Client) -> (i64, String) {
    let ds_id = create_ds(client, &app.address).await;
    let query_id = create_query(client, &app.address, ds_id).await;
    let viz_id = create_visualization(client, &app.address, query_id).await;
    let (dashboard_id, slug) = create_dashboard(client, &app.address).await;
    create_widget(client, &app.address, dashboard_id, viz_id).await;
    publish_dashboard(client, &app.address, &slug).await;

    // Single-org OSS harness: look up the seeded default org id rather than hardcode it.
    use arcodash_core::entities::organization;
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();

    seed_query_result(app, org.id, ds_id, query_id).await;

    let embed: serde_json::Value = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "dashboard", "object_id": dashboard_id, "anonymous": true
            }))
            .send()
            .await
            .unwrap(),
        "create embed",
    )
    .await;
    let token = embed["token"].as_str().unwrap().to_string();
    (dashboard_id, token)
}

#[tokio::test]
async fn public_embed_ignores_query_string_params() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (_dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    let bare = reqwest::Client::new();
    let plain = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status().as_u16(), 200);
    let plain_body = plain.text().await.unwrap();

    for tampered_qs in [
        "?customer_id=124",
        "?region=%2A",
        "?injected='%20OR%201%3D1--",
    ] {
        let tampered = bare
            .get(format!("{}/api/embed/{token}{tampered_qs}", app.address))
            .send()
            .await
            .unwrap();
        assert_eq!(tampered.status().as_u16(), 200);
        let tampered_body = tampered.text().await.unwrap();
        assert_eq!(
            plain_body, tampered_body,
            "query string {tampered_qs} must not change the served snapshot"
        );
    }
}

#[tokio::test]
async fn public_embed_unknown_and_malformed_tokens_both_404_uniformly() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let bare = reqwest::Client::new();

    let never_existed = bare
        .get(format!(
            "{}/api/embed/{}",
            app.address,
            uuid::Uuid::new_v4()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(never_existed.status().as_u16(), 404);

    let malformed = bare
        .get(format!("{}/api/embed/not-a-real-token-at-all", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status().as_u16(), 404);

    // Same shape for malformed and well-formed-but-absent: no oracle.
    assert_eq!(
        never_existed.text().await.unwrap(),
        malformed.text().await.unwrap()
    );
}

#[tokio::test]
async fn revoked_embed_returns_404_with_no_store_and_stays_dead() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (_dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    let embeds: serde_json::Value = ok_json(
        client
            .get(format!(
                "{}/api/embeds?object_type=dashboard&object_id={_dash_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list embeds",
    )
    .await;
    let embed_id = embeds[0]["id"].as_i64().unwrap();

    let revoke_resp = client
        .delete(format!("{}/api/embeds/{embed_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(revoke_resp.status().as_u16(), 204);

    let events: serde_json::Value = ok_json(
        client
            .get(format!("{}/api/events?action=embed.revoked", app.address))
            .send()
            .await
            .unwrap(),
        "list embed.revoked events",
    )
    .await;
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["object_id"].as_i64() == Some(embed_id)),
        "revoking an embed must record its action under the dotted 'embed.revoked' name: {events:#?}"
    );

    let bare = reqwest::Client::new();
    let after = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(after.status().as_u16(), 404);
    assert_eq!(
        after
            .headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap(),
        "no-store",
        "a revoked (error-path) response must carry no-store too, not just the success path"
    );

    let revoke_again = client
        .delete(format!("{}/api/embeds/{embed_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(revoke_again.status().as_u16(), 204);
}

#[tokio::test]
async fn expired_embed_returns_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (_dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    use arcodash_core::entities::embed;
    let row = embed::Entity::find()
        .filter(embed::Column::Token.eq(token.clone()))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: embed::ActiveModel = row.into();
    active.expires_at = Set(Some(
        (chrono::Utc::now() - chrono::Duration::days(1)).into(),
    ));
    active.update(&app.db).await.unwrap();

    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn visualization_token_replayed_on_dashboard_only_route_is_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let viz_id = create_visualization(&client, &app.address, query_id).await;

    let embed: serde_json::Value = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "visualization", "object_id": viz_id, "anonymous": true
            }))
            .send()
            .await
            .unwrap(),
        "create viz embed",
    )
    .await;
    let viz_token = embed["token"].as_str().unwrap().to_string();

    let bare = reqwest::Client::new();
    let via_unified = bare
        .get(format!("{}/api/embed/{viz_token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(via_unified.status().as_u16(), 200);

    // The deprecated dashboard-only route rejects it with 404 (object-type confusion).
    let via_dashboard_route = bare
        .get(format!("{}/api/dashboards/public/{viz_token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(via_dashboard_route.status().as_u16(), 404);
}

#[tokio::test]
async fn env_kill_switch_overrides_org_setting_being_on() {
    let app = skip_if_none!(spawn_app_with(|c| c.disable_public_urls = true).await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (_dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "env kill-switch must win");
}

#[tokio::test]
async fn org_pause_persistence_round_trip() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let mut tokens = Vec::new();
    let mut rows_before = Vec::new();
    for _ in 0..3 {
        let (_dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;
        tokens.push(token);
    }

    use arcodash_core::entities::embed;
    for t in &tokens {
        let row = embed::Entity::find()
            .filter(embed::Column::Token.eq(t.clone()))
            .one(&app.db)
            .await
            .unwrap()
            .unwrap();
        rows_before.push(row);
    }

    let bare = reqwest::Client::new();
    for t in &tokens {
        let resp = bare
            .get(format!("{}/api/embed/{t}", app.address))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "must serve before pause");
    }

    let off = client
        .post(format!("{}/api/orgs/me/embed_settings", app.address))
        .json(&serde_json::json!({ "public_urls_enabled": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(off.status().as_u16(), 200);

    for t in &tokens {
        let resp = bare
            .get(format!("{}/api/embed/{t}", app.address))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404, "must 404 while org-paused");
    }

    let on = client
        .post(format!("{}/api/orgs/me/embed_settings", app.address))
        .json(&serde_json::json!({ "public_urls_enabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(on.status().as_u16(), 200);

    for (t, before) in tokens.iter().zip(rows_before.iter()) {
        let resp = bare
            .get(format!("{}/api/embed/{t}", app.address))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "must serve again after resume");

        let after = embed::Entity::find()
            .filter(embed::Column::Token.eq(t.clone()))
            .one(&app.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.id, before.id, "row id must not change");
        assert_eq!(after.token, before.token, "token must not change");
        assert_eq!(
            after.created_at, before.created_at,
            "created_at must not change"
        );
        assert_eq!(
            after.revoked_at, before.revoked_at,
            "revoked_at must not change"
        );
    }
}

#[tokio::test]
async fn org_pause_does_not_resurrect_a_link_revoked_before_the_pause() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    let embeds: serde_json::Value = ok_json(
        client
            .get(format!(
                "{}/api/embeds?object_type=dashboard&object_id={dash_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list embeds",
    )
    .await;
    let embed_id = embeds[0]["id"].as_i64().unwrap();
    let revoke = client
        .delete(format!("{}/api/embeds/{embed_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status().as_u16(), 204);

    for enabled in [false, true] {
        let toggle = client
            .post(format!("{}/api/orgs/me/embed_settings", app.address))
            .json(&serde_json::json!({ "public_urls_enabled": enabled }))
            .send()
            .await
            .unwrap();
        assert_eq!(toggle.status().as_u16(), 200);

        let resp = reqwest::Client::new()
            .get(format!("{}/api/embed/{token}", app.address))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            404,
            "revoked link must stay dead regardless of org pause state"
        );
    }
}

/// Fetch the org's builtin "default" group id (has `create_embed`, not `share_public_anonymous`).
async fn default_group_id(admin: &reqwest::Client, base: &str) -> i64 {
    let groups: serde_json::Value = ok_json(
        admin
            .get(format!("{base}/api/groups"))
            .send()
            .await
            .unwrap(),
        "list groups",
    )
    .await;
    groups
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["name"] == "default")
        .expect("default group must exist")["id"]
        .as_i64()
        .unwrap()
}

async fn invite_and_onboard(
    app: &TestApp,
    admin: &reqwest::Client,
    email: &str,
    groups: Vec<i64>,
) -> reqwest::Client {
    let invite: serde_json::Value = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(
                &serde_json::json!({ "name": "Embed Test User", "email": email, "groups": groups }),
            )
            .send()
            .await
            .unwrap(),
        "invite user",
    )
    .await;
    let user_id = invite["id"].as_i64().unwrap();

    use arcodash_core::entities::user;
    let u = user::Entity::find_by_id(user_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let token = u.details["invitation_token"].as_str().unwrap().to_string();

    let bare = reqwest::Client::builder()
        .cookie_store(false)
        .build()
        .unwrap();
    let password = "T3stUserPassw0rd!";
    let accept = bare
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(
            &serde_json::json!({ "token": token, "name": "Embed Test User", "password": password }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(accept.status().as_u16(), 200, "accept invitation");

    let (session, csrf) = app.login(email, password).await;
    authed_client(&app.address, &session, &csrf)
}

/// `GET /api/orgs/me/embed_settings` requires a session: without one, any caller with a resolvable
/// org could read it.
#[tokio::test]
async fn embed_settings_get_requires_a_session() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let resp = reqwest::Client::new()
        .get(format!("{}/api/orgs/me/embed_settings", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        401,
        "reading embed settings with no session must be unauthenticated"
    );
}

/// The payload is one boolean the non-admin embed dialog needs, so org membership alone must
/// suffice (not `admin`).
#[tokio::test]
async fn embed_settings_get_succeeds_for_a_non_admin_member() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let default_gid = default_group_id(&admin, &app.address).await;
    let member = invite_and_onboard(&app, &admin, "member@test.local", vec![default_gid]).await;

    let resp = member
        .get(format!("{}/api/orgs/me/embed_settings", app.address))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(
        status, 200,
        "a non-admin member must still be able to read this: {body}"
    );
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["public_urls_enabled"], true);
}

#[tokio::test]
async fn user_without_create_embed_permission_cannot_create_any_embed() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id).await;
    let viz_id = create_visualization(&admin, &app.address, query_id).await;

    let plain = invite_and_onboard(&app, &admin, "no-perms@test.local", vec![]).await;

    let resp = plain
        .post(format!("{}/api/embeds", app.address))
        .json(&serde_json::json!({
            "object_type": "visualization", "object_id": viz_id, "anonymous": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn create_embed_without_share_public_anonymous_refuses_anonymous_but_allows_org_scoped() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;

    let default_gid = default_group_id(&admin, &app.address).await;
    let editor = invite_and_onboard(&app, &admin, "editor@test.local", vec![default_gid]).await;

    // Grant the editor's group data-source access, which `query_service::create` requires.
    let grant_ds = admin
        .post(format!(
            "{}/api/groups/{default_gid}/data_sources",
            app.address
        ))
        .json(&serde_json::json!({ "data_source_id": ds_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        grant_ds.status().as_u16(),
        200,
        "grant group data source access"
    );

    // Create the query/viz as the editor (owner-or-admin policy), so only the
    // create_embed/share_public_anonymous gate is under test.
    let query_id = create_query(&editor, &app.address, ds_id).await;
    let viz_id = create_visualization(&editor, &app.address, query_id).await;

    // has create_embed (via default group) but not share_public_anonymous.
    let anon_resp = editor
        .post(format!("{}/api/embeds", app.address))
        .json(&serde_json::json!({
            "object_type": "visualization", "object_id": viz_id, "anonymous": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        anon_resp.status().as_u16(),
        403,
        "anonymous create must be refused server-side without share_public_anonymous"
    );

    // Under `Anonymous` mode a non-anonymous link can never be served, so a permission-only pass
    // would mint a URL that 404s forever.
    let scoped_resp = editor
        .post(format!("{}/api/embeds", app.address))
        .json(&serde_json::json!({
            "object_type": "visualization", "object_id": viz_id, "anonymous": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        scoped_resp.status().as_u16(),
        400,
        "non-anonymous create must be refused in OSS's Anonymous access mode, not produce a dead link"
    );
}

/// Under OSS `Anonymous` mode a non-anonymous embed can never resolve, so `create_embed` must
/// refuse it up front and leave no row behind.
#[tokio::test]
async fn oss_rejects_a_non_anonymous_embed_create() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id).await;
    let viz_id = create_visualization(&admin, &app.address, query_id).await;

    let resp = admin
        .post(format!("{}/api/embeds", app.address))
        .json(&serde_json::json!({
            "object_type": "visualization", "object_id": viz_id, "anonymous": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "a non-anonymous embed create must be refused in OSS's Anonymous access mode"
    );

    use arcodash_core::entities::embed;
    let count = embed::Entity::find()
        .filter(embed::Column::ObjectType.eq("visualization"))
        .filter(embed::Column::ObjectId.eq(viz_id))
        .count(&app.db)
        .await
        .unwrap();
    assert_eq!(count, 0, "a rejected create must leave no embed row behind");
}

/// Positive control: an anonymous embed create succeeds and a real anonymous `GET` returns 200.
#[tokio::test]
async fn oss_still_accepts_an_anonymous_embed_create_and_serves_it() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id).await;
    let viz_id = create_visualization(&admin, &app.address, query_id).await;

    let embed: serde_json::Value = ok_json(
        admin
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "visualization", "object_id": viz_id, "anonymous": true
            }))
            .send()
            .await
            .unwrap(),
        "create anonymous embed",
    )
    .await;
    let token = embed["token"].as_str().unwrap().to_string();

    let bare = reqwest::Client::new();
    let get_resp = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        get_resp.status().as_u16(),
        200,
        "an anonymous embed's token must still resolve"
    );
}

#[tokio::test]
async fn anonymous_embed_stops_serving_once_creator_is_disabled() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (_dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    let bare = reqwest::Client::new();
    let before = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(before.status().as_u16(), 200);

    use arcodash_core::entities::user;
    let admin_user = user::Entity::find()
        .filter(user::Column::Email.eq(common::ADMIN_EMAIL))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: user::ActiveModel = admin_user.into();
    active.disabled_at = Set(Some(chrono::Utc::now().into()));
    active.update(&app.db).await.unwrap();

    let after = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        after.status().as_u16(),
        404,
        "gate #4 must stop serving once the creator is disabled"
    );
}

#[tokio::test]
async fn public_embed_response_has_no_sql_or_data_source_config() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (_dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();

    assert!(
        !body.contains("customer_secrets_marker_xyz"),
        "raw SQL text leaked into the public response: {body}"
    );
    for forbidden in [
        "\"query\":",
        "\"data_source",
        "\"options\":{\"host\"",
        "encrypted_options",
    ] {
        assert!(
            !body.contains(forbidden),
            "leaked field {forbidden} in {body}"
        );
    }
}

/// Drives the real `serialize_dashboard` against a fixture whose query SQL and widget options carry
/// distinctive markers; the type-level key-set test alone would miss a serializer copying a blob
/// into a free-form `Value` field.
#[tokio::test]
async fn public_embed_response_leaks_no_marker_from_query_or_widget_options() {
    use arcodash_core::entities::widget;

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;

    let query_id = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Marker Query",
                "query": "SELECT 'MARKER_SQL_c0ffee_DO_NOT_LEAK'",
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
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish query",
    )
    .await;

    let viz_id = create_visualization(&client, &app.address, query_id).await;
    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;
    let widget_id = create_widget(&client, &app.address, dashboard_id, viz_id).await;
    widget::Entity::update_many()
        .col_expr(
            widget::Column::Options,
            sea_orm::sea_query::Expr::value(
                serde_json::json!({ "marker": "MARKER_WIDGET_OPTIONS_c0ffee_DO_NOT_LEAK" }),
            ),
        )
        .filter(widget::Column::Id.eq(widget_id))
        .exec(&app.db)
        .await
        .unwrap();
    publish_dashboard(&client, &app.address, &slug).await;

    use arcodash_core::entities::organization;
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    seed_query_result(&app, org.id, ds_id, query_id).await;

    let embed = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "dashboard", "object_id": dashboard_id, "anonymous": true
            }))
            .send()
            .await
            .unwrap(),
        "create embed",
    )
    .await;
    let token = embed["token"].as_str().unwrap();

    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();

    for marker in [
        "MARKER_SQL_c0ffee_DO_NOT_LEAK",
        "MARKER_WIDGET_OPTIONS_c0ffee_DO_NOT_LEAK",
    ] {
        assert!(
            !body.contains(marker),
            "marker {marker} leaked into the public embed response: {body}"
        );
    }
    for forbidden in [
        "sql",
        "query_text",
        "data_source",
        "connection",
        "encrypted_options",
    ] {
        assert!(
            !body.contains(forbidden),
            "leaked forbidden field {forbidden}: {body}"
        );
    }
}

/// The deprecated `GET /api/dashboards/public/{token}` shim must leak nothing either (no widget
/// SQL, no full dashboard `options`).
#[tokio::test]
async fn deprecated_public_dashboard_route_has_no_sql_or_data_source_config() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (_dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/api/dashboards/public/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();

    assert!(
        !body.contains("customer_secrets_marker_xyz"),
        "raw SQL text leaked into the deprecated public dashboard response: {body}"
    );
    for forbidden in [
        "\"query\":",
        "\"data_source",
        "\"options\":{\"host\"",
        "encrypted_options",
    ] {
        assert!(
            !body.contains(forbidden),
            "leaked field {forbidden} in {body}"
        );
    }

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["object_type"], "dashboard");
    assert!(json["title"].is_string());
}

#[tokio::test]
async fn public_embed_response_carries_security_headers() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (_dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let headers = resp.headers();
    assert_eq!(
        headers.get("x-robots-tag").unwrap().to_str().unwrap(),
        "noindex, nofollow"
    );
    assert_eq!(
        headers.get("referrer-policy").unwrap().to_str().unwrap(),
        "no-referrer"
    );
    assert_eq!(
        headers.get("cache-control").unwrap().to_str().unwrap(),
        "no-store"
    );
    assert!(headers
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("frame-ancestors"));
}

#[tokio::test]
async fn archived_dashboard_stops_serving_its_embed() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    use arcodash_core::entities::dashboard;
    let d = dashboard::Entity::find_by_id(dash_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let slug = d.slug.clone();
    let archive_resp = client
        .post(format!("{}/api/dashboards/{slug}", app.address))
        .json(&serde_json::json!({ "is_archived": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(archive_resp.status().as_u16(), 200);

    let resp = reqwest::Client::new()
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

/// An unpublished dashboard's `published_snapshot` is `NULL`: the embed must fail closed with an
/// error status, not 200 with zero widgets.
#[tokio::test]
async fn unpublished_dashboard_stops_serving_its_embed_instead_of_going_blank() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    use arcodash_core::entities::dashboard;
    let d = dashboard::Entity::find_by_id(dash_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let slug = d.slug.clone();
    let unpublish_resp = client
        .post(format!("{}/api/dashboards/{slug}/unpublish", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unpublish_resp.status().as_u16(),
        200,
        "unpublish must succeed: {}",
        unpublish_resp.text().await.unwrap_or_default()
    );

    let resp = reqwest::Client::new()
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_ne!(
        status, 200,
        "an unpublished dashboard's embed must not 200 with an empty widget list: {body}"
    );

    // The token must survive the unpublish: republishing serves the same token again.
    let republish_resp = client
        .post(format!("{}/api/dashboards/{slug}/publish", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        republish_resp.status().as_u16(),
        200,
        "republish must succeed: {}",
        republish_resp.text().await.unwrap_or_default()
    );
    let resumed = reqwest::Client::new()
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resumed.status().as_u16(),
        200,
        "the same token must start serving again once published"
    );
}

#[tokio::test]
async fn active_count_reflects_embeds_on_the_query_and_its_dashboards() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let viz_id = create_visualization(&client, &app.address, query_id).await;
    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;
    create_widget(&client, &app.address, dashboard_id, viz_id).await;
    publish_dashboard(&client, &app.address, &slug).await;

    let zero: serde_json::Value = ok_json(
        client
            .get(format!(
                "{}/api/embeds/active_count?query_id={query_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "active_count before",
    )
    .await;
    assert_eq!(zero["count"], 0);

    let embed_resp = client
        .post(format!("{}/api/embeds", app.address))
        .json(&serde_json::json!({ "object_type": "dashboard", "object_id": dashboard_id, "anonymous": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(embed_resp.status().as_u16(), 200);

    let one: serde_json::Value = ok_json(
        client
            .get(format!(
                "{}/api/embeds/active_count?query_id={query_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "active_count after",
    )
    .await;
    assert_eq!(one["count"], 1);
}

/// `active_count` must authorize the specific query, not just org membership: a member without
/// reach on a restricted query must not learn its embed count.
#[tokio::test]
async fn active_count_requires_view_access_to_the_specific_query() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id).await;

    let restrict = admin
        .put(format!("{}/api/queries/{query_id}/visibility", app.address))
        .json(&serde_json::json!({ "visibility": "restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        restrict.status().as_u16(),
        204,
        "restrict visibility failed"
    );

    let group_id = default_group_id(&admin, &app.address).await;
    let plain = invite_and_onboard(&app, &admin, "no-reach@test.local", vec![group_id]).await;

    let resp = plain
        .get(format!(
            "{}/api/embeds/active_count?query_id={query_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_ne!(
        resp.status().as_u16(),
        200,
        "a caller with no view access to the query must not get a count back"
    );
}

#[tokio::test]
async fn legacy_share_endpoint_produces_a_link_the_new_guard_serves() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let viz_id = create_visualization(&client, &app.address, query_id).await;
    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;
    create_widget(&client, &app.address, dashboard_id, viz_id).await;
    publish_dashboard(&client, &app.address, &slug).await;

    let share: serde_json::Value = ok_json(
        client
            .post(format!("{}/api/dashboards/{slug}/share", app.address))
            .send()
            .await
            .unwrap(),
        "legacy share",
    )
    .await;
    let token = share["secret_token"].as_str().unwrap().to_string();

    let bare = reqwest::Client::new();
    let via_legacy = bare
        .get(format!("{}/api/dashboards/public/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        via_legacy.status().as_u16(),
        200,
        "legacy route must still serve"
    );

    let via_unified = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        via_unified.status().as_u16(),
        200,
        "the same token must also resolve through the new unified route"
    );

    let revoke = client
        .delete(format!("{}/api/dashboards/{slug}/share", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status().as_u16(), 204);

    let dead_legacy = bare
        .get(format!("{}/api/dashboards/public/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(dead_legacy.status().as_u16(), 404);

    let dead_unified = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(dead_unified.status().as_u16(), 404);
}

#[tokio::test]
async fn create_list_and_revoke_round_trip() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (dash_id, _token) = create_dashboard_embed_fixture(&app, &client).await;

    let list: serde_json::Value = ok_json(
        client
            .get(format!(
                "{}/api/embeds?object_type=dashboard&object_id={dash_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list embeds",
    )
    .await;
    let arr = list.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["object_type"], "dashboard");
    assert_eq!(arr[0]["object_id"], dash_id);
    assert!(arr[0]["public_url"]
        .as_str()
        .unwrap()
        .contains("/public/embed/"));
    assert!(arr[0]["revoked_at"].is_null());
}

/// `POST /api/embeds` must accept the response field name (`anon_authorized`) as an alias for
/// `anonymous`, so a round-tripped response can't silently produce a non-anonymous embed.
#[tokio::test]
async fn create_embed_accepts_anon_authorized_alias_for_anonymous() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let viz_id = create_visualization(&client, &app.address, query_id).await;
    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;
    create_widget(&client, &app.address, dashboard_id, viz_id).await;
    publish_dashboard(&client, &app.address, &slug).await;

    let via_alias: serde_json::Value = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "dashboard", "object_id": dashboard_id, "anon_authorized": true
            }))
            .send()
            .await
            .unwrap(),
        "create embed via anon_authorized alias",
    )
    .await;
    assert_eq!(via_alias["anon_authorized"], true);
    assert!(
        via_alias["token"].as_str().is_some(),
        "an anonymous embed must carry a public token"
    );

    let via_original: serde_json::Value = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "dashboard", "object_id": dashboard_id, "anonymous": true
            }))
            .send()
            .await
            .unwrap(),
        "create embed via anonymous",
    )
    .await;
    assert_eq!(via_original["anon_authorized"], true);
}

/// `GET /api/embeds?object_type=&object_id=` must apply the same owner-or-admin
/// `require_can_share_object` gate as `create_embed`, so a non-owner can't list another user's live
/// bearer tokens.
#[tokio::test]
async fn list_embeds_denies_a_non_owner_non_admin_caller() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id).await;
    let viz_id = create_visualization(&admin, &app.address, query_id).await;

    // `anonymous: true` because OSS `Anonymous` mode refuses non-anonymous creates, which is
    // orthogonal here.
    let create_resp = admin
        .post(format!("{}/api/embeds", app.address))
        .json(&serde_json::json!({
            "object_type": "visualization", "object_id": viz_id, "anonymous": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        create_resp.status().as_u16(),
        200,
        "admin creates the embed"
    );

    let default_gid = default_group_id(&admin, &app.address).await;
    let other =
        invite_and_onboard(&app, &admin, "not-the-owner@test.local", vec![default_gid]).await;

    let list_resp = other
        .get(format!(
            "{}/api/embeds?object_type=visualization&object_id={viz_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        list_resp.status().as_u16(),
        403,
        "a non-owner, non-admin caller must be denied listing another user's object's embeds"
    );

    let admin_list = admin
        .get(format!(
            "{}/api/embeds?object_type=visualization&object_id={viz_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(admin_list.status().as_u16(), 200);
}

/// `set_visibility` never touches `embeds`, so the guard must consult the object's `visibility`:
/// restricting a dashboard cuts off its anonymous link, and un-restricting restores it on the next
/// request.
#[tokio::test]
async fn restricted_dashboard_stops_serving_its_public_embed() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    let bare = reqwest::Client::new();
    let before = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        before.status().as_u16(),
        200,
        "must serve before restriction"
    );

    let restrict = client
        .put(format!(
            "{}/api/dashboards/{dash_id}/visibility",
            app.address
        ))
        .json(&serde_json::json!({ "visibility": "restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        restrict.status().as_u16(),
        204,
        "set_visibility to restricted failed"
    );

    let during = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        during.status().as_u16(),
        404,
        "a restricted dashboard's public embed must stop serving"
    );

    let unrestrict = client
        .put(format!(
            "{}/api/dashboards/{dash_id}/visibility",
            app.address
        ))
        .json(&serde_json::json!({ "visibility": "workspace" }))
        .send()
        .await
        .unwrap();
    assert_eq!(unrestrict.status().as_u16(), 204);

    let after = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        after.status().as_u16(),
        200,
        "un-restricting must resume serving the same token"
    );
}

/// Minting a link for a non-public-visibility dashboard must be refused (it would 404 forever), for
/// anonymous and org-scoped embeds; creation succeeds once it is `workspace`.
#[tokio::test]
async fn create_embed_rejects_link_for_restricted_dashboard_regardless_of_anonymous_flag() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let viz_id = create_visualization(&client, &app.address, query_id).await;
    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;
    create_widget(&client, &app.address, dashboard_id, viz_id).await;
    publish_dashboard(&client, &app.address, &slug).await;

    let restrict = client
        .put(format!(
            "{}/api/dashboards/{dashboard_id}/visibility",
            app.address
        ))
        .json(&serde_json::json!({ "visibility": "restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(restrict.status().as_u16(), 204);

    for anonymous in [true, false] {
        let resp = client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "dashboard", "object_id": dashboard_id, "anonymous": anonymous
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap();
        assert_eq!(
            status, 400,
            "creating a link (anonymous={anonymous}) for a restricted dashboard must be \
             rejected, not silently produce a dead link: {body}"
        );
        assert!(
            body.to_lowercase().contains("visibility"),
            "error must name the actual problem (visibility), not a generic message: {body}"
        );
    }

    let unrestrict = client
        .put(format!(
            "{}/api/dashboards/{dashboard_id}/visibility",
            app.address
        ))
        .json(&serde_json::json!({ "visibility": "workspace" }))
        .send()
        .await
        .unwrap();
    assert_eq!(unrestrict.status().as_u16(), 204);

    let ok = client
        .post(format!("{}/api/embeds", app.address))
        .json(&serde_json::json!({
            "object_type": "dashboard", "object_id": dashboard_id, "anonymous": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        ok.status().as_u16(),
        200,
        "public visibility must allow creating the link: {}",
        ok.text().await.unwrap_or_default()
    );
}

/// Same gate for visualizations, which inherit the query's visibility: `require_can_share_object`
/// must check the query.
#[tokio::test]
async fn create_embed_rejects_link_for_visualization_whose_query_is_restricted() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let viz_id = create_visualization(&client, &app.address, query_id).await;

    let restrict = client
        .put(format!("{}/api/queries/{query_id}/visibility", app.address))
        .json(&serde_json::json!({ "visibility": "restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(restrict.status().as_u16(), 204);

    let resp = client
        .post(format!("{}/api/embeds", app.address))
        .json(&serde_json::json!({
            "object_type": "visualization", "object_id": viz_id, "anonymous": true
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    assert_eq!(
        status, 400,
        "creating a link for a visualization on a restricted query must be rejected: {body}"
    );
    assert!(
        body.to_lowercase().contains("visibility"),
        "error must name the actual problem (visibility), not a generic message: {body}"
    );
}

#[tokio::test]
async fn unrecognized_visibility_value_stops_serving_its_public_embed() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    let bare = reqwest::Client::new();
    let before = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        before.status().as_u16(),
        200,
        "must serve before the visibility column carries a bogus value"
    );

    app.db
        .execute_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DbBackend::Postgres,
            "UPDATE dashboards SET visibility = $1 WHERE id = $2",
            ["some_bogus_tier".into(), dash_id.into()],
        ))
        .await
        .unwrap();

    let during = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        during.status().as_u16(),
        404,
        "an unrecognized visibility value must not fail open to public serving"
    );
}

#[tokio::test]
async fn embed_with_frame_ancestors_serves_the_exact_directive() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let viz_id = create_visualization(&client, &app.address, query_id).await;
    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;
    create_widget(&client, &app.address, dashboard_id, viz_id).await;
    publish_dashboard(&client, &app.address, &slug).await;
    use arcodash_core::entities::organization;
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    seed_query_result(&app, org.id, ds_id, query_id).await;

    let embed = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "dashboard",
                "object_id": dashboard_id,
                "anonymous": true,
                "frame_ancestors": ["https://a.example.com", "https://b.example.com"],
            }))
            .send()
            .await
            .unwrap(),
        "create embed with frame_ancestors",
    )
    .await;
    let token = embed["token"].as_str().unwrap();

    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let csp = resp
        .headers()
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(
        csp,
        "frame-ancestors https://a.example.com https://b.example.com"
    );
}

/// The deprecated `/api/dashboards/public/{token}` shim must honor the same `frame_ancestors`
/// allowlist as `/api/embed/{token}`.
#[tokio::test]
async fn legacy_public_dashboard_route_also_serves_the_frame_ancestors_directive() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let viz_id = create_visualization(&client, &app.address, query_id).await;
    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;
    create_widget(&client, &app.address, dashboard_id, viz_id).await;
    publish_dashboard(&client, &app.address, &slug).await;
    use arcodash_core::entities::organization;
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    seed_query_result(&app, org.id, ds_id, query_id).await;

    let embed = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "dashboard",
                "object_id": dashboard_id,
                "anonymous": true,
                "frame_ancestors": ["https://a.example.com"],
            }))
            .send()
            .await
            .unwrap(),
        "create embed with frame_ancestors",
    )
    .await;
    let token = embed["token"].as_str().unwrap();

    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/api/dashboards/public/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let csp = resp
        .headers()
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(
        csp, "frame-ancestors https://a.example.com",
        "the legacy route must not fall back to the wildcard default when the embed row has an allowlist"
    );
}

#[tokio::test]
async fn embed_without_frame_ancestors_serves_the_wildcard_default() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (_dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let csp = resp
        .headers()
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(csp, "frame-ancestors *");
}

#[tokio::test]
async fn embed_error_response_still_carries_the_default_csp_and_no_store() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/api/embed/does-not-exist", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    assert_eq!(
        resp.headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap(),
        "frame-ancestors *"
    );
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap(),
        "no-store"
    );
}

/// A past `expires_at` is rejected, and an omitted value with no org default stores no expiry.
#[tokio::test]
async fn create_embed_rejects_a_past_expires_at() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let viz_id = create_visualization(&client, &app.address, query_id).await;

    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let resp = client
        .post(format!("{}/api/embeds", app.address))
        .json(&serde_json::json!({
            "object_type": "visualization", "object_id": viz_id, "anonymous": true,
            "expires_at": past
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "a past expires_at must be rejected, not stored as a dead-on-arrival share"
    );

    let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let ok = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "visualization", "object_id": viz_id, "anonymous": true,
                "expires_at": future
            }))
            .send()
            .await
            .unwrap(),
        "create embed with a future expires_at",
    )
    .await;
    assert!(
        ok["expires_at"].is_string(),
        "a future expires_at (OSS: no org default to clamp against) must be stored verbatim"
    );
}

/// Batching the per-widget `query_result` fetch must not change the served output.
#[tokio::test]
async fn public_dashboard_render_is_byte_identical_across_widgets() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;

    use arcodash_core::entities::organization;
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();

    // Three widgets on separate queries, so a per-widget-fetch bug would show.
    let mut viz_ids = Vec::new();
    for i in 0..3 {
        let query_id = create_query(&client, &app.address, ds_id).await;
        let viz_id = create_visualization(&client, &app.address, query_id).await;
        seed_query_result(&app, org.id, ds_id, query_id).await;
        create_widget(&client, &app.address, dashboard_id, viz_id).await;
        viz_ids.push((query_id, viz_id, i));
    }
    publish_dashboard(&client, &app.address, &slug).await;

    let embed: serde_json::Value = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "dashboard", "object_id": dashboard_id, "anonymous": true
            }))
            .send()
            .await
            .unwrap(),
        "create embed",
    )
    .await;
    let token = embed["token"].as_str().unwrap().to_string();

    let bare = reqwest::Client::new();
    let first = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status().as_u16(), 200);
    let first_body = first.text().await.unwrap();

    let second = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    let second_body = second.text().await.unwrap();

    assert_eq!(
        first_body, second_body,
        "the batched render must be stable/deterministic across requests"
    );
    let parsed: serde_json::Value = serde_json::from_str(&first_body).unwrap();
    assert_eq!(
        parsed["widgets"].as_array().map(|a| a.len()),
        Some(3),
        "all three widgets must still be present after batching"
    );
    for w in parsed["widgets"].as_array().unwrap() {
        assert!(
            w["visualization"]["data"]["rows"].is_array(),
            "each widget must still carry its own result data: {w}"
        );
    }
}

/// Listing a dashboard's embeds must keep working after its visibility is restricted, so the owner
/// can still revoke the now-404 link.
#[tokio::test]
async fn listing_embeds_still_works_after_visibility_is_restricted() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let (dash_id, token) = create_dashboard_embed_fixture(&app, &client).await;

    let restrict = client
        .put(format!(
            "{}/api/dashboards/{dash_id}/visibility",
            app.address
        ))
        .json(&serde_json::json!({ "visibility": "restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        restrict.status().as_u16(),
        204,
        "set_visibility to restricted failed"
    );

    let list: serde_json::Value = ok_json(
        client
            .get(format!(
                "{}/api/embeds?object_type=dashboard&object_id={dash_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list embeds after restricting visibility",
    )
    .await;
    let live = list
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["token"].as_str() == Some(token.as_str()))
        .expect("the live link must still be listed once its dashboard is restricted");
    assert!(
        live["revoked_at"].is_null(),
        "the link is still live (not revoked) even though it now 404s on read"
    );
}

/// The legacy `DELETE /api/dashboards/{slug}/share` revoke must keep working after restriction:
/// turning a link off is never harder than turning it on.
#[tokio::test]
async fn legacy_revoke_still_works_after_visibility_is_restricted() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let viz_id = create_visualization(&client, &app.address, query_id).await;
    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;
    create_widget(&client, &app.address, dashboard_id, viz_id).await;
    publish_dashboard(&client, &app.address, &slug).await;

    let share: serde_json::Value = ok_json(
        client
            .post(format!("{}/api/dashboards/{slug}/share", app.address))
            .send()
            .await
            .unwrap(),
        "legacy share",
    )
    .await;
    let token = share["secret_token"].as_str().unwrap().to_string();

    let restrict = client
        .put(format!(
            "{}/api/dashboards/{dashboard_id}/visibility",
            app.address
        ))
        .json(&serde_json::json!({ "visibility": "restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        restrict.status().as_u16(),
        204,
        "set_visibility to restricted failed"
    );

    let revoke = client
        .delete(format!("{}/api/dashboards/{slug}/share", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        revoke.status().as_u16(),
        204,
        "revoking an already-restricted dashboard's public link must still succeed: {}",
        revoke.text().await.unwrap_or_default()
    );

    let list: serde_json::Value = ok_json(
        client
            .get(format!(
                "{}/api/embeds?object_type=dashboard&object_id={dashboard_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list embeds after legacy revoke",
    )
    .await;
    let revoked = list
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["token"].as_str() == Some(token.as_str()))
        .expect("the revoked link must still be listed (revoked, not deleted)");
    assert!(
        !revoked["revoked_at"].is_null(),
        "legacy revoke must have set revoked_at on the backing embeds row"
    );
}

/// A widget's query can be restricted independently of its dashboard, so `serialize_dashboard`
/// filters per widget: the embed serves the public-query widget and drops the restricted one.
#[tokio::test]
async fn public_embed_omits_a_widget_whose_query_is_not_public() {
    use arcodash_core::entities::organization;

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let public_query_id = create_query(&client, &app.address, ds_id).await;
    let private_query_id = create_query(&client, &app.address, ds_id).await;

    let public_viz_id = create_visualization(&client, &app.address, public_query_id).await;
    let private_viz_id = ok_json(
        client
            .post(format!("{}/api/visualizations", app.address))
            .json(&serde_json::json!({
                "query_id": private_query_id,
                "type": "chart",
                "name": "Private Widget Viz Marker",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create private visualization",
    )
    .await["id"]
        .as_i64()
        .unwrap();

    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;
    let public_widget_id = create_widget(&client, &app.address, dashboard_id, public_viz_id).await;
    create_widget(&client, &app.address, dashboard_id, private_viz_id).await;

    let restrict = client
        .put(format!(
            "{}/api/queries/{private_query_id}/visibility",
            app.address
        ))
        .json(&serde_json::json!({ "visibility": "restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        restrict.status().as_u16(),
        204,
        "restrict private query visibility failed"
    );

    publish_dashboard(&client, &app.address, &slug).await;

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    seed_query_result(&app, org.id, ds_id, public_query_id).await;
    seed_query_result(&app, org.id, ds_id, private_query_id).await;

    let embed = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "dashboard", "object_id": dashboard_id, "anonymous": true
            }))
            .send()
            .await
            .unwrap(),
        "create embed",
    )
    .await;
    let token = embed["token"].as_str().unwrap();

    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    let widgets = body["widgets"].as_array().unwrap();
    assert_eq!(
        widgets.len(),
        1,
        "only the widget backed by a public query may be served: {body}"
    );
    assert_eq!(
        widgets[0]["id"].as_i64(),
        Some(public_widget_id),
        "the surviving widget must be the public one, not just any single widget: {body}"
    );
    assert!(
        !body.to_string().contains("Private Widget Viz Marker"),
        "a widget backed by a non-public query must not leak into a public embed: {body}"
    );
}

#[tokio::test]
async fn public_embed_serves_monitor_tile_from_a_run_less_observation() {
    use arcodash_core::entities::{monitor_observation, organization};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id).await;
    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;
    create_monitor_widget(&client, &app.address, dashboard_id, &[monitor_id]).await;
    publish_dashboard(&client, &app.address, &slug).await;

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    monitor_observation::ActiveModel {
        id: NotSet,
        org_id: Set(org.id),
        monitor_id: Set(monitor_id),
        observed_at: Set(chrono::Utc::now().into()),
        value: Set(Some(42.0)),
        state: Set("ok".to_string()),
        run_id: Set(None),
        query_result_id: Set(None),
        explanation: Set(None),
    }
    .insert(&app.db)
    .await
    .expect("observation insert failed");

    let embed = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "dashboard", "object_id": dashboard_id, "anonymous": true
            }))
            .send()
            .await
            .unwrap(),
        "create embed",
    )
    .await;
    let token = embed["token"].as_str().unwrap();
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tile = &body["widgets"][0]["monitors"][0];
    assert_eq!(tile["observed"].as_f64(), Some(42.0), "{body}");
    assert!(tile["last_checked_at"].is_string(), "{body}");
    assert_eq!(tile["check_failed"], serde_json::json!(false));
}

#[tokio::test]
async fn public_embed_serves_monitor_tiles_with_state_and_observed_value() {
    use arcodash_core::entities::organization;

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id).await;

    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;
    create_monitor_widget(&client, &app.address, dashboard_id, &[monitor_id]).await;
    publish_dashboard(&client, &app.address, &slug).await;

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let qr_id = seed_query_result(&app, org.id, ds_id, query_id).await;
    insert_monitor_run(&app, org.id, monitor_id, qr_id).await;

    let embed = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "dashboard", "object_id": dashboard_id, "anonymous": true
            }))
            .send()
            .await
            .unwrap(),
        "create embed",
    )
    .await;
    let token = embed["token"].as_str().unwrap();

    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    let tiles = body["widgets"][0]["monitors"].as_array().unwrap();
    assert_eq!(tiles.len(), 1, "monitor tile must be attached: {body}");
    assert_eq!(tiles[0]["id"].as_i64(), Some(monitor_id));
    assert_eq!(tiles[0]["state"].as_str(), Some("unknown"));
    assert_eq!(
        tiles[0]["observed"],
        serde_json::json!(1),
        "the observed cell must come from the seeded run's result row: {body}"
    );
    assert!(
        tiles[0]["last_checked_at"].is_string(),
        "a terminal run must populate last_checked_at: {body}"
    );
    assert_eq!(
        body["as_of"], tiles[0]["last_checked_at"],
        "a monitor-only dashboard's as_of must follow its tiles' last check: {body}"
    );
    assert_eq!(tiles[0]["check_failed"], serde_json::json!(false));

    use arcodash_core::entities::run;
    let now = chrono::Utc::now();
    run::ActiveModel {
        id: NotSet,
        org_id: Set(org.id),
        entity_type: Set("monitor".to_string()),
        entity_id: Set(monitor_id),
        triggered_by: Set("manual".to_string()),
        status: Set("failed".to_string()),
        finished_at: Set(Some(now.into())),
        error: Set(Some("connection refused: db.internal:5432".to_string())),
        created_at: Set(now.into()),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .expect("failed run insert");
    let body: serde_json::Value = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tile = &body["widgets"][0]["monitors"][0];
    assert_eq!(tile["check_failed"], serde_json::json!(true), "{body}");
    assert!(
        tile["last_check_error"].is_null(),
        "public embeds must not expose run error text: {body}"
    );
    assert!(
        !body.to_string().contains("db.internal"),
        "error text leaked into the public payload: {body}"
    );
}

#[tokio::test]
async fn public_embed_drops_a_monitor_with_private_visibility() {
    use arcodash_core::entities::organization;

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id).await;
    set_monitor_visibility(&app, monitor_id, "restricted").await;

    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;
    create_monitor_widget(&client, &app.address, dashboard_id, &[monitor_id]).await;
    publish_dashboard(&client, &app.address, &slug).await;

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    seed_query_result(&app, org.id, ds_id, query_id).await;

    let embed = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "dashboard", "object_id": dashboard_id, "anonymous": true
            }))
            .send()
            .await
            .unwrap(),
        "create embed",
    )
    .await;
    let token = embed["token"].as_str().unwrap();

    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(
        body["widgets"][0]["monitors"],
        serde_json::json!([]),
        "a privately-visible monitor must be dropped, not served: {body}"
    );
    assert_eq!(body["widgets"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn public_embed_drops_a_monitor_whose_query_is_private() {
    use arcodash_core::entities::organization;

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id).await;
    let monitor_id = create_monitor(&client, &app.address, query_id).await;

    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;
    create_monitor_widget(&client, &app.address, dashboard_id, &[monitor_id]).await;
    publish_dashboard(&client, &app.address, &slug).await;

    // Restrict the monitor's bound QUERY (not the monitor) after publishing: its data must not be
    // served once the query is locked down.
    let restrict = client
        .put(format!("{}/api/queries/{query_id}/visibility", app.address))
        .json(&serde_json::json!({ "visibility": "restricted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(restrict.status().as_u16(), 204, "restrict query failed");

    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    seed_query_result(&app, org.id, ds_id, query_id).await;

    let embed = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "dashboard", "object_id": dashboard_id, "anonymous": true
            }))
            .send()
            .await
            .unwrap(),
        "create embed",
    )
    .await;
    let token = embed["token"].as_str().unwrap();

    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(
        body["widgets"][0]["monitors"],
        serde_json::json!([]),
        "a monitor bound to a now-private query must not leak its tile: {body}"
    );
    assert!(
        !body.to_string().contains("customer_secrets_marker_xyz"),
        "no trace of the restricted query's own text should leak either: {body}"
    );
}

#[tokio::test]
async fn public_embed_drops_a_monitor_id_belonging_to_another_org() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let foreign_monitor_id = seed_cross_org_public_monitor(&app).await;

    let (dashboard_id, slug) = create_dashboard(&client, &app.address).await;
    create_monitor_widget(&client, &app.address, dashboard_id, &[foreign_monitor_id]).await;
    publish_dashboard(&client, &app.address, &slug).await;

    let embed = ok_json(
        client
            .post(format!("{}/api/embeds", app.address))
            .json(&serde_json::json!({
                "object_type": "dashboard", "object_id": dashboard_id, "anonymous": true
            }))
            .send()
            .await
            .unwrap(),
        "create embed",
    )
    .await;
    let token = embed["token"].as_str().unwrap();

    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/api/embed/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(
        body["widgets"][0]["monitors"],
        serde_json::json!([]),
        "a monitor id from another org must never resolve, however it got into the widget's \
         options: {body}"
    );
}

/// Drives gate 4's `caller_org_id != Some(org.id)` branch directly via `resolve_public_embed`; the
/// HTTP path under RLS only reaches the "not signed in" branch.
#[tokio::test]
async fn resolve_public_embed_rejects_a_signed_in_caller_from_a_different_org() {
    use arcodash_backend::services::{
        dashboard_service, embed_service, embed_service::EmbedObjectType,
    };
    use arcodash_backend::PublicDashboardAccess;
    use arcodash_core::entities::{embed, organization};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let link_org_id = common::make_org(&app.db).await;
    let other_org_id = common::make_org(&app.db).await;
    let (owner, _) = common::user_with_role(&app.db, link_org_id, &["create_dashboard"]).await;

    let d = dashboard_service::create(&app.db, link_org_id, &owner, "Org-Scoped Link".to_string())
        .await
        .unwrap();

    // Minted directly: gate 4 only consults `anon_authorized`/`org_id`/liveness.
    let token = format!("embed-gate-test-{}", uuid::Uuid::new_v4().simple());
    embed::ActiveModel {
        id: NotSet,
        org_id: Set(link_org_id),
        object_type: Set(EmbedObjectType::Dashboard.as_str().to_string()),
        object_id: Set(d.id),
        mode: Set("public".to_string()),
        token: Set(Some(token.clone())),
        locked_params: Set(serde_json::json!({})),
        frame_ancestors: Set(None),
        expires_at: Set(None),
        created_by_id: Set(Some(owner.id)),
        anon_authorized: Set(false),
        revoked_at: Set(None),
        created_at: Set(chrono::Utc::now().into()),
    }
    .insert(&app.db)
    .await
    .unwrap();

    let link_org = organization::Entity::find_by_id(link_org_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();

    // Gate 4's `caller_org_id` branch needs `OrgMemberOnly`, so override the harness's `Anonymous`
    // state for this call.
    let mut state = app.state.clone();
    state.public_dashboard_access = PublicDashboardAccess::OrgMemberOnly;

    let wrong_org = embed_service::resolve_public_embed(
        &app.db,
        &state,
        &link_org,
        &token,
        Some(EmbedObjectType::Dashboard),
        Some(other_org_id),
    )
    .await;
    let err = match wrong_org {
        Ok(_) => panic!("a caller signed into a different org must not resolve"),
        Err(e) => e,
    };
    assert_eq!(
        err.status,
        axum::http::StatusCode::UNAUTHORIZED,
        "wrong-org caller must get 401, not a leak or a silent pass"
    );

    let same_org = embed_service::resolve_public_embed(
        &app.db,
        &state,
        &link_org,
        &token,
        Some(EmbedObjectType::Dashboard),
        Some(link_org_id),
    )
    .await;
    if let Err(e) = same_org {
        panic!(
            "a caller signed into the link's own org must resolve: {}",
            e.message
        );
    }
}
