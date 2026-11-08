//! Integration tests for `POST /api/queries/{id}/suggest_monitors`: check ordering (no provider
//! call or `ai_usage` row for a refused caller), and the validated pipeline against a stubbed
//! OpenAI-compatible provider over a real published query + stored result.

mod common;
use common::ok_json;
use common::{authed_client, spawn_app, TestApp};

use arcodash_core::entities::{ai_usage, query, query_result};
use arcodash_core::test_support;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

async fn invite_and_accept(
    app: &TestApp,
    admin: &reqwest::Client,
    email: &str,
    password: &str,
) -> (i64, reqwest::Client) {
    let user_id = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": "Other User", "email": email, "groups": [] }))
            .send()
            .await
            .unwrap(),
        "invite user",
    )
    .await["id"]
        .as_i64()
        .unwrap();

    use sea_orm::EntityTrait as _;
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
    assert_eq!(accept.status().as_u16(), 200, "accept invitation");

    let (session, csrf) = app.login(email, password).await;
    (user_id, authed_client(&app.address, &session, &csrf))
}

/// Creates a role with exactly `perms` and puts `user_id` in a group carrying it — mirrors
/// `integration_monitors.rs::grant_create_monitor`, generalized to any permission set.
async fn grant_permissions(
    admin: &reqwest::Client,
    base: &str,
    user_id: i64,
    name: &str,
    perms: &[&str],
) {
    let role = ok_json(
        admin
            .post(format!("{base}/api/roles"))
            .json(&serde_json::json!({ "name": format!("{name} role"), "permissions": perms }))
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
            .json(&serde_json::json!({ "name": format!("{name}-group") }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();

    // New groups are restricted (no data-source allowlist) by default — widen to unrestricted so
    // this group's member can reach whichever data source the calling test created, without
    // needing a second API call per test to allowlist it explicitly.
    let widen = admin
        .put(format!("{base}/api/groups/{group_id}/data_source_access"))
        .json(&serde_json::json!({ "restricted": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(widen.status().as_u16(), 200);

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

/// Configures a bogus-but-shape-valid OpenAI key so `effective.provider.is_none()` is false —
/// enough to get the handler past the `not_configured` bail into the request-shape checks that
/// run before any actual provider call. `base_url`, when given, redirects the real dispatch to a
/// local stub instead of the real OpenAI API.
async fn configure_openai_provider(
    admin: &reqwest::Client,
    base: &str,
    api_key: &str,
    base_url: Option<&str>,
) {
    let mut providers = serde_json::json!({ "openai": { "api_key": api_key } });
    // A localhost stub base_url trips the settings endpoint's own SSRF validation
    // (validate_base_url -> guard_url), so saving it needs the same guard create_ds_real uses.
    if let Some(url) = base_url {
        providers["openai"]["base_url"] = serde_json::json!(url);
        std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    }
    let resp = admin
        .post(format!("{base}/api/organization/ai"))
        .json(&serde_json::json!({ "providers": providers, "provider_order": ["openai"] }))
        .send()
        .await
        .unwrap();
    if base_url.is_some() {
        std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    }
    assert_eq!(resp.status().as_u16(), 200, "configure openai provider");
}

/// Clears this org+user's AI rate-limit counter in the shared valkey instance — it lives outside
/// `cleanup()`'s DB truncate, keyed only by (org_id, user_id) with a 60s TTL, so back-to-back
/// binary runs (or other provider-configured tests using the same fixed admin id=1) would
/// otherwise leave a test that actually exercises the limiter partway or fully spent before its
/// own requests begin.
async fn reset_ai_rate_limit(app: &TestApp, org_id: i64, user_id: i64) {
    use redis::AsyncCommands;
    let _: () = app
        .redis
        .clone()
        .del(format!("arcodash:ai_rate_limit:{org_id}:{user_id}"))
        .await
        .unwrap();
}

async fn ai_usage_count(db: &sea_orm::DatabaseConnection, org_id: i64) -> usize {
    ai_usage::Entity::find()
        .filter(ai_usage::Column::OrgId.eq(org_id))
        .all(db)
        .await
        .unwrap()
        .len()
}

/// Stub OpenAI-compatible server answering every chat completion with `content` as the model text.
async fn spawn_stub_openai(content: serde_json::Value) -> String {
    use axum::{routing::post, Json, Router};
    use std::sync::Arc;

    async fn handler(
        axum::extract::State(content): axum::extract::State<Arc<String>>,
    ) -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "choices": [{ "message": { "content": (*content).clone() } }],
            "usage": { "prompt_tokens": 42, "completion_tokens": 7 }
        }))
    }

    let app = Router::new()
        .route("/v1/chat/completions", post(handler))
        .with_state(Arc::new(content.to_string()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

async fn create_ds(client: &reqwest::Client, base: &str) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/data_sources"))
            .json(&serde_json::json!({ "name": "qms-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

async fn create_query(client: &reqwest::Client, base: &str, ds_id: i64, publish: bool) -> i64 {
    let id = ok_json(
        client
            .post(format!("{base}/api/queries"))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Refunds",
                "query": "SELECT 1 AS refund_rate, 'x' AS region",
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
    if publish {
        ok_json(
            client
                .post(format!("{base}/api/queries/{id}/publish"))
                .json(&serde_json::json!({ "expected_version": 0 }))
                .send()
                .await
                .unwrap(),
            "publish query",
        )
        .await;
    }
    id
}

/// Stores a result for the query and points the published result pointer at it.
async fn seed_published_result(app: &TestApp, query_id: i64) {
    let q = query::Entity::find_by_id(query_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let now = chrono::Utc::now().fixed_offset();
    let r = query_result::ActiveModel {
        org_id: Set(q.org_id),
        data_source_id: Set(q.data_source_id),
        query_hash: Set(q.query_hash.clone()),
        query: Set(q.query.clone()),
        data: Set(Some(serde_json::json!({
            "columns": [
                {"name": "refund_rate", "friendly_type": "float"},
                {"name": "region", "friendly_type": "string"}
            ],
            "rows": [{"refund_rate": 0.02, "region": "eu"}],
            "truncated": false
        }))),
        retrieved_at: Set(now),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();
    let mut am: query::ActiveModel = q.into();
    am.published_latest_query_data_id = Set(Some(r.id));
    am.update(&app.db).await.unwrap();
}

async fn ready_query(app: &TestApp, admin: &reqwest::Client) -> i64 {
    let ds = create_ds(admin, &app.address).await;
    let qid = create_query(admin, &app.address, ds, true).await;
    seed_published_result(app, qid).await;
    qid
}

async fn suggest(
    client: &reqwest::Client,
    base: &str,
    query_id: i64,
    body: serde_json::Value,
) -> reqwest::Response {
    // The stub provider lives on localhost, which the SSRF guard blocks without this.
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let resp = client
        .post(format!("{base}/api/queries/{query_id}/suggest_monitors"))
        .json(&body)
        .send()
        .await
        .unwrap();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    resp
}

fn item(name: &str, kind: &str, column: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name, "kind": kind, "column": column, "sensitivity": "medium",
        "op": if kind == "threshold" { ">" } else { "" },
        "value": if kind == "threshold" { 0.1 } else { 0.0 },
        "definition": format!("`{column}` check"), "why": "matters", "confidence": "high"
    })
}

#[tokio::test]
async fn happy_path_returns_validated_suggestions_and_usage_row() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let qid = ready_query(&app, &admin).await;
    let stub = spawn_stub_openai(serde_json::json!({
        "suggestions": [
            item("Refund rate drift", "baseline", "refund_rate"),
            item("Region cap", "threshold", "region"),
            item("Refund rate cap", "threshold", "refund_rate"),
        ],
        "unsupported": ""
    }))
    .await;
    configure_openai_provider(
        &admin,
        &app.address,
        "sk-test-query-monitors-0000",
        Some(&stub),
    )
    .await;

    let body = ok_json(
        suggest(&admin, &app.address, qid, serde_json::json!({})).await,
        "suggest",
    )
    .await;
    assert_eq!(body["status"], "ok", "{body}");
    let s = body["suggestions"].as_array().unwrap();
    assert_eq!(s.len(), 2, "{body}");
    assert_eq!(s[0]["name"], "Refund rate drift");
    assert_eq!(s[0]["kind"], "baseline");
    assert!(s[0]["op"].is_null());
    assert_eq!(s[1]["name"], "Refund rate cap");
    assert_eq!(s[1]["op"], ">");
    assert!(body["unsupported"].is_null());
    assert_eq!(body["served_by"], "openai");

    let rows = ai_usage::Entity::find()
        .filter(ai_usage::Column::OrgId.eq(1))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].feature, "monitor_suggestions_query");
    assert_eq!(rows[0].user_id, Some(1));
}

#[tokio::test]
async fn already_covered_reports_existing_monitor() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let qid = ready_query(&app, &admin).await;
    let monitor = ok_json(
        admin
            .post(format!("{}/api/monitors", app.address))
            .json(&serde_json::json!({
                "query_id": qid,
                "name": "Existing drift",
                "options": { "kind": "baseline", "column": "refund_rate" }
            }))
            .send()
            .await
            .unwrap(),
        "create monitor",
    )
    .await;
    let stub = spawn_stub_openai(serde_json::json!({
        "suggestions": [item("Refund rate drift", "baseline", "refund_rate")],
        "unsupported": ""
    }))
    .await;
    configure_openai_provider(
        &admin,
        &app.address,
        "sk-test-query-monitors-0000",
        Some(&stub),
    )
    .await;

    let body = ok_json(
        suggest(&admin, &app.address, qid, serde_json::json!({})).await,
        "suggest",
    )
    .await;
    assert_eq!(body["suggestions"].as_array().unwrap().len(), 0, "{body}");
    assert_eq!(body["already_covered"][0]["monitor_id"], monitor["id"]);
    assert_eq!(body["already_covered"][0]["column"], "refund_rate");
}

#[tokio::test]
async fn prompt_mode_unsupported() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let qid = ready_query(&app, &admin).await;
    let stub = spawn_stub_openai(serde_json::json!({
        "suggestions": [], "unsupported": "no refund column"
    }))
    .await;
    configure_openai_provider(
        &admin,
        &app.address,
        "sk-test-query-monitors-0000",
        Some(&stub),
    )
    .await;

    let body = ok_json(
        suggest(
            &admin,
            &app.address,
            qid,
            serde_json::json!({ "prompt": "alert on refunds" }),
        )
        .await,
        "suggest",
    )
    .await;
    assert_eq!(body["unsupported"], "no refund column");
    assert_eq!(body["suggestions"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn unpublished_query_409() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let ds = create_ds(&admin, &app.address).await;
    let qid = create_query(&admin, &app.address, ds, false).await;
    configure_openai_provider(&admin, &app.address, "sk-test-query-monitors-0000", None).await;

    let resp = suggest(&admin, &app.address, qid, serde_json::json!({})).await;
    assert_eq!(resp.status().as_u16(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error_code"], "query_unpublished");
    assert_eq!(ai_usage_count(&app.db, 1).await, 0);
}

#[tokio::test]
async fn never_run_query_409_no_result() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let ds = create_ds(&admin, &app.address).await;
    let qid = create_query(&admin, &app.address, ds, true).await;
    configure_openai_provider(&admin, &app.address, "sk-test-query-monitors-0000", None).await;

    let resp = suggest(&admin, &app.address, qid, serde_json::json!({})).await;
    assert_eq!(resp.status().as_u16(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error_code"], "no_result");
    assert_eq!(ai_usage_count(&app.db, 1).await, 0);
}

#[tokio::test]
async fn other_org_query_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let mut other_org = test_support::new_organization();
    other_org.slug = Set(format!("other-org-{}", uuid::Uuid::new_v4().simple()));
    let other_org = other_org.insert(&app.db).await.expect("insert other org");
    let ds = test_support::new_data_source(other_org.id)
        .insert(&app.db)
        .await
        .unwrap();
    let q = test_support::new_query(other_org.id, ds.id, 1)
        .insert(&app.db)
        .await
        .unwrap();

    let resp = suggest(&admin, &app.address, q.id, serde_json::json!({})).await;
    assert_eq!(resp.status().as_u16(), 404);
    assert_eq!(ai_usage_count(&app.db, 1).await, 0);
}

#[tokio::test]
async fn missing_create_monitor_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let qid = ready_query(&app, &admin).await;
    configure_openai_provider(&admin, &app.address, "sk-test-query-monitors-0000", None).await;
    let (uid, other) =
        invite_and_accept(&app, &admin, "no-monitor@test.local", "N0Mon!Pass1").await;
    grant_permissions(
        &admin,
        &app.address,
        uid,
        "viewer",
        &["view_query", "execute_query", "view_data_source"],
    )
    .await;
    let acl = admin
        .post(format!("{}/api/queries/{qid}/acl", app.address))
        .json(&serde_json::json!({ "user_id": uid, "access_type": "view" }))
        .send()
        .await
        .unwrap();
    assert_eq!(acl.status().as_u16(), 200, "grant query view");

    let resp = suggest(&other, &app.address, qid, serde_json::json!({})).await;
    assert_eq!(resp.status().as_u16(), 403);
    assert_eq!(ai_usage_count(&app.db, 1).await, 0);
}

#[tokio::test]
async fn not_configured_returns_tagged_status() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let qid = ready_query(&app, &admin).await;

    let body = ok_json(
        suggest(&admin, &app.address, qid, serde_json::json!({})).await,
        "suggest",
    )
    .await;
    assert_eq!(body["status"], "not_configured");
    assert_eq!(ai_usage_count(&app.db, 1).await, 0);
}
