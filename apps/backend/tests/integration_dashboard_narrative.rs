//! `POST|GET /api/dashboards/{id}/narrative` against a local OpenAI-compatible stub: gating,
//! per-reader widget omission, persistence, and that the stored digest never leaves the server.

mod common;
use common::{set_role_permissions, spawn_app, test_api_key, user_with_role, TestApp};

use arcodash_backend::services::permission_service;
use arcodash_core::entities::{
    dashboard, dashboard_narrative, data_source_group, group, organization, query, query_result,
    user, visualization, widget,
};
use arcodash_core::test_support;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

type Seen = Arc<Mutex<Vec<String>>>;

async fn org_id(app: &TestApp) -> i64 {
    organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .id
}

async fn admin_id(app: &TestApp) -> i64 {
    user::Entity::find()
        .filter(user::Column::Email.eq(common::ADMIN_EMAIL))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .id
}

/// Stub OpenAI chat-completions endpoint that records every request body.
async fn spawn_stub() -> (String, Seen) {
    use axum::{extract::State, routing::post, Json, Router};
    let seen: Seen = Arc::default();
    async fn handler(State(seen): State<Seen>, body: String) -> Json<Value> {
        seen.lock().unwrap().push(body);
        let content = json!({
            "summary": "Revenue is up.",
            "body": "**Revenue** is up.\n- [Rev](#widget-999999999) [x](https://evil.com)",
            "confidence": "high"
        })
        .to_string();
        Json(json!({
            "choices": [{ "message": { "content": content } }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
        }))
    }
    let router = Router::new()
        .route("/v1/chat/completions", post(handler))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://127.0.0.1:{port}"), seen)
}

async fn configure(admin: &reqwest::Client, base: &str, stub: &str, extra: Value) {
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let mut body = json!({
        "providers": { "openai": { "api_key": "sk-test-stub-key-0000000", "base_url": stub } },
        "provider_order": ["openai"]
    });
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let r = admin
        .post(format!("{base}/api/organization/ai"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "configure ai");
}

async fn reset_rate_limit(app: &TestApp, org: i64, user_id: i64) {
    use redis::AsyncCommands;
    let _: () = app
        .redis
        .clone()
        .del(format!("arcodash:ai_rate_limit:{org}:{user_id}"))
        .await
        .unwrap();
}

async fn new_ds(app: &TestApp, org: i64) -> i64 {
    let mut ds = test_support::new_data_source(org);
    ds.name = Set(format!("ds-{}", uuid::Uuid::new_v4().simple()));
    ds.insert(&app.db).await.unwrap().id
}

/// A widget showing one cached value on a fresh query over `ds_id`.
#[allow(clippy::too_many_arguments)]
async fn add_widget(
    app: &TestApp,
    org: i64,
    owner: i64,
    ds_id: i64,
    dash_id: i64,
    title: &str,
    value: i64,
    row: i64,
) -> i64 {
    let sfx = uuid::Uuid::new_v4().simple().to_string();
    let result = query_result::ActiveModel {
        org_id: Set(org),
        data_source_id: Set(ds_id),
        query_hash: Set(format!("h-{sfx}")),
        query: Set("SELECT 1".into()),
        data: Set(Some(
            json!({"columns": [{"name": "v"}], "rows": [{"v": value}]}),
        )),
        retrieved_at: Set(chrono::Utc::now().into()),
        last_used_at: Set(chrono::Utc::now().into()),
        created_at: Set(chrono::Utc::now().into()),
        updated_at: Set(chrono::Utc::now().into()),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();
    let mut q = test_support::new_query(org, ds_id, owner);
    q.api_key = Set(sfx.clone());
    q.latest_query_data_id = Set(Some(result.id));
    let q: query::Model = q.insert(&app.db).await.unwrap();
    let viz = visualization::ActiveModel {
        query_id: Set(q.id),
        r#type: Set("TABLE".into()),
        name: Set(title.into()),
        description: Set(None),
        options: Set(json!({})),
        created_at: Set(chrono::Utc::now().into()),
        updated_at: Set(chrono::Utc::now().into()),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();
    widget::ActiveModel {
        dashboard_id: Set(dash_id),
        visualization_id: Set(Some(viz.id)),
        text: Set(None),
        width: Set(1),
        options: Set(json!({"position": {"row": row, "col": 0}})),
        created_at: Set(chrono::Utc::now().into()),
        updated_at: Set(chrono::Utc::now().into()),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();
    q.id
}

/// Share an object with `grantee` as the admin owner.
async fn grant_viewer(app: &TestApp, kind: &str, id: i64, grantee: i64) {
    let admin = user::Entity::find_by_id(admin_id(app).await)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    permission_service::grant(
        &app.db,
        org_id(app).await,
        &admin,
        kind,
        id,
        Some(grantee),
        None,
        "viewer",
    )
    .await
    .expect("grant");
}

async fn new_dashboard(app: &TestApp, org: i64, owner: i64) -> dashboard::Model {
    let mut d = test_support::new_dashboard(org, owner);
    let sfx = uuid::Uuid::new_v4().simple().to_string();
    d.slug = Set(format!("nar-{sfx}"));
    d.visibility = Set("workspace".into());
    d.insert(&app.db).await.unwrap()
}

fn url(app: &TestApp, path: &str, key: Option<&str>) -> String {
    match key {
        Some(k) => format!("{}{path}?api_key={k}", app.address),
        None => format!("{}{path}", app.address),
    }
}

async fn post_json(app: &TestApp, c: &reqwest::Client, id: i64) -> Value {
    let r = c
        .post(url(app, &format!("/api/dashboards/{id}/narrative"), None))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    r.json().await.unwrap()
}

#[tokio::test]
async fn get_404_before_first_run() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let d = new_dashboard(&app, org_id(&app).await, admin_id(&app).await).await;
    let r = admin
        .get(url(
            &app,
            &format!("/api/dashboards/{}/narrative", d.id),
            None,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
}

#[tokio::test]
async fn viewer_without_dashboard_access_gets_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org = org_id(&app).await;
    let d = new_dashboard(&app, org, admin_id(&app).await).await;
    let (nobody, _) = user_with_role(&app.db, org, &[]).await;
    let key = test_api_key(&nobody.email);
    let http = reqwest::Client::new();
    let path = format!("/api/dashboards/{}/narrative", d.id);
    let get = http.get(url(&app, &path, Some(&key))).send().await.unwrap();
    assert_eq!(get.status().as_u16(), 404);
    let post = http
        .post(url(&app, &path, Some(&key)))
        .send()
        .await
        .unwrap();
    assert_eq!(post.status().as_u16(), 404);
}

#[tokio::test]
async fn setting_off_returns_disabled_and_hides_client_flag() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let org = org_id(&app).await;
    let uid = admin_id(&app).await;
    let (stub, seen) = spawn_stub().await;
    configure(&admin, &app.address, &stub, json!({})).await;
    let d = new_dashboard(&app, org, uid).await;
    let ds = new_ds(&app, org).await;
    add_widget(&app, org, uid, ds, d.id, "Rev", 100, 0).await;

    let flag = |admin: reqwest::Client, base: String| async move {
        let s: Value = admin
            .get(format!("{base}/api/session"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        s["client_config"]["ai_dashboard_narratives"].clone()
    };
    assert_eq!(flag(admin.clone(), app.address.clone()).await, json!(true));

    for off in [
        json!({ "dashboard_narratives": false }),
        json!({ "dashboard_narratives": true, "include_sample_values": false }),
    ] {
        configure(&admin, &app.address, &stub, off).await;
        reset_rate_limit(&app, org, uid).await;
        let body = post_json(&app, &admin, d.id).await;
        assert_eq!(body["status"], "disabled");
        assert_eq!(flag(admin.clone(), app.address.clone()).await, json!(false));
    }
    assert!(
        seen.lock().unwrap().is_empty(),
        "provider must not be called"
    );
}

#[tokio::test]
async fn widget_without_execute_reach_is_omitted_from_prompt() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let org = org_id(&app).await;
    let owner = admin_id(&app).await;
    let (stub, seen) = spawn_stub().await;
    configure(&admin, &app.address, &stub, json!({})).await;

    let (reader, _) = user_with_role(
        &app.db,
        org,
        &["view_dashboard", "view_query", "execute_query"],
    )
    .await;
    let g = reader.groups[0];
    let mut active: group::ActiveModel = group::Entity::find_by_id(g)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .into();
    active.data_source_access_restricted = Set(true);
    active.update(&app.db).await.unwrap();

    let open_ds = new_ds(&app, org).await;
    let closed_ds = new_ds(&app, org).await;
    data_source_group::ActiveModel {
        data_source_id: Set(open_ds),
        group_id: Set(g),
        created_at: Set(chrono::Utc::now().into()),
        updated_at: Set(chrono::Utc::now().into()),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();

    let d = new_dashboard(&app, org, owner).await;
    let q1 = add_widget(&app, org, owner, open_ds, d.id, "VisibleWidget", 4242, 0).await;
    let q2 = add_widget(&app, org, owner, closed_ds, d.id, "HiddenWidget", 7373, 1).await;
    grant_viewer(&app, "dashboard", d.id, reader.id).await;
    grant_viewer(&app, "query", q1, reader.id).await;
    grant_viewer(&app, "query", q2, reader.id).await;

    reset_rate_limit(&app, org, reader.id).await;
    let key = test_api_key(&reader.email);
    let r: Value = reqwest::Client::new()
        .post(url(
            &app,
            &format!("/api/dashboards/{}/narrative", d.id),
            Some(&key),
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["status"], "ok", "{r}");
    let prompts = seen.lock().unwrap().join("\n");
    assert!(prompts.contains("VisibleWidget") && prompts.contains("4242"));
    assert!(!prompts.contains("HiddenWidget") && !prompts.contains("7373"));
}

#[tokio::test]
async fn success_persists_row_and_get_returns_it_without_digest() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let org = org_id(&app).await;
    let uid = admin_id(&app).await;
    let (stub, _) = spawn_stub().await;
    configure(&admin, &app.address, &stub, json!({})).await;
    let d = new_dashboard(&app, org, uid).await;
    let ds = new_ds(&app, org).await;
    add_widget(&app, org, uid, ds, d.id, "Rev", 100, 0).await;

    reset_rate_limit(&app, org, uid).await;
    let body = post_json(&app, &admin, d.id).await;
    assert_eq!(body["status"], "ok", "{body}");
    assert_eq!(body["narrative"]["summary"], "Revenue is up.");
    assert_eq!(body["narrative"]["source"], "manual");
    assert_eq!(body["narrative"]["body"], "**Revenue** is up.\n- Rev x");
    assert_eq!(body["narrative"]["highlights"], json!([]));
    assert!(body["narrative"].get("digest").is_none());

    let rows = dashboard_narrative::Entity::find()
        .filter(dashboard_narrative::Column::DashboardId.eq(d.id))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].user_id, Some(uid));

    let got: Value = admin
        .get(url(
            &app,
            &format!("/api/dashboards/{}/narrative", d.id),
            None,
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["narrative"]["id"], body["narrative"]["id"]);
    assert_eq!(got["narrative"]["body"], body["narrative"]["body"]);
    assert!(got["narrative"].get("digest").is_none());
}

#[tokio::test]
async fn dashboard_narrative_endpoint_reads_from_storage() {
    use arcodash_core::org_storage::{StorageArea, StorageError};
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let org = org_id(&app).await;
    let uid = admin_id(&app).await;
    let (stub, _) = spawn_stub().await;
    configure(&admin, &app.address, &stub, json!({})).await;
    let d = new_dashboard(&app, org, uid).await;
    let ds = new_ds(&app, org).await;
    add_widget(&app, org, uid, ds, d.id, "Rev", 100, 0).await;

    reset_rate_limit(&app, org, uid).await;
    let posted = post_json(&app, &admin, d.id).await;
    assert_eq!(posted["status"], "ok", "{posted}");

    let row = dashboard_narrative::Entity::find()
        .filter(dashboard_narrative::Column::DashboardId.eq(d.id))
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(row.summary.is_none() && row.body.is_none() && row.digest.is_none());
    let rel = row.data_location.clone().expect("content location");
    let storage = app.state.storage.resolve(&app.db, org).await.unwrap();

    let stored: Value =
        serde_json::from_slice(&storage.get(StorageArea::Narratives, &rel).await.unwrap()).unwrap();
    let mut edited = stored.clone();
    edited["summary"] = json!("Read from the object.");
    storage
        .put(
            StorageArea::Narratives,
            &rel,
            serde_json::to_vec(&edited).unwrap().into(),
            "application/json",
        )
        .await
        .unwrap();

    let get = || async {
        admin
            .get(url(
                &app,
                &format!("/api/dashboards/{}/narrative", d.id),
                None,
            ))
            .send()
            .await
            .unwrap()
    };
    let got: Value = get().await.json().await.unwrap();
    assert_eq!(got["narrative"]["summary"], "Read from the object.");
    assert!(got["narrative"].get("digest").is_none());

    storage.delete(StorageArea::Narratives, &rel).await.unwrap();
    assert!(matches!(
        storage.get(StorageArea::Narratives, &rel).await,
        Err(StorageError::NotFound)
    ));
    assert_eq!(get().await.status(), 404);
}

#[tokio::test]
async fn get_hides_another_users_row() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let org = org_id(&app).await;
    let uid = admin_id(&app).await;
    let (stub, _) = spawn_stub().await;
    configure(&admin, &app.address, &stub, json!({})).await;
    let d = new_dashboard(&app, org, uid).await;
    let ds = new_ds(&app, org).await;
    add_widget(&app, org, uid, ds, d.id, "Rev", 100, 0).await;
    reset_rate_limit(&app, org, uid).await;
    assert_eq!(post_json(&app, &admin, d.id).await["status"], "ok");

    let (other, role) = user_with_role(&app.db, org, &["view_dashboard"]).await;
    set_role_permissions(&app.db, role, &["view_dashboard"]).await;
    grant_viewer(&app, "dashboard", d.id, other.id).await;
    let r = reqwest::Client::new()
        .get(url(
            &app,
            &format!("/api/dashboards/{}/narrative", d.id),
            Some(&test_api_key(&other.email)),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
}

#[tokio::test]
async fn public_dashboard_payload_has_no_narrative() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let org = org_id(&app).await;
    let uid = admin_id(&app).await;
    let (stub, _) = spawn_stub().await;
    configure(&admin, &app.address, &stub, json!({})).await;
    let d = new_dashboard(&app, org, uid).await;
    let ds = new_ds(&app, org).await;
    add_widget(&app, org, uid, ds, d.id, "Rev", 100, 0).await;
    reset_rate_limit(&app, org, uid).await;
    assert_eq!(post_json(&app, &admin, d.id).await["status"], "ok");

    let publish = admin
        .post(url(
            &app,
            &format!("/api/dashboards/{}/publish", d.slug),
            None,
        ))
        .send()
        .await
        .unwrap();
    assert!(
        publish.status().is_success(),
        "publish: {}",
        publish.status()
    );
    let share = admin
        .post(url(
            &app,
            &format!("/api/dashboards/{}/share", d.slug),
            None,
        ))
        .send()
        .await
        .unwrap();
    let share_status = share.status();
    let minted: Value = share.json().await.unwrap_or_default();
    let token = minted["secret_token"]
        .as_str()
        .unwrap_or_else(|| panic!("share token ({share_status}): {minted}"));
    let public = reqwest::Client::new()
        .get(url(&app, &format!("/api/dashboards/public/{token}"), None))
        .send()
        .await
        .unwrap();
    assert_eq!(public.status().as_u16(), 200);
    let text = public.text().await.unwrap();
    assert!(!text.contains("Revenue is up.") && !text.contains("narrative"));
}
