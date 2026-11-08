//! Integration tests: data source CRUD.

mod common;
use common::{make_org, spawn_app, test_api_key, user_with_role};

fn assert_ok(status: u16, body: &str, context: &str) {
    assert_eq!(status, 200, "{context}: {body}");
}

/// POST /api/data_sources → 200, data source with expected fields.
#[tokio::test]
async fn create_data_source_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "My Postgres",
            "type": "pg",
            "options": {
                // Public IP literal so the create-time SSRF guard passes without a DNS lookup.
                "host": "8.8.8.8",
                "port": 5433,
                "dbname": "arcodash_test",
                "user": "arcodash",
                "password": "arcodash"
            }
        }))
        .send()
        .await
        .unwrap();

    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_ok(status, &body_text, "create data source");

    let body: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    assert_eq!(body["name"], "My Postgres");
    assert_eq!(body["type"], "pg");
    assert!(!body["id"].is_null());
    // Secrets must never be echoed back.
    assert!(
        body.get("options").is_none() || body["options"].get("password").is_none(),
        "data source response must not echo password"
    );
}

/// GET /api/data_sources/types → array of connector descriptors.
#[tokio::test]
async fn list_types_returns_connector_list() {
    let app = skip_if_none!(spawn_app().await);

    let client = app.admin_client().await;

    let resp = client
        .get(format!("{}/api/data_sources/types", app.address))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body.is_array(), "types must be an array");
    let types = body.as_array().unwrap();
    assert!(!types.is_empty(), "types list must not be empty");
    // At minimum the postgres connector must be registered.
    let has_pg = types.iter().any(|t| {
        t["type"]
            .as_str()
            .map(|s| s == "pg" || s == "postgres")
            .unwrap_or(false)
    });
    assert!(has_pg, "pg connector must be in types list; got: {body}");
}

/// POST /api/data_sources/:id/test → responds without a 500 (pass or fail; no live target needed).
#[tokio::test]
async fn test_connection_endpoint_responds() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    // The local test DB is blocked by the SSRF guard by default; opt in as a self-hosted deployment
    // would.
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "test-conn-ds",
            "type": "pg",
            "options": {
                "host": "localhost",
                "port": 5433,
                "dbname": "arcodash_test",
                "user": "arcodash",
                "password": "arcodash"
            }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_ok(status, &body_text, "create ds for test_connection");
    let ds: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let resp = client
        .post(format!("{}/api/data_sources/test", app.address))
        .json(&serde_json::json!({ "id": ds_id }))
        .send()
        .await
        .unwrap();

    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);

    // Accept 200 (success), 400/422 (validation fail), or 503 (connection fail).
    let status = resp.status().as_u16();
    assert_ne!(status, 404, "test endpoint must exist");
    assert_ne!(status, 500, "test endpoint must not 500");
}

/// GET /api/data_sources → list includes the created data source.
#[tokio::test]
async fn list_data_sources_includes_created() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "list-test-ds",
            "type": "pg",
            "options": {}
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_ok(status, &body_text, "create ds for list test");
    let ds: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let resp = client
        .get(format!("{}/api/data_sources", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    let items = body["data_sources"]
        .as_array()
        .expect("response must be the { data_sources, can_create } envelope");
    let found = items.iter().any(|d| d["id"].as_i64() == Some(ds_id));
    assert!(found, "data source id={ds_id} must be in list");
    assert_eq!(
        body["can_create"].as_bool(),
        Some(true),
        "admin must always be able to create"
    );
}

/// Legacy cached `data_sources.schema` columns (bare name strings) are upgraded to `{name,
/// type_name}` objects on both read paths.
#[tokio::test]
async fn cached_schema_with_bare_column_names_is_normalized_in_responses() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "legacy-schema-ds",
            "type": "pg",
            "options": {}
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_ok(status, &body_text, "create ds for legacy schema test");
    let ds: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    use arcodash_core::entities::data_source;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    let row = data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("data source row must exist");
    let mut active: data_source::ActiveModel = row.into();
    active.schema = Set(Some(serde_json::json!({
        "tables": [{"name": "t", "columns": ["a", "b"]}]
    })));
    active.update(&app.db).await.unwrap();

    // Normalized columns are objects with `type_name` absent (not null) since it's unknown.
    let expected_columns = serde_json::json!([{"name": "a"}, {"name": "b"}]);

    let resp = client
        .get(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["schema"]["tables"][0]["columns"], expected_columns,
        "GET /data_sources/:id must normalize legacy bare-string columns: {body}"
    );

    let resp = client
        .get(format!("{}/api/data_sources/{ds_id}/schema", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["schema"]["tables"][0]["columns"], expected_columns,
        "GET /data_sources/:id/schema must normalize legacy bare-string columns: {body}"
    );
}

/// POST /api/data_sources targeting the cloud metadata IP is rejected at create time (same SSRF
/// guard and escape hatch as test/refresh/run).
#[tokio::test]
async fn create_targeting_metadata_ip_is_blocked_unless_private_connectors_allowed() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);

    let blocked_options = serde_json::json!({
        "name": "SSRF Probe - Metadata",
        "type": "pg",
        "options": {
            "host": "169.254.169.254",
            "port": 5432,
            "user": "x",
            "password": "x",
            "dbname": "x"
        }
    });

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&blocked_options)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "create targeting the metadata IP must be rejected, not silently persisted"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("blocked"),
        "rejection message must surface the guard's reason: {body}"
    );

    // Flag on: the same escape hatch `test`/`refresh_schema` respect.
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&blocked_options)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    assert_ok(status, &body_text, "create with the allow-private flag set");
}

/// POST /api/data_sources with an unrecognized `type` → 400, never silently accepted.
#[tokio::test]
async fn create_with_unknown_type_returns_400() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "bogus-type-ds",
            "type": "not-a-real-connector-type",
            "options": {}
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "an unrecognized data source type must be rejected"
    );
}

/// A type with no connector (e.g. `oracle`) → 400; real types succeed (two connectors catch an
/// inverted allow/deny list).
#[tokio::test]
async fn create_with_unsupported_type_returns_400_but_real_types_still_succeed() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "oracle-ds",
            "type": "oracle",
            "options": {}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "an unsupported connector type must be rejected at create"
    );

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "real-pg-ds",
            "type": "pg",
            "options": {
                "host": "8.8.8.8",
                "port": 5433,
                "dbname": "arcodash_test",
                "user": "arcodash",
                "password": "arcodash"
            }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_ok(status, &body_text, "create with real type 'pg'");

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "real-clickhouse-ds",
            "type": "clickhouse",
            "options": {
                "host": "8.8.8.8",
                "port": 8123,
                "db": "default"
            }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_ok(status, &body_text, "create with real type 'clickhouse'");
}

/// A junk extra option key is stripped to the connector's declared schema before storage.
#[tokio::test]
async fn create_strips_a_junk_option_key_not_in_the_schema() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "junk-key-ds",
            "type": "pg",
            "options": {
                "host": "8.8.8.8",
                "user": "arcodash",
                "dbname": "arcodash_test",
                "not_a_real_pg_option": "should be dropped"
            }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_ok(status, &body_text, "create ds with a junk option key");
    let ds: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let get_resp = client
        .get(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(get_resp.status().as_u16(), 200);
    let got: serde_json::Value = get_resp.json().await.unwrap();
    assert!(
        got["options"].get("not_a_real_pg_option").is_none(),
        "a key outside the connector's schema must never be stored/echoed back: {:?}",
        got["options"]
    );
    assert_eq!(got["options"]["host"], "8.8.8.8");
}

/// A blank-string `password` on update leaves the stored secret unchanged (verified by connecting
/// with the stored credentials).
#[tokio::test]
async fn update_with_blank_password_keeps_the_stored_secret() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let Ok(target_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("skipping: TEST_DATABASE_URL not set");
        return;
    };

    let client = app.admin_client().await;

    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "blank-password-keeps-secret",
            "type": "pg",
            "options": {
                "host": "127.0.0.1",
                "port": 5433,
                "dbname": db_name_from_url(&target_url),
                "user": "arcodash",
                "password": "arcodash",
                "sslmode": "disable"
            }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_ok(status, &body_text, "create data source");
    let created: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let ds_id = created["id"].as_i64().unwrap();

    // Blank password means "unchanged", not "clear".
    let resp = client
        .post(format!("{}/api/data_sources/{ds_id}", app.address))
        .json(&serde_json::json!({
            "options": {
                "host": "127.0.0.1",
                "port": 5433,
                "dbname": db_name_from_url(&target_url),
                "user": "arcodash",
                "password": "",
                "sslmode": "disable"
            }
        }))
        .send()
        .await
        .unwrap();
    assert_ok(
        resp.status().as_u16(),
        &resp.text().await.unwrap_or_default(),
        "update data source with blank password",
    );

    // Connect with the stored credentials; a wiped password would fail auth.
    let resp = client
        .post(format!("{}/api/data_sources/test", app.address))
        .json(&serde_json::json!({ "id": ds_id }))
        .send()
        .await
        .unwrap();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    assert_ok(
        resp.status().as_u16(),
        &resp.text().await.unwrap_or_default(),
        "test connection after blank-password update — stored password must still be valid",
    );
}

/// DELETE /api/data_sources/:id must 409 (not 500) while a query still references it, then
/// succeed once that query is gone.
#[tokio::test]
async fn delete_data_source_with_referencing_query_returns_409() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds: serde_json::Value = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "Referenced DS", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let query: serde_json::Value = client
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "Blocking Query",
            "query": "SELECT 1",
            "options": {}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let query_id = query["id"].as_i64().unwrap();

    let blocked = client
        .delete(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        blocked.status().as_u16(),
        409,
        "deleting a data source with a referencing query must 409, not 500"
    );

    let del_query = client
        .delete(format!("{}/api/queries/{query_id}", app.address))
        .send()
        .await
        .unwrap();
    assert!(
        del_query.status().is_success(),
        "delete referencing query must succeed: {}",
        del_query.status()
    );

    let now_ok = client
        .delete(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap();
    assert!(
        now_ok.status().is_success(),
        "deleting the data source must succeed once no queries reference it: {}",
        now_ok.status()
    );
}

/// A malformed pause body returns the API's `{"message": ...}` envelope, not axum's bare text/plain
/// rejection.
#[tokio::test]
async fn pause_with_malformed_body_returns_json_error_envelope() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let ds: serde_json::Value = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "malformed-pause-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let resp = client
        .post(format!("{}/api/data_sources/{ds_id}/pause", app.address))
        .header("Content-Type", "application/json")
        .body("not valid json")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 400, "malformed body must 400");
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("application/json"),
        "response must be JSON, got content-type: {content_type}"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.get("message").is_some(),
        "response body must have a message field: {body}"
    );
}

/// A non-numeric id returns the API's `{"message": ...}` envelope, not axum's bare path rejection.
#[tokio::test]
async fn get_with_non_numeric_id_returns_json_error_envelope() {
    let app = skip_if_none!(spawn_app().await);

    let client = app.admin_client().await;

    let resp = client
        .get(format!("{}/api/data_sources/abc", app.address))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "non-numeric path param must 400"
    );
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("application/json"),
        "response must be JSON, got content-type: {content_type}"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.get("message").is_some(),
        "response body must have a message field: {body}"
    );
}

// ── Cross-org upload_id via the real HTTP handler ───────────────────────
//
// The OSS harness has no multi-tenant routing, so `AppState.org_resolver` here reads a test-only
// header to serve two orgs from one app. `Authorization: Key` auth is CSRF-exempt, so no cookie
// dance is needed.
struct HeaderOrgResolver;

impl arcodash_backend::auth::OrgResolver for HeaderOrgResolver {
    fn resolve<'a>(
        &'a self,
        db: &'a sea_orm::DatabaseConnection,
        parts: &'a axum::http::request::Parts,
    ) -> arcodash_backend::auth::BoxFuture<
        'a,
        Result<arcodash_core::entities::organization::Model, arcodash_backend::error::ApiError>,
    > {
        Box::pin(async move {
            let org_id: i64 = parts
                .headers
                .get("x-test-org-id")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| {
                    arcodash_backend::error::ApiError::bad_request(
                        "missing X-Test-Org-Id test header",
                    )
                })?;
            use sea_orm::EntityTrait;
            arcodash_core::entities::organization::Entity::find_by_id(org_id)
                .one(db)
                .await
                .map_err(arcodash_backend::error::ApiError::from)?
                .ok_or_else(|| arcodash_backend::error::ApiError::not_found("org not found"))
        })
    }
}

/// A `reqwest::Client` authenticated as an API-key user, sending the test-only `X-Test-Org-Id`
/// header `HeaderOrgResolver` reads.
fn api_key_client(key: &str, org_id: i64) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "Authorization",
        reqwest::header::HeaderValue::from_str(&format!("Key {key}")).unwrap(),
    );
    headers.insert(
        "x-test-org-id",
        reqwest::header::HeaderValue::from_str(&org_id.to_string()).unwrap(),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

/// Uploads `bytes` as a CSV via the real multipart endpoint, returns the `upload_id`.
async fn upload_csv(client: &reqwest::Client, address: &str, bytes: &'static [u8]) -> String {
    let form = reqwest::multipart::Form::new().text("type", "csv").part(
        "file",
        reqwest::multipart::Part::bytes(bytes)
            .file_name("data.csv")
            .mime_str("text/csv")
            .unwrap(),
    );
    let resp = client
        .post(format!("{address}/api/data_sources/uploads"))
        .multipart(form)
        .send()
        .await
        .expect("upload request failed");
    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    assert_eq!(status.as_u16(), 200, "upload failed: {body_text}");
    let body: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    body["upload_id"].as_str().unwrap().to_string()
}

const CSV_A: &[u8] = b"name,age\nalice,30\n";
const CSV_B: &[u8] = b"name,age\nbob,40\n";

#[tokio::test]
async fn cross_org_upload_id_on_create_is_rejected_via_the_http_handler() {
    let app = skip_if_none!(
        common::spawn_app_with_org_resolver(std::sync::Arc::new(HeaderOrgResolver)).await
    );
    app.cleanup().await;

    let org_a = make_org(&app.db).await;
    let (user_a, _) = user_with_role(
        &app.db,
        org_a,
        &["create_data_source", "edit_data_source", "view_data_source"],
    )
    .await;
    let client_a = api_key_client(&test_api_key(&user_a.email), org_a);

    let org_b = make_org(&app.db).await;
    let (user_b, _) = user_with_role(
        &app.db,
        org_b,
        &["create_data_source", "edit_data_source", "view_data_source"],
    )
    .await;
    let client_b = api_key_client(&test_api_key(&user_b.email), org_b);

    let upload_id_a = upload_csv(&client_a, &app.address, CSV_A).await;

    // Org B attempts to create a data source referencing org A's upload.
    let resp = client_b
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "cross-org-create-attempt",
            "type": "csv",
            "options": { "upload_id": upload_id_a, "delimiter": "," }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "creating a data source with another org's upload_id must be forbidden"
    );

    use arcodash_core::entities::data_source;
    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
    let count = data_source::Entity::find()
        .filter(data_source::Column::OrgId.eq(org_b))
        .count(&app.db)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "a rejected cross-org upload_id must leave no data source row behind in org B"
    );
}

#[tokio::test]
async fn cross_org_upload_id_on_update_is_rejected_via_the_http_handler() {
    let app = skip_if_none!(
        common::spawn_app_with_org_resolver(std::sync::Arc::new(HeaderOrgResolver)).await
    );
    app.cleanup().await;

    let org_a = make_org(&app.db).await;
    let (user_a, _) = user_with_role(
        &app.db,
        org_a,
        &["create_data_source", "edit_data_source", "view_data_source"],
    )
    .await;
    let client_a = api_key_client(&test_api_key(&user_a.email), org_a);

    let org_b = make_org(&app.db).await;
    let (user_b, _) = user_with_role(
        &app.db,
        org_b,
        &["create_data_source", "edit_data_source", "view_data_source"],
    )
    .await;
    let client_b = api_key_client(&test_api_key(&user_b.email), org_b);

    let upload_id_a = upload_csv(&client_a, &app.address, CSV_A).await;

    // Org B legitimately creates its own csv data source with its own upload first.
    let upload_id_b = upload_csv(&client_b, &app.address, CSV_B).await;
    let resp = client_b
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "cross-org-update-target",
            "type": "csv",
            "options": { "upload_id": upload_id_b, "delimiter": "," }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "org B's own create must succeed"
    );
    let ds_b: serde_json::Value = resp.json().await.unwrap();
    let ds_b_id = ds_b["id"].as_i64().unwrap();

    // Org B attempts to swap in org A's upload_id via update.
    let resp = client_b
        .post(format!("{}/api/data_sources/{ds_b_id}", app.address))
        .json(&serde_json::json!({
            "options": { "upload_id": upload_id_a, "delimiter": "," }
        }))
        .send()
        .await
        .unwrap();
    let update_status = resp.status().as_u16();
    let update_body = resp.text().await.unwrap_or_default();
    assert_eq!(
        update_status, 403,
        "updating a data source to reference another org's upload_id must be forbidden: {update_body}"
    );

    // The row's persisted options must be untouched — still pointing at org B's own upload.
    let resp = client_b
        .get(format!("{}/api/data_sources/{ds_b_id}", app.address))
        .send()
        .await
        .unwrap();
    let get_status = resp.status().as_u16();
    let get_body_text = resp.text().await.unwrap_or_default();
    assert_eq!(get_status, 200, "GET must succeed: {get_body_text}");
    let reloaded: serde_json::Value = serde_json::from_str(&get_body_text).unwrap();
    assert_eq!(
        reloaded["options"]["upload_id"], upload_id_b,
        "a rejected cross-org update must leave the row's persisted upload_id unchanged"
    );
}

/// Creating a data source whose connector can't connect marks health Failing immediately.
#[tokio::test]
async fn create_with_unreachable_connection_marks_health_failing() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "health-failing-on-create",
            "type": "pg",
            "options": {
                // A public IP with nothing listening: a fast, deterministic "can't connect".
                "host": "8.8.8.8",
                "port": 5433,
                "dbname": "nope",
                "user": "nope",
                "password": "nope"
            }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_ok(status, &body_text, "create data source");
    let created: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let ds_id = created["id"].as_i64().unwrap();

    // Assert against a fresh GET: health is written by a save-time probe after the create response.
    let reloaded: serde_json::Value = client
        .get(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        reloaded["health_state"], "failing",
        "a create-time connectivity probe failure must mark health Failing immediately: {reloaded}"
    );
    assert!(
        reloaded["health_error"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "health_error must carry the connection failure message: {reloaded}"
    );
    assert!(
        !reloaded["health_checked_at"].is_null(),
        "health_checked_at must be set: {reloaded}"
    );
}

/// A successful save-time probe marks health OK immediately (uses the shared test Postgres, with
/// the private-address opt-in).
#[tokio::test]
async fn create_with_reachable_connection_marks_health_ok() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let Ok(target_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("skipping: TEST_DATABASE_URL not set");
        return;
    };

    let client = app.admin_client().await;
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "health-unknown-on-create",
            "type": "pg",
            "options": {
                "host": "127.0.0.1",
                "port": 5433,
                "dbname": db_name_from_url(&target_url),
                "user": "arcodash",
                "password": "arcodash",
                "sslmode": "disable"
            }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    assert_ok(status, &body_text, "create data source");
    let created: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let ds_id = created["id"].as_i64().unwrap();

    // Fresh GET: `create()` returns the model captured before the save-time probe's UPDATE.
    let reloaded: serde_json::Value = client
        .get(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(
        reloaded["health_state"], "ok",
        "a successful create-time probe must record health immediately: {reloaded}"
    );
    assert!(reloaded["health_error"].is_null());
    assert!(!reloaded["health_checked_at"].is_null());
}

/// A save-time probe success clears a stale `failing` state and lands on `ok`, not `unknown`.
#[tokio::test]
async fn update_with_now_reachable_connection_clears_a_stale_failing_health_state() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let Ok(target_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("skipping: TEST_DATABASE_URL not set");
        return;
    };

    let client = app.admin_client().await;

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "health-reset-on-update",
            "type": "pg",
            "options": {
                "host": "8.8.8.8",
                "port": 5433,
                "dbname": "nope",
                "user": "nope",
                "password": "nope"
            }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_ok(status, &body_text, "create data source");
    let created: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let ds_id = created["id"].as_i64().unwrap();

    // Fresh GET: the create response is always the pre-probe `unknown`.
    let after_create: serde_json::Value = client
        .get(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        after_create["health_state"], "failing",
        "sanity check: {after_create}"
    );

    // Update with options that connect (the shared test Postgres, with the private-address opt-in).
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let resp = client
        .post(format!("{}/api/data_sources/{ds_id}", app.address))
        .json(&serde_json::json!({
            "options": {
                "host": "127.0.0.1",
                "port": 5433,
                "dbname": db_name_from_url(&target_url),
                "user": "arcodash",
                "password": "arcodash",
                "sslmode": "disable"
            }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    assert_ok(status, &body_text, "update data source");

    let reloaded: serde_json::Value = client
        .get(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        reloaded["health_state"], "ok",
        "a now-successful probe must clear the stale `failing` state and record health: {reloaded}"
    );
    assert!(reloaded["health_error"].is_null());
}

/// The standalone "Test connection" endpoint also records a successful test as health OK.
#[tokio::test]
async fn standalone_test_connection_marks_health_ok() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let Ok(target_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("skipping: TEST_DATABASE_URL not set");
        return;
    };

    let client = app.admin_client().await;

    // Start `failing` so only the standalone test can flip it.
    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "health-ok-on-standalone-test",
            "type": "pg",
            "options": {
                "host": "8.8.8.8",
                "port": 5433,
                "dbname": "nope",
                "user": "nope",
                "password": "nope"
            }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_ok(status, &body_text, "create data source");
    let created: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let ds_id = created["id"].as_i64().unwrap();

    // Fix the connection via update, then hit the standalone test endpoint.
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
    let resp = client
        .post(format!("{}/api/data_sources/{ds_id}", app.address))
        .json(&serde_json::json!({
            "options": {
                "host": "127.0.0.1",
                "port": 5433,
                "dbname": db_name_from_url(&target_url),
                "user": "arcodash",
                "password": "arcodash",
                "sslmode": "disable"
            }
        }))
        .send()
        .await
        .unwrap();
    assert_ok(
        resp.status().as_u16(),
        &resp.text().await.unwrap_or_default(),
        "update data source",
    );

    // Force `failing` directly; update()'s own probe already set it to `ok`.
    use arcodash_core::entities::data_source;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    let mut active: data_source::ActiveModel = data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .into();
    active.health_state = Set("failing".to_string());
    active.health_error = Set(Some("stale".to_string()));
    active.update(&app.db).await.unwrap();

    let resp = client
        .post(format!("{}/api/data_sources/test", app.address))
        .json(&serde_json::json!({ "id": ds_id }))
        .send()
        .await
        .unwrap();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    assert_ok(
        resp.status().as_u16(),
        &resp.text().await.unwrap_or_default(),
        "standalone test connection",
    );

    let reloaded: serde_json::Value = client
        .get(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        reloaded["health_state"], "ok",
        "an explicit successful Test connection must record health OK: {reloaded}"
    );
    assert!(reloaded["health_error"].is_null());
    assert!(!reloaded["health_checked_at"].is_null());
}

/// Failure-mode mirror: the standalone test marks health `failing` after health was forced to `ok`.
#[tokio::test]
async fn standalone_test_connection_failure_marks_health_failing() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    // Use the same unreachable target the standalone test will hit.
    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "health-failing-on-standalone-test",
            "type": "pg",
            "options": {
                "host": "8.8.8.8",
                "port": 5433,
                "dbname": "nope",
                "user": "nope",
                "password": "nope"
            }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_ok(status, &body_text, "create data source");
    let created: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let ds_id = created["id"].as_i64().unwrap();

    // Force `ok` directly so only the standalone test can flip it back.
    use arcodash_core::entities::data_source;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    let mut active: data_source::ActiveModel = data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .into();
    active.health_state = Set("ok".to_string());
    active.health_error = Set(None);
    active.update(&app.db).await.unwrap();

    let resp = client
        .post(format!("{}/api/data_sources/test", app.address))
        .json(&serde_json::json!({ "id": ds_id }))
        .send()
        .await
        .unwrap();
    // The connection genuinely fails, so the endpoint itself must report an error — not 200.
    assert_ne!(
        resp.status().as_u16(),
        200,
        "a failed standalone test must not report success"
    );

    let reloaded: serde_json::Value = client
        .get(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        reloaded["health_state"], "failing",
        "a failed standalone Test connection must record health Failing, not leave it stale: {reloaded}"
    );
    assert!(
        reloaded["health_error"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "health_error must carry the connection failure message: {reloaded}"
    );
    assert!(
        !reloaded["health_checked_at"].is_null(),
        "health_checked_at must be updated: {reloaded}"
    );
}

/// Live check against the demo Postgres on :5435: a successful Test connection flips health to
/// `ok`. Skips if the container isn't running.
#[tokio::test]
async fn standalone_test_connection_against_demo_postgres_marks_health_ok() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    if std::net::TcpStream::connect_timeout(
        &"127.0.0.1:5435".parse().unwrap(),
        std::time::Duration::from_millis(500),
    )
    .is_err()
    {
        eprintln!("skipping: demo Postgres not reachable on :5435");
        return;
    }

    let client = app.admin_client().await;
    std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({
            "name": "demo-postgres-real-dependency-check",
            "type": "pg",
            "options": {
                "host": "127.0.0.1",
                "port": 5435,
                "dbname": "demo",
                "user": "demo",
                "password": "demo1234",
                "sslmode": "disable"
            }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_ok(
        status,
        &body_text,
        "create data source against demo postgres",
    );
    let created: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let ds_id = created["id"].as_i64().unwrap();

    // Also call the standalone Test connection against the same real target.
    let resp = client
        .post(format!("{}/api/data_sources/test", app.address))
        .json(&serde_json::json!({ "id": ds_id }))
        .send()
        .await
        .unwrap();
    std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_ok(
        status,
        &body_text,
        "standalone test connection against demo postgres",
    );

    let reloaded: serde_json::Value = client
        .get(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        reloaded["health_state"], "ok",
        "a real connection to the demo Postgres container must flip health to ok: {reloaded}"
    );
    assert!(!reloaded["health_checked_at"].is_null());
}

/// GET /api/data_sources/:id/groups: an unrestricted group shows `has_access: true`; a restricted
/// one shows `false` until assigned via `POST /groups/:id/data_sources`.
#[tokio::test]
async fn list_groups_for_data_source_reflects_restriction_and_assignment() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let ds_resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "reverse-groups-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    let ds_status = ds_resp.status().as_u16();
    let ds_text = ds_resp.text().await.unwrap_or_default();
    assert_ok(ds_status, &ds_text, "create data source");
    let ds: serde_json::Value = serde_json::from_str(&ds_text).unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let unrestricted_group: serde_json::Value = client
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": "reverse-groups-unrestricted" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let unrestricted_id = unrestricted_group["id"].as_i64().unwrap();
    // New groups default to restricted; make this one unrestricted.
    let unrestrict_resp = client
        .put(format!(
            "{}/api/groups/{unrestricted_id}/data_source_access",
            app.address
        ))
        .json(&serde_json::json!({ "restricted": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(unrestrict_resp.status().as_u16(), 200);

    let restricted_group: serde_json::Value = client
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": "reverse-groups-restricted" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let restricted_id = restricted_group["id"].as_i64().unwrap();
    let set_restricted_resp = client
        .put(format!(
            "{}/api/groups/{restricted_id}/data_source_access",
            app.address
        ))
        .json(&serde_json::json!({ "restricted": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(set_restricted_resp.status().as_u16(), 200);

    let list_before_resp = client
        .get(format!("{}/api/data_sources/{ds_id}/groups", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(list_before_resp.status().as_u16(), 200);
    let list_before: serde_json::Value = list_before_resp.json().await.unwrap();
    let rows_before = list_before.as_array().expect("must be a JSON array");
    let unrestricted_row = rows_before
        .iter()
        .find(|r| r["group_id"].as_i64() == Some(unrestricted_id))
        .unwrap_or_else(|| panic!("unrestricted group missing from {list_before}"));
    assert_eq!(unrestricted_row["restricted"], false);
    assert_eq!(
        unrestricted_row["has_access"], true,
        "an unrestricted group reaches every data source with no join row"
    );
    let restricted_row_before = rows_before
        .iter()
        .find(|r| r["group_id"].as_i64() == Some(restricted_id))
        .unwrap_or_else(|| panic!("restricted group missing from {list_before}"));
    assert_eq!(restricted_row_before["restricted"], true);
    assert_eq!(
        restricted_row_before["has_access"], false,
        "a restricted group with no assignment must not show access: {list_before}"
    );

    let assign_resp = client
        .post(format!(
            "{}/api/groups/{restricted_id}/data_sources",
            app.address
        ))
        .json(&serde_json::json!({ "data_source_id": ds_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(assign_resp.status().as_u16(), 200);

    let list_after_resp = client
        .get(format!("{}/api/data_sources/{ds_id}/groups", app.address))
        .send()
        .await
        .unwrap();
    let list_after: serde_json::Value = list_after_resp.json().await.unwrap();
    let restricted_row_after = list_after
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["group_id"].as_i64() == Some(restricted_id))
        .unwrap_or_else(|| panic!("restricted group missing from {list_after}"));
    assert_eq!(
        restricted_row_after["has_access"], true,
        "assigning through the group-keyed endpoint must flip has_access: {list_after}"
    );
}

/// GET /api/data_sources/:id/groups without `view_groups` must 403, not leak group data.
#[tokio::test]
async fn list_groups_for_data_source_without_permission_is_forbidden() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let ds_resp = admin
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "groups-403-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    let ds_text = ds_resp.text().await.unwrap_or_default();
    let ds: serde_json::Value = serde_json::from_str(&ds_text).unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    use arcodash_core::entities::organization;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist");

    // Holds view_data_source (so it can reach the data source itself) but not view_groups.
    let (user, _) = user_with_role(&app.db, org.id, &["view_data_source"]).await;
    let client = api_key_client(&test_api_key(&user.email), org.id);

    let resp = client
        .get(format!("{}/api/data_sources/{ds_id}/groups", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "a caller without view_groups must be forbidden from reading group access"
    );
}

/// Another org's data source → 404, no cross-org group membership leak.
#[tokio::test]
async fn list_groups_for_data_source_cross_org_is_not_found() {
    let app = skip_if_none!(
        common::spawn_app_with_org_resolver(std::sync::Arc::new(HeaderOrgResolver)).await
    );
    app.cleanup().await;

    let org_a = make_org(&app.db).await;
    let (user_a, _) = user_with_role(
        &app.db,
        org_a,
        &["create_data_source", "edit_data_source", "view_data_source"],
    )
    .await;
    let client_a = api_key_client(&test_api_key(&user_a.email), org_a);

    let ds_resp = client_a
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "groups-404-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    let ds_status = ds_resp.status().as_u16();
    let ds_text = ds_resp.text().await.unwrap_or_default();
    assert_ok(ds_status, &ds_text, "create data source in org A");
    let ds: serde_json::Value = serde_json::from_str(&ds_text).unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let org_b = make_org(&app.db).await;
    let (user_b, _) = user_with_role(&app.db, org_b, &["view_data_source", "view_groups"]).await;
    let client_b = api_key_client(&test_api_key(&user_b.email), org_b);

    let resp = client_b
        .get(format!("{}/api/data_sources/{ds_id}/groups", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "a data source belonging to another org must not be found"
    );
}

/// `postgres://user:pass@host:port/dbname` -> `dbname` (manual parse, no `url` crate).
fn db_name_from_url(url: &str) -> String {
    url.rsplit_once('/')
        .map(|(_, db)| db)
        .unwrap_or("")
        .to_string()
}

// ── pause refuses executions ──

/// A paused data source 409s (with the pause reason) on a cache-miss `results` execute and a manual
/// `run`; cache hits are unaffected and resuming restores both.
#[tokio::test]
async fn paused_data_source_refuses_new_executions_but_not_cached_reads() {
    use arcodash_core::entities::{query_result, run};
    use arcodash_core::hash::gen_query_hash;
    use sea_orm::{
        ActiveModelTrait, ActiveValue::NotSet, ActiveValue::Set, ColumnTrait, EntityTrait,
        PaginatorTrait, QueryFilter,
    };

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds: serde_json::Value = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "pause-refuses-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let q: serde_json::Value = client
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "Pause Refuses Query",
            "query": "SELECT 1",
            "options": {}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let query_id = q["id"].as_i64().unwrap();
    let org_id = arcodash_core::entities::organization::Entity::find()
        .filter(arcodash_core::entities::organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist")
        .id;

    // Seed a cached result under the key `execute` computes, so the cache-hit path needs no real
    // connector.
    let query_hash = gen_query_hash("SELECT 1", &serde_json::Value::Null);
    query_result::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        data_source_id: Set(ds_id),
        query_hash: Set(query_hash),
        query: Set("SELECT 1".to_string()),
        data: Set(Some(serde_json::json!({"columns": [], "rows": []}))),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(None),
        retrieved_at: Set(chrono::Utc::now().into()),
        created_at: NotSet,
        updated_at: NotSet,
    }
    .insert(&app.db)
    .await
    .unwrap();

    let pause_resp = client
        .post(format!("{}/api/data_sources/{ds_id}/pause", app.address))
        .json(&serde_json::json!({ "reason": "maintenance window" }))
        .send()
        .await
        .unwrap();
    assert_eq!(pause_resp.status().as_u16(), 200, "pause must succeed");

    // Cache-miss execute (max_age: 0 forces past any cache) must 409 with the reason.
    let miss_resp = client
        .post(format!("{}/api/queries/{query_id}/results", app.address))
        .json(&serde_json::json!({ "parameters": {}, "max_age": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        miss_resp.status().as_u16(),
        409,
        "cache-miss execute against a paused source must 409"
    );
    let miss_body = miss_resp.text().await.unwrap_or_default();
    assert!(
        miss_body.contains("maintenance window"),
        "409 body must carry the pause reason: {miss_body}"
    );

    // A manual run must also 409, and must never strand a pending `runs` row.
    let run_resp = client
        .post(format!("{}/api/queries/{query_id}/run", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        run_resp.status().as_u16(),
        409,
        "run against a paused source must 409"
    );
    let pending_runs = run::Entity::find()
        .filter(run::Column::EntityType.eq("query"))
        .filter(run::Column::EntityId.eq(query_id))
        .filter(run::Column::Status.eq("pending"))
        .count(&app.db)
        .await
        .unwrap();
    assert_eq!(
        pending_runs, 0,
        "a refused run must never leave a pending runs row behind"
    );

    // A cached read never touches the source — must still succeed while paused.
    let cached_resp = client
        .post(format!("{}/api/queries/{query_id}/results", app.address))
        .json(&serde_json::json!({ "parameters": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        cached_resp.status().as_u16(),
        200,
        "a cache hit must still succeed against a paused source: {}",
        cached_resp.text().await.unwrap_or_default()
    );

    let resume_resp = client
        .post(format!("{}/api/data_sources/{ds_id}/resume", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resume_resp.status().as_u16(), 200, "resume must succeed");

    // `max_age: 0` forces the enqueue path, proving the paused check now passes.
    let after_resume_results = client
        .post(format!("{}/api/queries/{query_id}/results", app.address))
        .json(&serde_json::json!({ "parameters": {}, "max_age": 0 }))
        .send()
        .await
        .unwrap();
    assert!(
        after_resume_results.status().is_success(),
        "execute must succeed again once resumed: {}",
        after_resume_results.status()
    );

    let after_resume_run = client
        .post(format!("{}/api/queries/{query_id}/run", app.address))
        .send()
        .await
        .unwrap();
    assert!(
        after_resume_run.status().is_success(),
        "run must succeed again once resumed: {}",
        after_resume_run.status()
    );
}

// ── pause/resume leave an audit trail ──

/// Fetches every `events` row for `action`, asserting at least one exists.
async fn events_for_action(
    client: &reqwest::Client,
    base: &str,
    action: &str,
) -> Vec<serde_json::Value> {
    let resp = client
        .get(format!("{base}/api/events?action={action}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let list: serde_json::Value = resp.json().await.unwrap();
    list.as_array()
        .unwrap_or_else(|| panic!("events response for {action} was not an array: {list}"))
        .clone()
}

/// Pause with a reason and resume each leave an `events` row.
#[tokio::test]
async fn pause_and_resume_are_audited() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds: serde_json::Value = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "audit-pause-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let pause_resp = client
        .post(format!("{}/api/data_sources/{ds_id}/pause", app.address))
        .json(&serde_json::json!({ "reason": "scheduled maintenance" }))
        .send()
        .await
        .unwrap();
    assert_eq!(pause_resp.status().as_u16(), 200);

    let pause_events = events_for_action(&client, &app.address, "data_source.paused").await;
    let pause_event = pause_events
        .iter()
        .find(|e| e["object_id"].as_i64() == Some(ds_id))
        .unwrap_or_else(|| panic!("no data_source.paused event for ds {ds_id}: {pause_events:#?}"));
    assert_eq!(
        pause_event["additional_properties"]["reason"],
        serde_json::json!("scheduled maintenance")
    );

    let resume_resp = client
        .post(format!("{}/api/data_sources/{ds_id}/resume", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resume_resp.status().as_u16(), 200);

    let resume_events = events_for_action(&client, &app.address, "data_source.resumed").await;
    assert!(
        resume_events
            .iter()
            .any(|e| e["object_id"].as_i64() == Some(ds_id)),
        "no data_source.resumed event for ds {ds_id}: {resume_events:#?}"
    );
}

/// The 500-char audit bound also applies to the stored `pause_reason` column.
#[tokio::test]
async fn pause_reason_is_truncated_before_it_reaches_the_column() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds: serde_json::Value = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "long-reason-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let long_reason = "x".repeat(2000);
    let pause_resp = client
        .post(format!("{}/api/data_sources/{ds_id}/pause", app.address))
        .json(&serde_json::json!({ "reason": long_reason }))
        .send()
        .await
        .unwrap();
    assert_eq!(pause_resp.status().as_u16(), 200);
    let pause_body: serde_json::Value = pause_resp.json().await.unwrap();
    assert_eq!(
        pause_body["pause_reason"].as_str().unwrap().chars().count(),
        500,
        "the pause response must already reflect the truncated value"
    );

    use arcodash_core::entities::data_source;
    use sea_orm::EntityTrait;
    let row = data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("data source must exist");
    assert_eq!(
        row.pause_reason.as_deref().unwrap().chars().count(),
        500,
        "the stored column must be truncated, not just the audit event"
    );
}

/// `GET /:id/ai_profile` returns `{profile:null}` before a build, the typed profile once one is
/// stored, 404 to another org — and the profile never leaks into `GET /:id`.
#[tokio::test]
async fn ai_profile_route_null_typed_cross_org_and_absent_from_get() {
    use arcodash_core::entities::data_source;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

    let app = skip_if_none!(
        common::spawn_app_with_org_resolver(std::sync::Arc::new(HeaderOrgResolver)).await
    );
    app.cleanup().await;

    let perms = ["admin"];
    let org_a = make_org(&app.db).await;
    let (user_a, _) = user_with_role(&app.db, org_a, &perms).await;
    let client_a = api_key_client(&test_api_key(&user_a.email), org_a);
    let org_b = make_org(&app.db).await;
    let (user_b, _) = user_with_role(&app.db, org_b, &perms).await;
    let client_b = api_key_client(&test_api_key(&user_b.email), org_b);

    let resp = client_a
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "profiled", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let ds_id = resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();
    let url = format!("{}/api/data_sources/{ds_id}/ai_profile", app.address);

    let body: serde_json::Value = client_a
        .get(&url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body["profile"].is_null(), "{body}");
    assert!(body["built_at"].is_null(), "{body}");
    assert_eq!(body["supported"], true, "{body}");
    assert_eq!(body["building"], false, "{body}");
    assert_eq!(body["current"], false, "{body}");

    let profile = serde_json::json!({
        "tables": [{
            "name": "orders", "sampled_rows": 20,
            "columns": [{ "name": "status", "null_frac": 0.0, "distinct_in_sample": 3,
                          "samples": ["open", "paid"], "min": null, "max": null }]
        }],
        "relationships": [{ "from_table": "orders", "from_column": "user_id",
                            "to_table": "users", "to_column": "id", "source": "fk" }],
        "schema_refreshed_at_seen": "2026-09-30T00:00:00Z"
    });
    let row = data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: data_source::ActiveModel = row.into();
    active.ai_profile = Set(Some(profile));
    active.ai_profile_built_at = Set(Some(chrono::Utc::now().into()));
    active.update(&app.db).await.unwrap();

    let body: serde_json::Value = client_a
        .get(&url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["profile"]["tables"][0]["name"], "orders", "{body}");
    assert_eq!(
        body["profile"]["relationships"][0]["source"], "fk",
        "{body}"
    );
    assert!(body["built_at"].is_string(), "{body}");

    assert_eq!(
        client_b.get(&url).send().await.unwrap().status().as_u16(),
        404
    );

    let ds: serde_json::Value = client_a
        .get(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(ds.get("ai_profile").is_none(), "{ds}");
    assert!(ds.get("ai_profile_built_at").is_none(), "{ds}");
}

/// Rebuild is admin-only and de-duplicated: 202 then 409; non-admin 403 (also on GET); foreign org
/// 404.
#[tokio::test]
async fn ai_profile_rebuild_dedupes_and_is_admin_only() {
    let app = skip_if_none!(
        common::spawn_app_with_org_resolver(std::sync::Arc::new(HeaderOrgResolver)).await
    );
    app.cleanup().await;

    let org_a = make_org(&app.db).await;
    let (admin_a, _) = user_with_role(&app.db, org_a, &["admin"]).await;
    let admin_a = api_key_client(&test_api_key(&admin_a.email), org_a);
    let (viewer, _) = user_with_role(&app.db, org_a, &["view_data_source"]).await;
    let viewer = api_key_client(&test_api_key(&viewer.email), org_a);
    let org_b = make_org(&app.db).await;
    let (admin_b, _) = user_with_role(&app.db, org_b, &["admin"]).await;
    let admin_b = api_key_client(&test_api_key(&admin_b.email), org_b);

    let resp = admin_a
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "rebuild", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let ds_id = resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();
    let url = format!("{}/api/data_sources/{ds_id}/ai_profile", app.address);
    let rebuild = format!("{url}/rebuild");

    assert_eq!(
        viewer
            .post(&rebuild)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        403
    );
    assert_eq!(
        viewer.get(&url).send().await.unwrap().status().as_u16(),
        403
    );
    assert_eq!(
        admin_b
            .post(&rebuild)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        404
    );

    let first = admin_a.post(&rebuild).send().await.unwrap();
    assert_eq!(first.status().as_u16(), 202);
    assert_eq!(
        first.json::<serde_json::Value>().await.unwrap()["status"],
        "queued"
    );
    let second = admin_a.post(&rebuild).send().await.unwrap();
    assert_eq!(second.status().as_u16(), 409);
    assert_eq!(
        second.json::<serde_json::Value>().await.unwrap()["status"],
        "already_queued"
    );

    let key = arcodash_core::models::ai_profile_pending_key(ds_id);
    let mut conn = app.redis.clone();
    let _: () = redis::AsyncCommands::del(&mut conn, key).await.unwrap();
}

/// `building` follows the Redis pending key; `current` is true only while the profile's
/// `schema_refreshed_at_seen` equals the source's cached schema timestamp; unsampleable types are
/// `supported: false`.
#[tokio::test]
async fn ai_profile_flags_building_current_supported() {
    use arcodash_core::entities::data_source;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

    let app = skip_if_none!(
        common::spawn_app_with_org_resolver(std::sync::Arc::new(HeaderOrgResolver)).await
    );
    app.cleanup().await;

    let org = make_org(&app.db).await;
    let (admin, _) = user_with_role(&app.db, org, &["admin"]).await;
    let admin = api_key_client(&test_api_key(&admin.email), org);

    let create = |name: &'static str, ty: &'static str| {
        let admin = admin.clone();
        let addr = app.address.clone();
        async move {
            let resp = admin
                .post(format!("{addr}/api/data_sources"))
                .json(&serde_json::json!({ "name": name, "type": ty, "options": {} }))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status().as_u16(), 200);
            resp.json::<serde_json::Value>().await.unwrap()["id"]
                .as_i64()
                .unwrap()
        }
    };
    let get = |id: i64| {
        let admin = admin.clone();
        let addr = app.address.clone();
        async move {
            admin
                .get(format!("{addr}/api/data_sources/{id}/ai_profile"))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };

    let ds_id = create("flags", "pg").await;
    assert_eq!(get(ds_id).await["building"], false);
    let rebuild = format!(
        "{}/api/data_sources/{ds_id}/ai_profile/rebuild",
        app.address
    );
    assert_eq!(
        admin.post(&rebuild).send().await.unwrap().status().as_u16(),
        202
    );
    assert_eq!(get(ds_id).await["building"], true);

    let seen = "2026-09-30T00:00:00Z";
    let refreshed: chrono::DateTime<chrono::FixedOffset> = seen.parse().unwrap();
    let row = data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: data_source::ActiveModel = row.into();
    active.ai_profile = Set(Some(serde_json::json!({
        "tables": [], "relationships": [], "schema_refreshed_at_seen": seen
    })));
    active.ai_profile_built_at = Set(Some(chrono::Utc::now().into()));
    active.schema_refreshed_at = Set(Some(refreshed));
    active.update(&app.db).await.unwrap();
    assert_eq!(get(ds_id).await["current"], true);

    let row = data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: data_source::ActiveModel = row.into();
    active.schema_refreshed_at = Set(Some("2026-10-01T00:00:00Z".parse().unwrap()));
    active.update(&app.db).await.unwrap();
    let body = get(ds_id).await;
    assert_eq!(body["current"], true, "{body}");
    assert!(body["profile"].is_object(), "{body}");

    let row = data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: data_source::ActiveModel = row.into();
    active.schema = Set(Some(serde_json::json!({
        "tables": [{"name": "orders", "columns": [{"name": "id", "type": "int"}]}]
    })));
    active.update(&app.db).await.unwrap();
    let body = get(ds_id).await;
    assert_eq!(body["current"], false, "{body}");
    assert!(body["profile"].is_object(), "{body}");

    let row = data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: data_source::ActiveModel = row.into();
    active.ai_profile = Set(Some(serde_json::json!({
        "tables": [{"name": "orders", "sampled_rows": 0, "error": "timeout", "columns": []}],
        "relationships": [], "schema_refreshed_at_seen": seen
    })));
    active.update(&app.db).await.unwrap();
    let body = get(ds_id).await;
    assert_eq!(body["current"], true, "{body}");

    let key = arcodash_core::models::ai_profile_pending_key(ds_id);
    let mut conn = app.redis.clone();
    let _: () = redis::AsyncCommands::del(&mut conn, key).await.unwrap();

    let csv_id = create("flat", "csv").await;
    assert_eq!(get(csv_id).await["supported"], false);
}

/// `GET /:id/schema?refresh=true` never touches the connector: it queues a `Changed` profile job,
/// answers from the cache with `refreshing:true`, and surfaces a failed schema pass via
/// `refresh_error` (prefix stripped) once the job is gone.
#[tokio::test]
async fn schema_refresh_enqueues_job_and_reports_state() {
    use arcodash_core::entities::data_source;
    use redis::AsyncCommands;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let resp = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": "schema-enqueue-ds", "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    let ds: serde_json::Value = resp.json().await.unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let row = data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: data_source::ActiveModel = row.into();
    active.schema = Set(Some(
        serde_json::json!({ "tables": [{"name": "cached_t", "columns": []}] }),
    ));
    active.ai_profile_last_error = Set(Some("schema refresh failed: boom".to_string()));
    active.update(&app.db).await.unwrap();

    let mut redis = app.redis.clone();
    let pending_key = arcodash_core::models::ai_profile_pending_key(ds_id);
    let _: () = redis.del(&pending_key).await.unwrap();

    let url = format!("{}/api/data_sources/{ds_id}/schema", app.address);
    let body: serde_json::Value = client.get(&url).send().await.unwrap().json().await.unwrap();
    assert_eq!(
        body["refreshing"], false,
        "plain read must not refresh: {body}"
    );
    assert_eq!(body["refresh_error"], "boom", "{body}");

    let body: serde_json::Value = client
        .get(format!("{url}?refresh=true"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["refreshing"], true, "{body}");
    assert_eq!(body["schema"]["tables"][0]["name"], "cached_t", "{body}");

    let queued: Vec<String> = redis.lrange("arcodash:queue:schemas", 0, -1).await.unwrap();
    assert!(
        queued.iter().any(|j| {
            let v: serde_json::Value = serde_json::from_str(j).unwrap_or_default();
            v["type"] == "ProfileDataSource"
                && v["data_source_id"].as_i64() == Some(ds_id)
                && v["data"] == "changed"
        }),
        "a ProfileDataSource(changed) job must be queued for the data source"
    );

    // A second refresh finds the job pending in its schema phase.
    let body: serde_json::Value = client
        .get(format!("{url}?refresh=true"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["refreshing"], true, "{body}");

    let _: () = redis.set(&pending_key, "data").await.unwrap();
    let body: serde_json::Value = client.get(&url).send().await.unwrap().json().await.unwrap();
    assert_eq!(
        body["refreshing"], false,
        "data phase is not a schema refresh: {body}"
    );
    let _: () = redis.del(&pending_key).await.unwrap();
}

/// List flags `profiling` (pending key) and `profile_failed` (recorded error); `ai_profile`
/// reports `phase`, `last_error` and `metered`; rebuild queues an `all` job.
#[tokio::test]
async fn list_and_ai_profile_report_profile_status() {
    use arcodash_core::entities::data_source;
    use redis::AsyncCommands;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let mut ids = Vec::new();
    for name in ["status-pg", "status-bq"] {
        let resp = client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": name, "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap();
        let ds: serde_json::Value = resp.json().await.unwrap();
        ids.push(ds["id"].as_i64().unwrap());
    }
    let (pg_id, bq_id) = (ids[0], ids[1]);

    let row = data_source::Entity::find_by_id(bq_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: data_source::ActiveModel = row.into();
    active.r#type = Set("bigquery".to_string());
    active.ai_profile_last_error = Set(Some("sampling exploded".to_string()));
    active.update(&app.db).await.unwrap();

    let mut redis = app.redis.clone();
    let pg_key = arcodash_core::models::ai_profile_pending_key(pg_id);
    let _: () = redis.del(&pg_key).await.unwrap();

    let list = |client: reqwest::Client, addr: String| async move {
        let v: serde_json::Value = client
            .get(format!("{addr}/api/data_sources"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["data_sources"].as_array().unwrap().clone()
    };
    let find = |items: &[serde_json::Value], id: i64| {
        items.iter().find(|d| d["id"] == id).unwrap().clone()
    };
    let items = list(client.clone(), app.address.clone()).await;
    let (pg, bq) = (find(&items, pg_id), find(&items, bq_id));
    assert_eq!(pg["profiling"], false, "{pg}");
    assert_eq!(pg["profile_failed"], false, "{pg}");
    assert_eq!(bq["profile_failed"], true, "{bq}");

    let profile_url = format!("{}/api/data_sources/{pg_id}/ai_profile", app.address);
    let status = |url: String| {
        let client = client.clone();
        async move {
            client
                .get(url)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let body = status(profile_url.clone()).await;
    assert!(body["phase"].is_null(), "{body}");
    assert_eq!(body["metered"], false, "{body}");

    let _: () = redis.set(&pg_key, "schema").await.unwrap();
    let items = list(client.clone(), app.address.clone()).await;
    assert_eq!(find(&items, pg_id)["profiling"], true);
    let body = status(profile_url.clone()).await;
    assert_eq!(body["phase"], "schema", "{body}");
    assert_eq!(body["building"], true, "{body}");
    let _: () = redis.set(&pg_key, "data").await.unwrap();
    assert_eq!(status(profile_url.clone()).await["phase"], "data");
    let _: () = redis.del(&pg_key).await.unwrap();

    let body = status(format!(
        "{}/api/data_sources/{bq_id}/ai_profile",
        app.address
    ))
    .await;
    assert_eq!(body["metered"], true, "{body}");
    assert_eq!(body["last_error"], "sampling exploded", "{body}");

    let resp = client
        .post(format!("{profile_url}/rebuild"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    let queued: Vec<String> = redis.lrange("arcodash:queue:schemas", 0, -1).await.unwrap();
    assert!(
        queued.iter().any(|j| {
            let v: serde_json::Value = serde_json::from_str(j).unwrap_or_default();
            v["data_source_id"].as_i64() == Some(pg_id) && v["data"] == "all"
        }),
        "rebuild must queue a ProfileDataSource(all) job"
    );
    let _: () = redis.del(&pg_key).await.unwrap();
}
