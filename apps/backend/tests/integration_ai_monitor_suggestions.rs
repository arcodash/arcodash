//! Integration tests for `POST /api/data_sources/{id}/suggest_monitors`: access/authz ordering,
//! entitlement gate, request validation, rate limiter, and the full pipeline against a stubbed
//! OpenAI-compatible provider. `create_ds_real` uses the shared `target-postgres` fixture and needs
//! `ALLOW_PRIVATE_ENV=1` (the SSRF guard blocks localhost) around both the create and suggest
//! calls.

mod common;
use arcodash_backend::AppExtensions;
use common::ok_json;
use common::{authed_client, spawn_app, spawn_app_with_extensions, TestApp};

use arcodash_core::entities::ai_usage;
use arcodash_core::test_support;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

fn target_pg_options() -> serde_json::Value {
    serde_json::json!({
        "host": "localhost",
        "port": 5434,
        "dbname": "target_db",
        "user": "target",
        "password": "target"
    })
}

/// A `pg` data source on the real `target-postgres` fixture. Callers wrap any use that reaches the
/// connector in the `ALLOW_PRIVATE_ENV` guard.
async fn create_ds_real(client: &reqwest::Client, base: &str) -> i64 {
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let id = ok_json(
        client
            .post(format!("{base}/api/data_sources"))
            .json(&serde_json::json!({
                "name": "suggest-monitors-ds",
                "type": "pg",
                "options": target_pg_options()
            }))
            .send()
            .await
            .unwrap(),
        "create real ds",
    )
    .await["id"]
        .as_i64()
        .unwrap();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    id
}

/// A `pg` data source with no real connection, for tests that never reach the schema fetch.
async fn create_ds_fake(client: &reqwest::Client, base: &str) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/data_sources"))
            .json(&serde_json::json!({ "name": "fake-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create fake ds",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

async fn suggest(
    client: &reqwest::Client,
    base: &str,
    ds_id: i64,
    tables: &[&str],
) -> reqwest::Response {
    client
        .post(format!("{base}/api/data_sources/{ds_id}/suggest_monitors"))
        .json(&serde_json::json!({ "tables": tables }))
        .send()
        .await
        .unwrap()
}

async fn suggest_real(
    client: &reqwest::Client,
    base: &str,
    ds_id: i64,
    tables: &[&str],
) -> reqwest::Response {
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let resp = suggest(client, base, ds_id, tables).await;
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    resp
}

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

/// Creates a role with exactly `perms` and puts `user_id` in a group carrying it.
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

    // New groups are restricted by default; widen to unrestricted so the member can reach any test
    // data source.
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

/// Configures a bogus-but-valid OpenAI key so `effective.provider.is_none()` is false; `base_url`
/// redirects dispatch to a local stub.
async fn configure_openai_provider(
    admin: &reqwest::Client,
    base: &str,
    api_key: &str,
    base_url: Option<&str>,
) {
    let mut providers = serde_json::json!({ "openai": { "api_key": api_key } });
    // A localhost stub base_url trips the settings endpoint's SSRF validation, so saving it needs
    // the same guard as `create_ds_real`.
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

/// Clears this org+user's AI rate-limit counter in valkey; it lives outside `cleanup()`'s truncate
/// (60s TTL) and could leave a limiter test partly spent.
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

#[tokio::test]
async fn other_orgs_data_source_returns_404_not_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let mut other_org = test_support::new_organization();
    other_org.slug = Set(format!("other-org-{}", uuid::Uuid::new_v4().simple()));
    let other_org = other_org.insert(&app.db).await.expect("insert other org");
    let other_ds = test_support::new_data_source(other_org.id)
        .insert(&app.db)
        .await
        .expect("insert other org ds");

    let before = ai_usage_count(&app.db, 1).await;
    let resp = suggest(&admin, &app.address, other_ds.id, &["orders"]).await;
    assert_eq!(resp.status().as_u16(), 404);
    assert_eq!(
        ai_usage_count(&app.db, 1).await,
        before,
        "no ai_usage row on a denied caller"
    );
}

#[tokio::test]
async fn nonexistent_data_source_returns_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let resp = suggest(&admin, &app.address, 999999, &["orders"]).await;
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn caller_missing_create_query_gets_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds_fake(&admin, &app.address).await;
    let (uid, other) =
        invite_and_accept(&app, &admin, "no-create-query@test.local", "N0Query!Pass").await;
    // create_monitor only — missing create_query.
    grant_permissions(
        &admin,
        &app.address,
        uid,
        "monitor-only",
        &["view_data_source", "create_monitor"],
    )
    .await;

    let before = ai_usage_count(&app.db, 1).await;
    let resp = suggest(&other, &app.address, ds_id, &["orders"]).await;
    assert_eq!(resp.status().as_u16(), 404);
    assert_eq!(ai_usage_count(&app.db, 1).await, before);
}

#[tokio::test]
async fn caller_missing_create_monitor_gets_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds_fake(&admin, &app.address).await;
    let (uid, other) = invite_and_accept(
        &app,
        &admin,
        "no-create-monitor@test.local",
        "N0M0nitor!Pass",
    )
    .await;
    // create_query only — missing create_monitor.
    grant_permissions(
        &admin,
        &app.address,
        uid,
        "query-only",
        &["view_data_source", "create_query"],
    )
    .await;

    let resp = suggest(&other, &app.address, ds_id, &["orders"]).await;
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn caller_with_both_permissions_reaches_request_shape_checks() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds_id = create_ds_fake(&admin, &app.address).await;
    let (uid, other) =
        invite_and_accept(&app, &admin, "both-perms@test.local", "B0thPerms!Pass").await;
    grant_permissions(
        &admin,
        &app.address,
        uid,
        "both",
        &["view_data_source", "create_query", "create_monitor"],
    )
    .await;

    let resp = suggest(&other, &app.address, ds_id, &[]).await;
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn default_org_is_not_blocked_by_the_entitlement_gate() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds_fake(&admin, &app.address).await;

    let resp = suggest(&admin, &app.address, ds_id, &[]).await;
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn ai_endpoint_returns_forbidden_when_entitlements_deny_ai() {
    let app = skip_if_none!(
        spawn_app_with_extensions(AppExtensions {
            entitlements: Some(std::sync::Arc::new(common::DenyAll)),
            ..AppExtensions::default()
        })
        .await
    );
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds_fake(&admin, &app.address).await;

    let before = ai_usage_count(&app.db, 1).await;
    let resp = suggest(&admin, &app.address, ds_id, &["orders"]).await;
    assert_eq!(resp.status().as_u16(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["message"],
        "AI data-quality suggestions aren't enabled for this organization"
    );
    assert_eq!(
        ai_usage_count(&app.db, 1).await,
        before,
        "provider must never be reached"
    );
}

#[tokio::test]
async fn empty_tables_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds_fake(&admin, &app.address).await;

    let resp = suggest(&admin, &app.address, ds_id, &[]).await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["message"], "tables must not be empty");
}

#[tokio::test]
async fn twenty_one_tables_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds_fake(&admin, &app.address).await;

    let names: Vec<String> = (0..21).map(|i| format!("t{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let resp = suggest(&admin, &app.address, ds_id, &refs).await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["message"], "tables must not exceed 20 entries");
}

#[tokio::test]
async fn unknown_table_name_returns_400_rather_than_silently_dropping() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let ds_id = create_ds_real(&admin, &app.address).await;
    configure_openai_provider(&admin, &app.address, "sk-test-not-a-real-key-000", None).await;

    let resp = suggest_real(
        &admin,
        &app.address,
        ds_id,
        &["orders", "definitely_not_a_table"],
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("definitely_not_a_table"),
        "message should name the bad table: {body}"
    );
}

#[tokio::test]
async fn no_provider_configured_returns_200_not_configured() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds_fake(&admin, &app.address).await;

    let resp = suggest(&admin, &app.address, ds_id, &["orders"]).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "not_configured");
}

#[tokio::test]
async fn rate_limit_exhausted_returns_200_rate_limited_charged_once_per_request() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    // A configured provider is required to reach the rate limiter (`not_configured` is returned
    // before it), so this needs a real ds and a stub provider that answers.
    let ds_id = create_ds_real(&admin, &app.address).await;

    // Fresh user: the limiter is process-global.
    let (uid, other) =
        invite_and_accept(&app, &admin, "rate-limit-user@test.local", "R4teL1mit!Pass").await;
    grant_permissions(
        &admin,
        &app.address,
        uid,
        "rate-limited",
        &["view_data_source", "create_query", "create_monitor"],
    )
    .await;

    // `cleanup()` restarts user identities, so this user gets the same id every run; reset its
    // counter too.
    reset_ai_rate_limit(&app, 1, uid).await;

    let stub_url = spawn_stub_openai(serde_json::json!([raw_suggestion(
        "orders",
        "row_count",
        "",
        "row_count",
        "SELECT COUNT(*) AS row_count FROM orders",
    )]))
    .await;
    configure_openai_provider(
        &admin,
        &app.address,
        "sk-test-rate-limit-0000",
        Some(&stub_url),
    )
    .await;

    for i in 0..10 {
        let resp = suggest_real(&other, &app.address, ds_id, &["orders"]).await;
        assert_eq!(
            resp.status().as_u16(),
            200,
            "request {i} must not yet be rate limited"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["status"], "ok", "request {i}: {body}");
    }

    let resp = suggest_real(&other, &app.address, ds_id, &["orders"]).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "rate_limited");
    assert!(body["retry_after_secs"].as_u64().is_some());
}

// The full pipeline against a stubbed provider: a tiny axum server answers `/v1/chat/completions`
// with a canned suggestions envelope via the `base_url` override, wired to a real DB.

/// Starts a stub OpenAI-compatible server on port 0 answering `/v1/chat/completions` with
/// `suggestions`; returns `http://127.0.0.1:<port>`.
async fn spawn_stub_openai(suggestions: serde_json::Value) -> String {
    use axum::{routing::post, Json, Router};
    use std::sync::Arc;

    #[derive(Clone)]
    struct StubState {
        content: Arc<String>,
    }

    async fn handler(
        axum::extract::State(state): axum::extract::State<StubState>,
    ) -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "choices": [{ "message": { "content": (*state.content).clone() } }],
            "usage": { "prompt_tokens": 42, "completion_tokens": 7 }
        }))
    }

    let content = serde_json::json!({ "suggestions": suggestions }).to_string();
    let state = StubState {
        content: Arc::new(content),
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

fn raw_suggestion(
    table: &str,
    kind: &str,
    target_column: &str,
    column: &str,
    sql: &str,
) -> serde_json::Value {
    serde_json::json!({
        "table": table,
        "kind": kind,
        "target_column": target_column,
        "title": format!("{table} {kind} check"),
        "rationale": format!("Alerts on {kind} for {table}."),
        "sql": sql,
        "column": column,
        "op": ">",
        "value": 1.0
    })
}

#[tokio::test]
async fn stub_pipeline_normal_response_grouped_in_schema_order() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let ds_id = create_ds_real(&admin, &app.address).await;

    let stub_url = spawn_stub_openai(serde_json::json!([
        raw_suggestion(
            "orders",
            "row_count",
            "",
            "row_count",
            "SELECT COUNT(*) AS row_count FROM orders",
        ),
        raw_suggestion(
            "customers",
            "row_count",
            "",
            "row_count",
            "SELECT COUNT(*) AS row_count FROM customers",
        ),
    ]))
    .await;
    configure_openai_provider(
        &admin,
        &app.address,
        "sk-test-stub-key-0000000",
        Some(&stub_url),
    )
    .await;

    let resp = suggest_real(&admin, &app.address, ds_id, &["orders", "customers"]).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok", "body: {body}");
    assert_eq!(body["served_by"], "openai");
    assert_eq!(body["scanned_tables"], 2);
    let suggestions = body["suggestions"].as_array().unwrap();
    assert_eq!(suggestions.len(), 2);
    let tables: Vec<&str> = suggestions
        .iter()
        .map(|s| s["table"].as_str().unwrap())
        .collect();
    assert!(tables.contains(&"orders") && tables.contains(&"customers"));

    let usage_rows = ai_usage::Entity::find()
        .filter(ai_usage::Column::OrgId.eq(1))
        .filter(ai_usage::Column::Feature.eq("monitor_suggestions"))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(usage_rows.len(), 1, "exactly one ai_usage row for this run");
    assert_eq!(usage_rows[0].status, "ok");
    assert_eq!(usage_rows[0].tokens_in, Some(42));
    assert_eq!(usage_rows[0].tokens_out, Some(7));
}

/// Floor: an empty `suggestions` array still yields a synthesized row-count suggestion for every
/// in-scope table.
#[tokio::test]
async fn stub_pipeline_empty_response_still_synthesizes_row_count_floor() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let ds_id = create_ds_real(&admin, &app.address).await;

    let stub_url = spawn_stub_openai(serde_json::json!([])).await;
    configure_openai_provider(
        &admin,
        &app.address,
        "sk-test-stub-key-1111111",
        Some(&stub_url),
    )
    .await;

    let resp = suggest_real(&admin, &app.address, ds_id, &["orders"]).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok", "body: {body}");
    let suggestions = body["suggestions"].as_array().unwrap();
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0]["table"], "orders");
    assert_eq!(suggestions[0]["kind"], "row_count");
}

/// A suggestion naming a table outside the schema is dropped; the run still succeeds.
#[tokio::test]
async fn stub_pipeline_drops_off_schema_table_without_failing_the_run() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let ds_id = create_ds_real(&admin, &app.address).await;

    let stub_url = spawn_stub_openai(serde_json::json!([
        raw_suggestion(
            "definitely_not_a_table",
            "row_count",
            "",
            "row_count",
            "SELECT COUNT(*) AS row_count FROM definitely_not_a_table",
        ),
        raw_suggestion(
            "orders",
            "row_count",
            "",
            "row_count",
            "SELECT COUNT(*) AS row_count FROM orders",
        ),
    ]))
    .await;
    configure_openai_provider(
        &admin,
        &app.address,
        "sk-test-stub-key-2222222",
        Some(&stub_url),
    )
    .await;

    let resp = suggest_real(&admin, &app.address, ds_id, &["orders"]).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok", "body: {body}");
    let suggestions = body["suggestions"].as_array().unwrap();
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0]["table"], "orders");
}

/// Cross-run dedup: an existing monitor on `orders`' uniqueness check suppresses a same-shape
/// suggestion, but a different-column one is still offered.
#[tokio::test]
async fn stub_pipeline_cross_run_dedup_against_a_real_existing_monitor() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let ds_id = create_ds_real(&admin, &app.address).await;

    let query = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Orders uniqueness",
                "query": "SELECT COUNT(*) - COUNT(DISTINCT id) AS duplicate_count FROM orders",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create dedup query",
    )
    .await;
    let query_id = query["id"].as_i64().unwrap();
    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish dedup query",
    )
    .await;
    let monitor = ok_json(
        admin
            .post(format!("{}/api/monitors", app.address))
            .json(&serde_json::json!({
                "query_id": query_id,
                "name": "Orders uniqueness monitor",
                "options": { "op": ">", "value": 0, "column": "duplicate_count" }
            }))
            .send()
            .await
            .unwrap(),
        "create dedup monitor",
    )
    .await;
    let _monitor_id = monitor["id"].as_i64().unwrap();

    let stub_url = spawn_stub_openai(serde_json::json!([
        raw_suggestion(
            "orders",
            "uniqueness",
            "id",
            "duplicate_count",
            "SELECT COUNT(*) - COUNT(DISTINCT id) AS duplicate_count FROM orders",
        ),
        raw_suggestion(
            "orders",
            "null_rate",
            "customer_id",
            "null_pct",
            "SELECT 100.0 * COUNT(*) FILTER (WHERE customer_id IS NULL) / NULLIF(COUNT(*),0) AS null_pct FROM orders",
        ),
    ]))
    .await;
    configure_openai_provider(
        &admin,
        &app.address,
        "sk-test-stub-key-3333333",
        Some(&stub_url),
    )
    .await;

    let resp = suggest_real(&admin, &app.address, ds_id, &["orders"]).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok", "body: {body}");

    let suggestions = body["suggestions"].as_array().unwrap();
    assert!(
        suggestions.iter().all(|s| s["kind"] != "uniqueness"),
        "the uniqueness suggestion must be suppressed as covered: {suggestions:?}"
    );
    assert!(
        suggestions.iter().any(|s| s["kind"] == "null_rate"),
        "a different-column suggestion on the same table must NOT be over-suppressed: {suggestions:?}"
    );
    // A `CoveredTable` row is only emitted when every candidate for a table was suppressed; this
    // table still has a survivor.
    assert!(body["covered"].as_array().unwrap().is_empty());
}

/// If covering empties a table's entire candidate set, it shows as a muted `CoveredTable` row
/// instead of vanishing.
#[tokio::test]
async fn stub_pipeline_cross_run_dedup_emits_covered_table_when_it_is_the_only_candidate() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let ds_id = create_ds_real(&admin, &app.address).await;

    let query = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Orders uniqueness only",
                "query": "SELECT COUNT(*) - COUNT(DISTINCT id) AS duplicate_count FROM orders",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create dedup-only query",
    )
    .await;
    let query_id = query["id"].as_i64().unwrap();
    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish dedup-only query",
    )
    .await;
    let monitor = ok_json(
        admin
            .post(format!("{}/api/monitors", app.address))
            .json(&serde_json::json!({
                "query_id": query_id,
                "name": "Orders uniqueness only monitor",
                "options": { "op": ">", "value": 0, "column": "duplicate_count" }
            }))
            .send()
            .await
            .unwrap(),
        "create dedup-only monitor",
    )
    .await;
    let monitor_id = monitor["id"].as_i64().unwrap();

    // Only the covered shape is offered for `orders`, so nothing keeps its candidate set non-empty.
    let stub_url = spawn_stub_openai(serde_json::json!([raw_suggestion(
        "orders",
        "uniqueness",
        "id",
        "duplicate_count",
        "SELECT COUNT(*) - COUNT(DISTINCT id) AS duplicate_count FROM orders",
    )]))
    .await;
    configure_openai_provider(
        &admin,
        &app.address,
        "sk-test-stub-key-4444444",
        Some(&stub_url),
    )
    .await;

    let resp = suggest_real(&admin, &app.address, ds_id, &["orders"]).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok", "body: {body}");
    assert!(
        body["suggestions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["table"] != "orders"),
        "orders must not appear in suggestions once fully covered: {body}"
    );
    let covered = body["covered"].as_array().unwrap();
    assert_eq!(covered.len(), 1, "body: {body}");
    assert_eq!(covered[0]["table"], "orders");
    assert_eq!(covered[0]["monitor_id"], monitor_id);
}

/// A cached schema serves `suggest_monitors` without touching the warehouse (the live path would
/// 502).
#[tokio::test]
async fn cached_schema_is_used_without_a_live_lookup() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let ds_id = create_ds_fake(&admin, &app.address).await;

    let ds = arcodash_core::entities::data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: arcodash_core::entities::data_source::ActiveModel = ds.into();
    active.schema = Set(Some(serde_json::json!({ "tables": [{
        "name": "orders",
        "columns": [{ "name": "id", "type_name": "integer" }]
    }]})));
    active.update(&app.db).await.unwrap();

    let stub_url = spawn_stub_openai(serde_json::json!([])).await;
    configure_openai_provider(
        &admin,
        &app.address,
        "sk-test-stub-key-2222222",
        Some(&stub_url),
    )
    .await;

    let resp = suggest_real(&admin, &app.address, ds_id, &["orders"]).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok", "body: {body}");
    assert_eq!(body["scanned_tables"], 1);
}

/// A requested table missing from the cache triggers a live re-check, not a 400 (the 502 shows the
/// lookup was attempted).
#[tokio::test]
async fn table_missing_from_cache_rechecks_live_instead_of_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let ds_id = common::create_unroutable_pg_ds(&admin, &app.address).await;

    let ds = arcodash_core::entities::data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: arcodash_core::entities::data_source::ActiveModel = ds.into();
    active.schema = Set(Some(serde_json::json!({ "tables": [{
        "name": "orders",
        "columns": [{ "name": "id", "type_name": "integer" }]
    }]})));
    active.update(&app.db).await.unwrap();

    let stub_url = spawn_stub_openai(serde_json::json!([])).await;
    configure_openai_provider(
        &admin,
        &app.address,
        "sk-test-stub-key-3333333",
        Some(&stub_url),
    )
    .await;

    let resp = suggest(&admin, &app.address, ds_id, &["new_table"]).await;
    assert_eq!(resp.status().as_u16(), 502);
}

/// An empty or unreadable cache is not served: the live schema decides (real table 200, unknown
/// table 400).
async fn assert_unusable_cache_defers_to_live_schema(cache: serde_json::Value) {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    reset_ai_rate_limit(&app, 1, 1).await;
    let ds_id = create_ds_real(&admin, &app.address).await;

    let ds = arcodash_core::entities::data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: arcodash_core::entities::data_source::ActiveModel = ds.into();
    active.schema = Set(Some(cache));
    active.update(&app.db).await.unwrap();

    let stub_url = spawn_stub_openai(serde_json::json!([])).await;
    configure_openai_provider(
        &admin,
        &app.address,
        "sk-test-stub-key-4444444",
        Some(&stub_url),
    )
    .await;

    let known = suggest_real(&admin, &app.address, ds_id, &["orders"]).await;
    assert_eq!(known.status().as_u16(), 200);
    let body: serde_json::Value = known.json().await.unwrap();
    assert_eq!(body["status"], "ok", "body: {body}");

    let unknown = suggest_real(&admin, &app.address, ds_id, &["no_such_table"]).await;
    assert_eq!(unknown.status().as_u16(), 400);
    let body = unknown.text().await.unwrap();
    assert!(
        body.contains("not found in this data source's schema"),
        "body: {body}"
    );
}

#[tokio::test]
async fn empty_cache_defers_to_live_schema() {
    assert_unusable_cache_defers_to_live_schema(serde_json::json!({ "tables": [] })).await;
}

#[tokio::test]
async fn unreadable_cache_defers_to_live_schema() {
    assert_unusable_cache_defers_to_live_schema(serde_json::json!({ "tables": "nope" })).await;
}
