//! Integration tests: query CRUD and execution.

mod common;
use common::ok_json;
use common::{authed_client, spawn_app};

// Helper: assert 200 and parse JSON, with a useful error on failure.

/// POST /api/queries → 200, query created with expected fields.
#[tokio::test]
async fn create_query_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({
                "name": "test-pg",
                "type": "pg",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let body = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Test Query",
                "query": "SELECT 1",
                "description": "integration test",
                "options": {},
                "is_draft": false
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;

    assert_eq!(body["name"], "Test Query");
    assert_eq!(body["query"], "SELECT 1");
    assert_eq!(body["data_source_id"], ds_id);
    assert!(!body["id"].is_null());
    assert!(body.get("api_key").is_none());
}

/// GET /api/queries → list includes the newly created query.
#[tokio::test]
async fn list_queries_includes_created_query() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({
                "name": "test-pg-list",
                "type": "pg",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create ds for list test",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let created = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Listed Query",
                "query": "SELECT 2",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query for list test",
    )
    .await;
    let created_id = created["id"].as_i64().unwrap();

    let list_resp = client
        .get(format!("{}/api/queries", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(list_resp.status().as_u16(), 200);

    let list: serde_json::Value = list_resp.json().await.unwrap();
    let results = if list.is_array() {
        list.as_array().unwrap().clone()
    } else {
        list["results"].as_array().unwrap().clone()
    };

    let found = results.iter().any(|q| q["id"].as_i64() == Some(created_id));
    assert!(found, "created query id={created_id} not in list");
}

/// POST /api/queries/:id/results → job created.
#[tokio::test]
async fn execute_query_creates_job() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "exec-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds for execute test",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Exec Test",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query for execute test",
    )
    .await;
    let query_id = q["id"].as_i64().unwrap();

    let exec_resp = client
        .post(format!("{}/api/queries/{query_id}/results", app.address))
        .json(&serde_json::json!({ "parameters": {} }))
        .send()
        .await
        .unwrap();

    let status = exec_resp.status().as_u16();
    let body_text = exec_resp.text().await.unwrap_or_default();
    assert!(
        status == 200 || status == 202,
        "execute query returned status {status}: {body_text}"
    );
    let body: serde_json::Value = serde_json::from_str(&body_text)
        .unwrap_or_else(|e| panic!("execute body not JSON ({e}): {body_text}"));
    assert!(
        body.get("job").is_some(),
        "execute response must contain a job field; got: {body}"
    );
    assert!(
        !body["job"]["id"].is_null(),
        "job must have an id; got: {body}"
    );
}

/// POST /api/queries/:id with is_archived:true archives the query; DELETE is a hard delete.
#[tokio::test]
async fn archive_query_sets_is_archived() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "archive-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds for archive test",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Archive Me",
                "query": "SELECT 3",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query for archive test",
    )
    .await;
    let query_id = q["id"].as_i64().unwrap();

    let archive_resp = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "is_archived": true }))
            .send()
            .await
            .unwrap(),
        "archive query",
    )
    .await;
    assert_eq!(
        archive_resp["is_archived"], true,
        "archive response must have is_archived=true"
    );

    let get = ok_json(
        client
            .get(format!("{}/api/queries/{query_id}", app.address))
            .send()
            .await
            .unwrap(),
        "get archived query",
    )
    .await;
    assert_eq!(
        get["is_archived"], true,
        "GET after archive must have is_archived=true"
    );
}

// ── orphaned Text-parameter lifecycle ─────────────────────────────────────────────

/// Bare client with no cookies or CSRF.
fn bare() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(false)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Invite a new user, accept the invitation, and log in; returns `(user_id, authed_client)`.
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

/// A Text parameter's definition must be pruned once its `{{ token }}` is removed from the SQL,
/// else an orphaned definition would 403-block an Editor-tier user forever. Covers: blocked while
/// live, pruned on update, executable afterwards.
#[tokio::test]
async fn update_removing_a_text_param_token_prunes_the_definition_and_unblocks_execute() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "orphan-param-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds for orphan-param test",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let text_param_options = serde_json::json!({
        "parameters": [{ "name": "t", "type": "text", "value": "hello" }]
    });
    let created = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Orphan Param Query",
                "query": "SELECT '{{ t }}'",
                "options": text_param_options
            }))
            .send()
            .await
            .unwrap(),
        "create query with a Text param",
    )
    .await;
    let query_id = created["id"].as_i64().unwrap();

    // Publish: a draft is private to its owner.
    let pub_resp = admin
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(pub_resp.status().as_u16(), 200, "publish must succeed");

    // Editor-tier user: can edit/execute queries but has neither `edit_data_source` nor
    // `manage_data_source`.
    let role_resp = admin
        .post(format!("{}/api/roles", app.address))
        .json(&serde_json::json!({
            "name": format!("orphan-param-editor-{}", uuid::Uuid::new_v4().simple()),
            "permissions": ["view_query", "create_query", "edit_query", "execute_query", "view_data_source"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(role_resp.status().as_u16(), 200);
    let role_id = role_resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();

    let grp_resp = admin
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": format!("orphan-param-group-{}", uuid::Uuid::new_v4().simple()) }))
        .send()
        .await
        .unwrap();
    assert_eq!(grp_resp.status().as_u16(), 200);
    let group_id = grp_resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();
    let role_assign_resp = admin
        .post(format!("{}/api/groups/{group_id}/roles", app.address))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(role_assign_resp.status().as_u16(), 200);
    // Make the new group unrestricted so it keeps reach on the data source (without manage-tier
    // capability).
    let unrestrict_resp = admin
        .put(format!(
            "{}/api/groups/{group_id}/data_source_access",
            app.address
        ))
        .json(&serde_json::json!({ "restricted": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(unrestrict_resp.status().as_u16(), 200);

    let (uid, editor_client) = invite_and_login(
        &app,
        &admin,
        &format!(
            "orphan-param-editor-{}@test.local",
            uuid::Uuid::new_v4().simple()
        ),
        "Orphan Param Editor",
        "P@ssw0rd!",
        vec![group_id],
    )
    .await;

    let blocked = editor_client
        .post(format!("{}/api/queries/{query_id}/results", app.address))
        .json(&serde_json::json!({ "parameters": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        blocked.status().as_u16(),
        403,
        "a live Text param must block an Editor-tier user without manage access to the data source"
    );

    // Admin drops the token from the SQL, sending the same unchanged options.
    let updated = ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 1", "options": text_param_options }))
            .send()
            .await
            .unwrap(),
        "update query dropping the {{ t }} token",
    )
    .await;
    assert!(
        updated["options"]["parameters"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the orphaned Text definition must be pruned from stored options.parameters: {:?}",
        updated["options"]
    );

    // Execute serves published content, so re-publish.
    let republish = admin
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": updated["working_version"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        republish.status().as_u16(),
        200,
        "re-publish after dropping the token must succeed"
    );

    let unblocked = editor_client
        .post(format!("{}/api/queries/{query_id}/results", app.address))
        .json(&serde_json::json!({ "parameters": {} }))
        .send()
        .await
        .unwrap();
    let status = unblocked.status().as_u16();
    let body_text = unblocked.text().await.unwrap_or_default();
    assert!(
        status == 200 || status == 202,
        "pruning the orphaned Text param must unblock execute for the same user: {status}: {body_text}"
    );

    // Cleanup: remove the user from the group so it can be deleted.
    let _ = admin
        .delete(format!(
            "{}/api/groups/{group_id}/members/{uid}",
            app.address
        ))
        .send()
        .await;
    let _ = admin
        .delete(format!("{}/api/groups/{group_id}", app.address))
        .send()
        .await;
}

/// A malformed (non-JSON) body must return the API's `{"message": ...}` envelope, not axum's bare
/// text/plain rejection.
#[tokio::test]
async fn execute_saved_with_malformed_body_returns_json_error_envelope() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "malformed-body-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds for malformed body test",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let query = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Malformed Body Query",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query for malformed body test",
    )
    .await;
    let query_id = query["id"].as_i64().unwrap();

    let resp = client
        .post(format!("{}/api/queries/{query_id}/results", app.address))
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

/// `POST /api/queries/:id/run` has no body for parameter values, so a query whose live SQL
/// references `{{ }}` must 400 before enqueueing and create no `runs` row.
#[tokio::test]
async fn run_rejects_a_parameterized_query_without_enqueueing() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "run-param-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds for run-param test",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let query = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Run Param Query",
                "query": "SELECT '{{ n }}'",
                "options": { "parameters": [{ "name": "n", "type": "text", "value": "hi" }] }
            }))
            .send()
            .await
            .unwrap(),
        "create parameterized query",
    )
    .await;
    let query_id = query["id"].as_i64().unwrap();

    let resp = client
        .post(format!("{}/api/queries/{query_id}/run", app.address))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    assert_eq!(
        status, 400,
        "a parameterized query must be rejected with a clean 400, not enqueued: {body:?}"
    );
    assert!(
        body.get("message").is_some(),
        "the rejection must carry an explanatory message: {body:?}"
    );

    use arcodash_core::entities::run;
    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
    let run_count = run::Entity::find()
        .filter(run::Column::EntityType.eq("query"))
        .filter(run::Column::EntityId.eq(query_id))
        .count(&app.db)
        .await
        .unwrap();
    assert_eq!(
        run_count, 0,
        "no runs row must be created for the rejected parameterized run"
    );
}

/// A parameter-free query must still run normally through `/run`.
#[tokio::test]
async fn run_still_succeeds_for_a_parameter_free_query() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "run-no-param-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds for run-no-param test",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let query = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Run No-Param Query",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create parameter-free query",
    )
    .await;
    let query_id = query["id"].as_i64().unwrap();

    let resp = client
        .post(format!("{}/api/queries/{query_id}/run", app.address))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_eq!(
        status, 201,
        "a parameter-free query must still enqueue normally: {status}: {body_text}"
    );
}

// ── tags ─────────────────────────────────────────────────────────────────────

async fn create_ds(client: &reqwest::Client, address: &str, name: &str) -> i64 {
    let ds = ok_json(
        client
            .post(format!("{address}/api/data_sources"))
            .json(&serde_json::json!({ "name": name, "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source for tags test",
    )
    .await;
    ds["id"].as_i64().unwrap()
}

/// POST /api/queries with `tags` → 200; response and a subsequent GET both carry them.
#[tokio::test]
async fn create_query_with_tags_persists_them() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address, "tags-create-ds").await;

    let created = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Tagged on create",
                "query": "SELECT 1",
                "tags": ["revenue", "data quality"]
            }))
            .send()
            .await
            .unwrap(),
        "create query with tags",
    )
    .await;
    let tags = created["tags"].as_array().expect("tags must be an array");
    assert!(tags.iter().any(|t| t == "revenue"));
    assert!(tags.iter().any(|t| t == "data quality"));

    let id = created["id"].as_i64().unwrap();
    let fetched = ok_json(
        client
            .get(format!("{}/api/queries/{id}", app.address))
            .send()
            .await
            .unwrap(),
        "get query after tagged create",
    )
    .await;
    let tags = fetched["tags"].as_array().expect("tags must be an array");
    assert!(tags.iter().any(|t| t == "revenue"));
    assert!(tags.iter().any(|t| t == "data quality"));
}

/// POST /api/queries/:id with `tags` replaces the whole array; omitting `tags` on a later
/// update leaves it untouched.
#[tokio::test]
async fn update_query_replaces_tags() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address, "tags-update-ds").await;

    let created = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Untagged",
                "query": "SELECT 1"
            }))
            .send()
            .await
            .unwrap(),
        "create untagged query",
    )
    .await;
    let id = created["id"].as_i64().unwrap();
    assert_eq!(created["tags"].as_array().unwrap().len(), 0);

    let updated = ok_json(
        client
            .post(format!("{}/api/queries/{id}", app.address))
            .json(&serde_json::json!({ "tags": ["a", "b"] }))
            .send()
            .await
            .unwrap(),
        "set tags a,b",
    )
    .await;
    assert_eq!(updated["tags"], serde_json::json!(["a", "b"]));

    let replaced = ok_json(
        client
            .post(format!("{}/api/queries/{id}", app.address))
            .json(&serde_json::json!({ "tags": ["b"] }))
            .send()
            .await
            .unwrap(),
        "replace with just b",
    )
    .await;
    assert_eq!(replaced["tags"], serde_json::json!(["b"]));

    let untouched = ok_json(
        client
            .post(format!("{}/api/queries/{id}", app.address))
            .json(&serde_json::json!({ "name": "Renamed, tags omitted" }))
            .send()
            .await
            .unwrap(),
        "update without tags field",
    )
    .await;
    assert_eq!(untouched["tags"], serde_json::json!(["b"]));
}

/// `?tags=` filters server-side with "contains ALL requested tags" semantics; untagged queries
/// never match.
#[tokio::test]
async fn list_queries_tags_filter_requires_every_requested_tag() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address, "tags-filter-ds").await;

    async fn create(client: &reqwest::Client, base: &str, ds_id: i64, name: &str, tags: &[&str]) {
        ok_json(
            client
                .post(format!("{base}/api/queries"))
                .json(&serde_json::json!({
                    "data_source_id": ds_id,
                    "name": name,
                    "query": "SELECT 1",
                    "tags": tags,
                }))
                .send()
                .await
                .unwrap(),
            "create query for tags filter test",
        )
        .await;
    }

    create(&client, &app.address, ds_id, "Exact Match", &["a", "b"]).await;
    create(
        &client,
        &app.address,
        ds_id,
        "Superset Match",
        &["a", "b", "c"],
    )
    .await;
    create(&client, &app.address, ds_id, "Partial Only A", &["a"]).await;
    create(&client, &app.address, ds_id, "Untagged", &[]).await;

    let resp = ok_json(
        client
            .get(format!("{}/api/queries?tags=a,b", app.address))
            .send()
            .await
            .unwrap(),
        "list queries filtered by tags=a,b",
    )
    .await;
    let results = if resp.is_array() {
        resp.as_array().unwrap().clone()
    } else {
        resp["results"].as_array().unwrap().clone()
    };
    let names: Vec<&str> = results
        .iter()
        .map(|q| q["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Exact Match"), "got: {names:?}");
    assert!(names.contains(&"Superset Match"), "got: {names:?}");
    assert!(
        !names.contains(&"Partial Only A"),
        "a query missing a requested tag must not match: {names:?}"
    );
    assert!(
        !names.contains(&"Untagged"),
        "an untagged query must not match a non-empty tag filter: {names:?}"
    );
}

/// GET /api/queries/tags → distinct tags across the org's non-archived queries.
#[tokio::test]
async fn queries_tags_endpoint_returns_org_tags() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address, "tags-endpoint-ds").await;

    ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Tagged Query",
                "query": "SELECT 1",
                "tags": ["science", "reports"]
            }))
            .send()
            .await
            .unwrap(),
        "create tagged query for tags endpoint test",
    )
    .await;

    let resp = ok_json(
        client
            .get(format!("{}/api/queries/tags", app.address))
            .send()
            .await
            .unwrap(),
        "get query tags",
    )
    .await;
    let tags = resp["tags"].as_array().expect("tags must be an array");
    assert!(
        tags.iter().any(|t| t.as_str() == Some("science")),
        "tags must include 'science'; got: {resp}"
    );
    assert!(
        tags.iter().any(|t| t.as_str() == Some("reports")),
        "tags must include 'reports'; got: {resp}"
    );
}

/// Tag validation: >25 tags is rejected, an over-50-char tag is rejected, and
/// trim+dedupe+drop-empty happens in one write.
#[tokio::test]
async fn create_query_rejects_oversized_tags() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address, "tags-oversized-ds").await;

    let too_many: Vec<String> = (0..26).map(|i| format!("t{i}")).collect();
    let resp = client
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "Too Many Tags",
            "query": "SELECT 1",
            "tags": too_many
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400, "26 tags must be rejected");

    let long_tag = "a".repeat(51);
    let resp2 = client
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "Too Long Tag",
            "query": "SELECT 1",
            "tags": [long_tag]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp2.status().as_u16(),
        400,
        "a 51-char tag must be rejected"
    );

    let created = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Trimmed Deduped Tags",
                "query": "SELECT 1",
                "tags": ["  x  ", "x", ""]
            }))
            .send()
            .await
            .unwrap(),
        "create with trim+dedupe+drop-empty tags",
    )
    .await;
    assert_eq!(created["tags"], serde_json::json!(["x"]));
}

/// GET /api/queries?tags[]=x returns only the tagged query.
#[tokio::test]
async fn list_queries_filters_by_tag() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address, "tags-filter-ds").await;

    let tagged = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Filter Target",
                "query": "SELECT 1",
                "tags": ["filter-me"]
            }))
            .send()
            .await
            .unwrap(),
        "create tagged query for filter test",
    )
    .await;
    let tagged_id = tagged["id"].as_i64().unwrap();

    ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Filter Bystander",
                "query": "SELECT 1"
            }))
            .send()
            .await
            .unwrap(),
        "create untagged bystander query",
    )
    .await;

    let list_resp = client
        .get(format!("{}/api/queries?tags=filter-me", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(list_resp.status().as_u16(), 200);
    let list: Vec<serde_json::Value> = list_resp.json().await.unwrap();
    assert!(
        list.iter().any(|q| q["id"].as_i64() == Some(tagged_id)),
        "filtered list must include the tagged query"
    );
    assert!(
        list.iter().all(|q| q["name"] != "Filter Bystander"),
        "filtered list must not include the untagged bystander"
    );
}

// ── delete cascade: schedules for the query AND any monitor on it are cleaned up ─────────────

/// DELETE /api/queries/:id removes the query's schedule and its monitor's auto-created schedule
/// (via DB trigger and FK cascade).
#[tokio::test]
async fn deleting_a_query_leaves_no_schedules_for_it_or_its_monitor() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds_id = create_ds(&client, &app.address, "cascade-ds").await;

    let query = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Cascade Query",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query for cascade test",
    )
    .await;
    let query_id = query["id"].as_i64().unwrap();

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish query for cascade test",
    )
    .await;

    let schedule_resp = client
        .post(format!("{}/api/schedules", app.address))
        .json(&serde_json::json!({
            "entity_type": "query",
            "entity_id": query_id,
            "spec": { "kind": "interval", "every_secs": 900 }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(schedule_resp.status().as_u16(), 201);
    let query_schedule: serde_json::Value = schedule_resp.json().await.unwrap();
    assert!(!query_schedule["id"].is_null());

    let monitor = ok_json(
        client
            .post(format!("{}/api/monitors", app.address))
            .json(&serde_json::json!({
                "query_id": query_id,
                "name": "Cascade Monitor",
                "options": { "op": ">", "value": 100, "column": "count" }
            }))
            .send()
            .await
            .unwrap(),
        "create monitor for cascade test",
    )
    .await;
    let monitor_id = monitor["id"].as_i64().unwrap();

    let del = client
        .delete(format!("{}/api/queries/{query_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status().as_u16(), 204);

    use arcodash_core::entities::{monitor as monitor_entity, schedule};
    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};

    let query_schedules_left = schedule::Entity::find()
        .filter(schedule::Column::EntityType.eq("query"))
        .filter(schedule::Column::EntityId.eq(query_id))
        .count(&app.db)
        .await
        .unwrap();
    assert_eq!(query_schedules_left, 0, "query's own schedule must be gone");

    let monitor_schedules_left = schedule::Entity::find()
        .filter(schedule::Column::EntityType.eq("monitor"))
        .filter(schedule::Column::EntityId.eq(monitor_id))
        .count(&app.db)
        .await
        .unwrap();
    assert_eq!(
        monitor_schedules_left, 0,
        "the monitor's schedule must be gone too"
    );

    let monitor_left = monitor_entity::Entity::find_by_id(monitor_id)
        .one(&app.db)
        .await
        .unwrap();
    assert!(
        monitor_left.is_none(),
        "the monitor itself must cascade-delete with its query"
    );
}

// ── fork serves published content to a non-editor (never the pending draft) ─────

/// Create a role + group carrying `perms`; returns the group id.
async fn make_capability_group(admin: &reqwest::Client, base: &str, perms: &[&str]) -> i64 {
    let role_resp = admin
        .post(format!("{base}/api/roles"))
        .json(&serde_json::json!({
            "name": format!("fork-test-role-{}", uuid::Uuid::new_v4().simple()),
            "permissions": perms
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(role_resp.status().as_u16(), 200);
    let role_id = role_resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();

    let grp_resp = admin
        .post(format!("{base}/api/groups"))
        .json(&serde_json::json!({ "name": format!("fork-test-group-{}", uuid::Uuid::new_v4().simple()) }))
        .send()
        .await
        .unwrap();
    assert_eq!(grp_resp.status().as_u16(), 200);
    let group_id = grp_resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();

    let assign_resp = admin
        .post(format!("{base}/api/groups/{group_id}/roles"))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(assign_resp.status().as_u16(), 200);
    group_id
}

/// A non-editor who forks a published query with a pending edit gets the published text, never the
/// owner's draft.
#[tokio::test]
async fn fork_by_non_editor_copies_published_content_not_the_pending_draft() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "fork-nonedit-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let created = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Published Then Edited",
                "query": "SELECT 'published'",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = created["id"].as_i64().unwrap();

    let publish_resp = admin
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(publish_resp.status().as_u16(), 200, "publish must succeed");

    let edited = ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 'draft-only'" }))
            .send()
            .await
            .unwrap(),
        "edit draft without publishing",
    )
    .await;
    assert_eq!(edited["query"], "SELECT 'draft-only'");
    assert_eq!(edited["published"], true);

    let group_id =
        make_capability_group(&admin, &app.address, &["view_query", "create_query"]).await;
    let (bob_id, bob) = invite_and_login(
        &app,
        &admin,
        &format!(
            "fork-nonedit-bob-{}@test.local",
            uuid::Uuid::new_v4().simple()
        ),
        "Bob Forker",
        "P@ssw0rd!",
        vec![group_id],
    )
    .await;

    let acl_resp = admin
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": bob_id, "access_type": "view" }))
        .send()
        .await
        .unwrap();
    assert_eq!(acl_resp.status().as_u16(), 200, "grant Bob viewer ACL");

    let bob_get = ok_json(
        bob.get(format!("{}/api/queries/{query_id}", app.address))
            .send()
            .await
            .unwrap(),
        "Bob GET query",
    )
    .await;
    assert_eq!(
        bob_get["query"], "SELECT 'published'",
        "a non-editor's GET must already show published content"
    );

    let fork = ok_json(
        bob.post(format!("{}/api/queries/{query_id}/fork", app.address))
            .send()
            .await
            .unwrap(),
        "Bob forks the query",
    )
    .await;
    assert_eq!(
        fork["query"], "SELECT 'published'",
        "a non-editor's fork must copy the published text, not the pending draft"
    );
    assert_eq!(fork["user_id"].as_i64().unwrap(), bob_id);
}

/// The owner/editor forking their own query still gets the draft text.
#[tokio::test]
async fn fork_by_editor_still_copies_the_editors_own_draft() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "fork-editor-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let created = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Owner Fork Source",
                "query": "SELECT 'published'",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = created["id"].as_i64().unwrap();

    let publish_resp = admin
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(publish_resp.status().as_u16(), 200);

    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 'draft-only'" }))
            .send()
            .await
            .unwrap(),
        "edit draft without publishing",
    )
    .await;

    let fork = ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/fork", app.address))
            .send()
            .await
            .unwrap(),
        "owner forks own query",
    )
    .await;
    assert_eq!(
        fork["query"], "SELECT 'draft-only'",
        "the owner/editor's own fork must copy their own draft, not fall back to published"
    );
}

/// A never-published query has no published content, so a non-editor's fork copies the draft.
#[tokio::test]
async fn fork_of_a_never_published_query_by_non_editor_copies_the_draft() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "fork-neverpub-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let created = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Never Published",
                "query": "SELECT 'draft-only-ever'",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = created["id"].as_i64().unwrap();
    assert_eq!(created["published"], false);

    let group_id =
        make_capability_group(&admin, &app.address, &["view_query", "create_query"]).await;
    let (bob_id, bob) = invite_and_login(
        &app,
        &admin,
        &format!(
            "fork-neverpub-bob-{}@test.local",
            uuid::Uuid::new_v4().simple()
        ),
        "Bob Forker",
        "P@ssw0rd!",
        vec![group_id],
    )
    .await;

    let acl_resp = admin
        .post(format!("{}/api/queries/{query_id}/acl", app.address))
        .json(&serde_json::json!({ "user_id": bob_id, "access_type": "view" }))
        .send()
        .await
        .unwrap();
    assert_eq!(acl_resp.status().as_u16(), 200);

    let fork = ok_json(
        bob.post(format!("{}/api/queries/{query_id}/fork", app.address))
            .send()
            .await
            .unwrap(),
        "Bob forks the never-published query",
    )
    .await;
    assert_eq!(
        fork["query"], "SELECT 'draft-only-ever'",
        "a never-published query has nothing else to serve — fork must still copy the draft"
    );
}

// ── malformed options fail at the write, not silently at every read ────────

/// An `options` blob whose `max_age` isn't a number must 400 naming the field instead of being
/// persisted.
#[tokio::test]
async fn create_query_rejects_malformed_options() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "bad-options-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds for malformed-options test",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let resp = client
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "Bad Options",
            "query": "SELECT 1",
            "options": { "max_age": "60" }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    assert_eq!(status, 400, "malformed options must 400: {body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("expected i64"),
        "error must describe the type mismatch: {body}"
    );
}

/// The same malformed `options` blob must also 400 through `POST /api/queries/:id` (update).
#[tokio::test]
async fn update_query_rejects_malformed_options() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "bad-options-update-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Good Options",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = q["id"].as_i64().unwrap();

    let resp = client
        .post(format!("{}/api/queries/{query_id}", app.address))
        .json(&serde_json::json!({ "options": { "max_age": "60" } }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    assert_eq!(status, 400, "malformed options must 400 on update: {body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("expected i64"),
        "error must describe the type mismatch: {body}"
    );
}

/// A legacy row with a malformed `options` blob must fail execute with 400, not silently drop
/// parameter definitions (which would also skip the manage-access gate).
#[tokio::test]
async fn execute_400s_naming_options_when_a_preexisting_row_has_malformed_options() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(
                &serde_json::json!({ "name": "degraded-options-ds", "type": "pg", "options": {} }),
            )
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Degrades Gracefully",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = q["id"].as_i64().unwrap();

    // Simulate a pre-existing row whose options blob wouldn't pass today's write-side check.
    use arcodash_core::entities::query;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    let existing = query::Entity::find_by_id(query_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: query::ActiveModel = existing.into();
    active.options = Set(serde_json::json!({
        "max_age": "60",
        "parameters": [{ "name": "x", "type": "number" }]
    }));
    active.update(&app.db).await.unwrap();

    let resp = client
        .post(format!("{}/api/queries/{query_id}/results", app.address))
        .json(&serde_json::json!({ "parameters": {} }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(
        status, 400,
        "a structurally-invalid options blob must 400, not silently execute: {body}"
    );
    assert!(
        body.contains("options"),
        "the error must name the options blob as the problem: {body}"
    );
}

// ── updated_at reflects the last edit ───────────────────────────────────────

/// PATCH-ing a query's name must strictly advance `updated_at` and must not touch `created_at`.
#[tokio::test]
async fn update_query_bumps_updated_at_but_not_created_at() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "updated-at-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let created = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Before Edit",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = created["id"].as_i64().unwrap();
    let created_at = created["created_at"].as_str().unwrap().to_string();
    let updated_at_before = created["updated_at"].as_str().unwrap().to_string();

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let updated = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "name": "After Edit" }))
            .send()
            .await
            .unwrap(),
        "update query name",
    )
    .await;
    assert_eq!(updated["created_at"].as_str().unwrap(), created_at);
    assert!(
        updated["updated_at"].as_str().unwrap() > updated_at_before.as_str(),
        "updated_at must strictly increase after an edit: before={updated_at_before} after={}",
        updated["updated_at"]
    );
}

// ── audit events for the query/run lifecycle ────────────────────────

/// Most recent `events` row matching `(org_id, action, object_id)`, if any.
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

/// Every mutating query lifecycle verb records exactly one `events` row with the expected
/// `action`/`object_type`, walked as one continuous lifecycle.
#[tokio::test]
async fn query_lifecycle_actions_each_record_an_audit_event() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    use arcodash_core::entities::query;
    use sea_orm::EntityTrait;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "lifecycle-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let created = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Lifecycle Query",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = created["id"].as_i64().unwrap();
    let org_id = query::Entity::find_by_id(query_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .org_id;
    let created_event = latest_event(&app.db, org_id, "query.created", query_id)
        .await
        .expect("query.created event must be recorded");
    assert_eq!(created_event.object_type.as_deref(), Some("query"));

    let updated = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "name": "Lifecycle Query Renamed" }))
            .send()
            .await
            .unwrap(),
        "update query",
    )
    .await;
    let update_event = latest_event(&app.db, org_id, "query.updated", query_id)
        .await
        .expect("query.updated event must be recorded");
    assert_eq!(update_event.object_type.as_deref(), Some("query"));
    let working_version = updated["working_version"].as_i64().unwrap() as i32;

    // publish (first) -> query.published, first_publish: true
    let published = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": working_version }))
            .send()
            .await
            .unwrap(),
        "publish query",
    )
    .await;
    let publish_event = latest_event(&app.db, org_id, "query.published", query_id)
        .await
        .expect("query.published event must be recorded");
    assert_eq!(
        publish_event.additional_properties["first_publish"],
        serde_json::json!(true)
    );
    let working_version = published["working_version"].as_i64().unwrap() as i32;

    // draft edit, then discard -> query.discarded
    let edited = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({
                "query": "SELECT 2",
                "expected_version": working_version
            }))
            .send()
            .await
            .unwrap(),
        "draft edit before discard",
    )
    .await;
    let working_version = edited["working_version"].as_i64().unwrap() as i32;
    let discarded = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/discard", app.address))
            .json(&serde_json::json!({ "expected_version": working_version }))
            .send()
            .await
            .unwrap(),
        "discard changes",
    )
    .await;
    latest_event(&app.db, org_id, "query.discarded", query_id)
        .await
        .expect("query.discarded event must be recorded");
    let working_version = discarded["working_version"].as_i64().unwrap() as i32;

    // draft edit, publish again (second time) -> first_publish: false; then unpublish
    let edited2 = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({
                "query": "SELECT 3",
                "expected_version": working_version
            }))
            .send()
            .await
            .unwrap(),
        "draft edit before second publish",
    )
    .await;
    let working_version = edited2["working_version"].as_i64().unwrap() as i32;
    let republished = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": working_version }))
            .send()
            .await
            .unwrap(),
        "second publish",
    )
    .await;
    let second_publish_event = latest_event(&app.db, org_id, "query.published", query_id)
        .await
        .expect("second query.published event must be recorded");
    assert_eq!(
        second_publish_event.additional_properties["first_publish"],
        serde_json::json!(false),
        "a republish must not report first_publish"
    );
    let working_version = republished["working_version"].as_i64().unwrap() as i32;

    ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/unpublish", app.address))
            .json(&serde_json::json!({ "expected_version": working_version }))
            .send()
            .await
            .unwrap(),
        "unpublish",
    )
    .await;
    latest_event(&app.db, org_id, "query.unpublished", query_id)
        .await
        .expect("query.unpublished event must be recorded");

    // fork -> query.forked, source_query_id points back at the original
    let fork = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/fork", app.address))
            .send()
            .await
            .unwrap(),
        "fork",
    )
    .await;
    let fork_id = fork["id"].as_i64().unwrap();
    let fork_event = latest_event(&app.db, org_id, "query.forked", fork_id)
        .await
        .expect("query.forked event must be recorded");
    assert_eq!(
        fork_event.additional_properties["source_query_id"],
        serde_json::json!(query_id)
    );

    // revert to version 1 (the pre-edit content) -> query.reverted, to_version: 1
    let reverted = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/revert", app.address))
            .json(&serde_json::json!({ "version": 1 }))
            .send()
            .await
            .unwrap(),
        "revert to version 1",
    )
    .await;
    let revert_event = latest_event(&app.db, org_id, "query.reverted", query_id)
        .await
        .expect("query.reverted event must be recorded");
    assert_eq!(
        revert_event.additional_properties["to_version"],
        serde_json::json!(1)
    );
    let working_version = reverted["working_version"].as_i64().unwrap() as i32;

    // restore to version 1's change -> query.restored, from_version: 1
    let versions = ok_json(
        client
            .get(format!("{}/api/queries/{query_id}/version", app.address))
            .send()
            .await
            .unwrap(),
        "list versions",
    )
    .await;
    let first_change_id = versions
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["version"].as_i64() == Some(1))
        .expect("version 1 present")["change_id"]
        .as_i64()
        .unwrap();
    client
        .post(format!("{}/api/queries/{query_id}/restore", app.address))
        .json(&serde_json::json!({
            "change_id": first_change_id,
            "expected_version": working_version
        }))
        .send()
        .await
        .unwrap();
    let restore_event = latest_event(&app.db, org_id, "query.restored", query_id)
        .await
        .expect("query.restored event must be recorded");
    assert_eq!(
        restore_event.additional_properties["from_version"],
        serde_json::json!(1)
    );
}

/// Downloading a query result (CSV) records `query.result_downloaded`.
#[tokio::test]
async fn downloading_a_query_result_records_an_audit_event() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    use arcodash_core::entities::query_result;
    use sea_orm::{
        ActiveModelTrait,
        ActiveValue::{NotSet, Set},
        EntityTrait,
    };

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "download-event-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();
    let org_id = arcodash_core::entities::data_source::Entity::find_by_id(ds_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .org_id;

    let now = chrono::Utc::now();
    let qr = query_result::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        data_source_id: Set(ds_id),
        query_hash: Set(format!("hash-{}", uuid::Uuid::new_v4().simple())),
        query: Set("SELECT 1".to_string()),
        data: Set(Some(
            serde_json::json!({ "columns": [{ "name": "n" }], "rows": [{ "n": 1 }, { "n": 2 }] }),
        )),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(Some(0.01)),
        retrieved_at: Set(now.into()),
        last_used_at: Set(now.into()),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    }
    .insert(&app.db)
    .await
    .unwrap();

    let resp = client
        .get(format!("{}/api/query_results/{}.csv", app.address, qr.id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "csv download must succeed");

    let event = latest_event(&app.db, org_id, "query.result_downloaded", qr.id)
        .await
        .expect("query.result_downloaded event must be recorded");
    assert_eq!(event.additional_properties["format"], "csv");
    assert_eq!(event.additional_properties["row_count"], 2);
}

/// A first manual `POST /api/queries/:id/run` records `run.triggered`; a second click while in
/// flight records nothing.
#[tokio::test]
async fn manual_run_records_run_triggered_only_on_first_trigger() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    use arcodash_core::entities::query;
    use sea_orm::EntityTrait;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "run-triggered-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Run Triggered Query",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = q["id"].as_i64().unwrap();
    let org_id = query::Entity::find_by_id(query_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .org_id;

    let first_resp = client
        .post(format!("{}/api/queries/{query_id}/run", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        first_resp.status().as_u16(),
        201,
        "a genuinely new run is 201, not 200"
    );
    let first: serde_json::Value = first_resp.json().await.unwrap();
    let run_id = first["id"].as_i64().unwrap();
    latest_event(&app.db, org_id, "run.triggered", run_id)
        .await
        .expect("run.triggered event must be recorded on the run that actually started");

    use sea_orm::{ColumnTrait, QueryFilter};
    let events_after_first = arcodash_core::entities::event::Entity::find()
        .filter(arcodash_core::entities::event::Column::Action.eq("run.triggered"))
        .all(&app.db)
        .await
        .unwrap()
        .len();

    let second = client
        .post(format!("{}/api/queries/{query_id}/run", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        second.status().as_u16(),
        200,
        "idempotent second click is 200, not 201"
    );

    let events_after_second = arcodash_core::entities::event::Entity::find()
        .filter(arcodash_core::entities::event::Column::Action.eq("run.triggered"))
        .all(&app.db)
        .await
        .unwrap()
        .len();
    assert_eq!(
        events_after_first, events_after_second,
        "the idempotent second click must not record a second run.triggered event"
    );
}

/// If the conveyor reports the enqueue never landed (the in-flight slot is occupied),
/// `run_service::run_now` must return an error rather than a success envelope. Called directly
/// because the HTTP harness wires `AppState.conveyor = None`.
#[tokio::test]
async fn manual_run_reports_an_error_when_the_conveyor_enqueue_is_already_inflight() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "conveyor-gap-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Conveyor Gap Query",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = q["id"].as_i64().unwrap();

    use arcodash_core::entities::{query, run};
    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
    let saved_query = query::Entity::find_by_id(query_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();

    // Occupy the conveyor's (task_type, entity_id) slot; `ON CONFLICT DO NOTHING` since
    // `conveyor_tasks` isn't truncated between runs.
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    app.db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO conveyor_tasks \
             (task_type, entity_id, status, attempt, triggered_by, created_at) \
             VALUES ('execute_query_scheduled', $1, 'pending', 1, 'enqueue', now()) \
             ON CONFLICT (task_type, entity_id) WHERE status IN ('pending', 'running') \
             DO NOTHING",
            [query_id.into()],
        ))
        .await
        .unwrap();

    let redis_url =
        std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6380".to_string());
    let conveyor = arcodash_core::query_exec::build_producer_conveyor(
        app.db.clone(),
        app.redis.clone(),
        redis_url,
    );
    let queue = std::sync::Arc::new(arcodash_core::queue::ValkeyQueue::new(app.redis.clone()));

    std::env::set_var("CONVEYOR_EXECUTE_QUERY_ENABLED", "true");
    let pool_db = arcodash_backend::db::Db::Pool(app.db.clone());
    let result = arcodash_backend::services::run_service::run_now(
        &pool_db,
        queue,
        Some(conveyor.clone()),
        app.redis.clone(),
        app.db.clone(),
        saved_query.org_id,
        saved_query.user_id,
        "query",
        query_id,
        Some(ds_id),
    )
    .await;
    std::env::remove_var("CONVEYOR_EXECUTE_QUERY_ENABLED");

    assert!(
        result.is_err(),
        "a run whose enqueue never actually landed must report an error, not a success envelope"
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
        "run_now's own pending row must be removed on enqueue failure — in Db::Pool mode the \
         deferred enqueue still runs inline and its error still propagates, so the same \
         stranded-row cleanup fires as before"
    );
}

/// An interactive `POST /api/query_results` over the data source's concurrency cap must report
/// `job.queued == false` (parked); the job status stays pollable. The cap is saturated with a
/// `running` conveyor task under the source's `resource_key`.
#[tokio::test]
async fn ad_hoc_execute_over_the_concurrency_cap_reports_parked_not_enqueued() {
    let app = skip_if_none!(common::spawn_app_with_conveyor().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "parked-cap-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    app.db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE data_sources SET max_concurrent_runs = 1 WHERE id = $1",
            [ds_id.into()],
        ))
        .await
        .unwrap();
    // `conveyor_tasks` isn't truncated by cleanup and ids reset, so clear this resource_key first
    // to avoid stale-row collisions.
    app.db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM conveyor_tasks WHERE resource_key = $1",
            [ds_id.to_string().into()],
        ))
        .await
        .unwrap();
    // Occupy the one slot: a `running` conveyor task under this data source's resource_key.
    app.db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO conveyor_tasks \
             (task_type, resource_key, status, attempt, triggered_by, created_at) \
             VALUES ('execute_query', $1, 'running', 1, 'enqueue', now())",
            [ds_id.to_string().into()],
        ))
        .await
        .unwrap();

    std::env::set_var("CONVEYOR_EXECUTE_QUERY_ENABLED", "true");
    let resp = client
        .post(format!("{}/api/query_results", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "query": "SELECT 1 AS parked_marker",
            "options": {}
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    std::env::remove_var("CONVEYOR_EXECUTE_QUERY_ENABLED");
    assert_eq!(status, 200, "parked execute is still a 200: {body_text}");
    let body: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    assert_eq!(
        body["job"]["queued"], false,
        "a parked job must report queued=false: {body}"
    );
    assert_eq!(body["job"]["status"], 1, "still PENDING: {body}");
    let job_id = body["job"]["id"].as_str().unwrap().to_string();

    let job_resp = client
        .get(format!("{}/api/jobs/{job_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(job_resp.status().as_u16(), 200);
    let job_body: serde_json::Value = job_resp.json().await.unwrap();
    assert_eq!(
        job_body["job"]["status"], 1,
        "a parked job's status record must exist and read PENDING: {job_body}"
    );
}

/// With the cap not saturated, the interactive execute enqueues normally (`job.queued == true`).
#[tokio::test]
async fn ad_hoc_execute_under_the_concurrency_cap_reports_queued() {
    let app = skip_if_none!(common::spawn_app_with_conveyor().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "unparked-cap-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    app.db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE data_sources SET max_concurrent_runs = 1 WHERE id = $1",
            [ds_id.into()],
        ))
        .await
        .unwrap();
    // `conveyor_tasks` isn't truncated by cleanup and ids reset, so clear this resource_key first
    // to avoid stale-row collisions.
    app.db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM conveyor_tasks WHERE resource_key = $1",
            [ds_id.to_string().into()],
        ))
        .await
        .unwrap();

    std::env::set_var("CONVEYOR_EXECUTE_QUERY_ENABLED", "true");
    let resp = client
        .post(format!("{}/api/query_results", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "query": "SELECT 1 AS not_parked_marker",
            "options": {}
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    std::env::remove_var("CONVEYOR_EXECUTE_QUERY_ENABLED");
    assert_eq!(status, 200, "{body_text}");
    let body: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    assert_eq!(
        body["job"]["queued"], true,
        "an execute under the cap must report queued=true: {body}"
    );
}

/// With the conveyor flag off, the legacy queue is used and `job.queued` is always `true`.
#[tokio::test]
async fn ad_hoc_execute_with_conveyor_flag_off_always_reports_queued() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "legacy-queued-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let resp = client
        .post(format!("{}/api/query_results", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "query": "SELECT 1 AS legacy_marker",
            "options": {}
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "{body_text}");
    let body: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    assert_eq!(body["job"]["queued"], true, "{body}");
}

/// Downloading a run's CSV result (`GET /api/runs/:id/result.csv`) records `run.result_downloaded`.
#[tokio::test]
async fn downloading_a_run_result_records_an_audit_event() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    use arcodash_core::entities::{query, query_result, run};
    use sea_orm::{
        ActiveModelTrait,
        ActiveValue::{NotSet, Set},
        EntityTrait,
    };

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "run-download-event-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Run Download Event Query",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = q["id"].as_i64().unwrap();
    let org_id = query::Entity::find_by_id(query_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .org_id;

    let now = chrono::Utc::now();
    let qr = query_result::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        data_source_id: Set(ds_id),
        query_hash: Set(format!("hash-{}", uuid::Uuid::new_v4().simple())),
        query: Set("SELECT 1".to_string()),
        data: Set(Some(
            serde_json::json!({ "columns": [{ "name": "n" }], "rows": [{ "n": 1 }] }),
        )),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(Some(0.01)),
        retrieved_at: Set(now.into()),
        last_used_at: Set(now.into()),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    }
    .insert(&app.db)
    .await
    .unwrap();

    let run_row = run::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        entity_type: Set("query".to_string()),
        entity_id: Set(query_id),
        schedule_id: Set(None),
        data_source_id: Set(Some(ds_id)),
        scheduled_for: Set(None),
        triggered_by: Set("manual".to_string()),
        status: Set("success".to_string()),
        waiting_on_run_id: Set(None),
        job_id: Set(None),
        worker_id: Set(None),
        lease_expires_at: Set(None),
        enqueued_at: NotSet,
        started_at: Set(Some(now.into())),
        finished_at: Set(Some(now.into())),
        duration_ms: Set(Some(10)),
        row_count: Set(Some(1)),
        error: Set(None),
        data_age_secs: Set(None),
        query_result_id: Set(Some(qr.id)),
        condition_met: Set(None),
        created_at: Set(now.into()),
    }
    .insert(&app.db)
    .await
    .unwrap();

    let resp = client
        .get(format!(
            "{}/api/runs/{}/result.csv",
            app.address, run_row.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "run csv download must succeed");

    let event = latest_event(&app.db, org_id, "run.result_downloaded", run_row.id)
        .await
        .expect("run.result_downloaded event must be recorded");
    assert_eq!(event.additional_properties["format"], "csv");
}

// ── shared helpers for the pointer/cache-hit and capability tests below ────────────────────

/// Org id owning a data source.
async fn org_id_for_ds(db: &sea_orm::DatabaseConnection, ds_id: i64) -> i64 {
    use sea_orm::EntityTrait;
    arcodash_core::entities::data_source::Entity::find_by_id(ds_id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .org_id
}

/// Seed a `query_results` row directly (no worker) and return its id.
async fn seed_result(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    data_source_id: i64,
    query_hash: &str,
    query: &str,
    data: serde_json::Value,
) -> i64 {
    use arcodash_core::entities::query_result;
    use sea_orm::{
        ActiveModelTrait,
        ActiveValue::{NotSet, Set},
    };
    let now = chrono::Utc::now();
    query_result::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        data_source_id: Set(data_source_id),
        query_hash: Set(query_hash.to_string()),
        query: Set(query.to_string()),
        data: Set(Some(data)),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(Some(0.01)),
        retrieved_at: Set(now.into()),
        last_used_at: Set(now.into()),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    }
    .insert(db)
    .await
    .unwrap()
    .id
}

/// Overwrite a query's result pointers, standing in for a completed execution (no in-process
/// worker).
async fn set_pointers(
    db: &sea_orm::DatabaseConnection,
    query_id: i64,
    latest: Option<i64>,
    published_latest: Option<i64>,
) {
    use arcodash_core::entities::query;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    let existing = query::Entity::find_by_id(query_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut active: query::ActiveModel = existing.into();
    active.latest_query_data_id = Set(latest);
    active.published_latest_query_data_id = Set(published_latest);
    active.update(db).await.unwrap();
}

/// `(latest_query_data_id, published_latest_query_data_id)` straight from the row.
async fn get_pointers(
    db: &sea_orm::DatabaseConnection,
    query_id: i64,
) -> (Option<i64>, Option<i64>) {
    use arcodash_core::entities::query;
    use sea_orm::EntityTrait;
    let q = query::Entity::find_by_id(query_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    (q.latest_query_data_id, q.published_latest_query_data_id)
}

/// Rows from `GET /api/queries`, handling both the `{results: [...]}` envelope and a plain array.
fn list_rows(list: &serde_json::Value) -> Vec<serde_json::Value> {
    list["results"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| list.as_array().cloned().unwrap())
}

// ── list_for_user applies the same capability∩reach test get_for_user does ─────────────

/// `GET /api/queries` must hide an unpublished draft from an Editor-tier ACL grantee lacking
/// `edit_query`, like `GET /api/queries/{id}`; a grantee with the capability still sees it.
#[tokio::test]
async fn list_hides_draft_from_editor_grant_without_edit_query_capability() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "list-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let created = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "List Query",
                "query": "SELECT 'published-marker'",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = created["id"].as_i64().unwrap();

    let publish = admin
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": created["working_version"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(publish.status().as_u16(), 200, "publish must succeed");

    let edited = ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 'draft-marker'" }))
            .send()
            .await
            .unwrap(),
        "draft edit after publish",
    )
    .await;
    assert!(
        edited["has_unpublished_changes"].as_bool().unwrap(),
        "fixture must actually have a pending draft edit"
    );

    // Capability-less Editor grantee: Editor-tier ACL but no edit_query.
    let no_cap_group = make_capability_group(&admin, &app.address, &["view_query"]).await;
    let (no_cap_uid, no_cap_client) = invite_and_login(
        &app,
        &admin,
        &format!("nocap-{}@test.local", uuid::Uuid::new_v4().simple()),
        "No Cap",
        "P@ssw0rd!",
        vec![no_cap_group],
    )
    .await;

    // Capability-holding Editor grantee: same tier plus edit_query.
    let cap_group =
        make_capability_group(&admin, &app.address, &["view_query", "edit_query"]).await;
    let (cap_uid, cap_client) = invite_and_login(
        &app,
        &admin,
        &format!("cap-{}@test.local", uuid::Uuid::new_v4().simple()),
        "Cap",
        "P@ssw0rd!",
        vec![cap_group],
    )
    .await;

    for uid in [no_cap_uid, cap_uid] {
        let grant = admin
            .post(format!("{}/api/queries/{query_id}/acl", app.address))
            .json(&serde_json::json!({ "grantee_id": uid, "access_type": "editor" }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            grant.status().as_u16(),
            200,
            "editor ACL grant must succeed"
        );
    }

    // Capability-less grantee: list serves the published row, matching `get`.
    let list_no_cap = ok_json(
        no_cap_client
            .get(format!("{}/api/queries", app.address))
            .send()
            .await
            .unwrap(),
        "list as capability-less editor grantee",
    )
    .await;
    let row = list_rows(&list_no_cap)
        .into_iter()
        .find(|q| q["id"].as_i64() == Some(query_id))
        .expect("query must be in list");
    assert_eq!(
        row["query"], "SELECT 'published-marker'",
        "REQUIRED: capability-less Editor grantee must see published SQL in the list, not the draft"
    );

    let get_no_cap = ok_json(
        no_cap_client
            .get(format!("{}/api/queries/{query_id}", app.address))
            .send()
            .await
            .unwrap(),
        "get as capability-less editor grantee",
    )
    .await;
    assert_eq!(
        row["query"], get_no_cap["query"],
        "list and get must agree on what a capability-less Editor grantee sees"
    );

    // Capability-holding grantee: list still shows the draft.
    let list_cap = ok_json(
        cap_client
            .get(format!("{}/api/queries", app.address))
            .send()
            .await
            .unwrap(),
        "list as capability-holding editor grantee",
    )
    .await;
    let row = list_rows(&list_cap)
        .into_iter()
        .find(|q| q["id"].as_i64() == Some(query_id))
        .expect("query must be in list");
    assert_eq!(
        row["query"], "SELECT 'draft-marker'",
        "an Editor grantee who also holds edit_query must still see the draft in the list"
    );
}

/// `list_for_user`'s owner check is unconditional while `get_for_user` applies capability∩reach, so
/// an owner stripped of `edit_query` sees different things on the two surfaces. This pins that
/// current behavior rather than asserting an invariant.
#[tokio::test]
async fn list_still_shows_draft_to_owner_stripped_of_edit_query_capability() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let role = ok_json(
        admin
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({
                "name": format!("owner-role-{}", uuid::Uuid::new_v4().simple()),
                "permissions": ["view_query", "create_query", "edit_query"]
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
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({
                "name": format!("owner-group-{}", uuid::Uuid::new_v4().simple())
            }))
            .send()
            .await
            .unwrap(),
        "create group",
    )
    .await;
    let group_id = group["id"].as_i64().unwrap();
    let assign = admin
        .post(format!("{}/api/groups/{group_id}/roles", app.address))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(assign.status().as_u16(), 200);
    let unrestrict = admin
        .put(format!(
            "{}/api/groups/{group_id}/data_source_access",
            app.address
        ))
        .json(&serde_json::json!({ "restricted": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(unrestrict.status().as_u16(), 200);

    let (_owner_uid, owner_client) = invite_and_login(
        &app,
        &admin,
        &format!("owner-{}@test.local", uuid::Uuid::new_v4().simple()),
        "Owner",
        "P@ssw0rd!",
        vec![group_id],
    )
    .await;

    // Admin creates the data source; the owner only needs View reach, which the unrestricted group
    // grants.
    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "owner-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let created = ok_json(
        owner_client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Owner Query",
                "query": "SELECT 'published-marker'",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query as owner",
    )
    .await;
    let query_id = created["id"].as_i64().unwrap();

    let publish = owner_client
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": created["working_version"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(publish.status().as_u16(), 200);

    let edited = ok_json(
        owner_client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({ "query": "SELECT 'draft-marker'" }))
            .send()
            .await
            .unwrap(),
        "draft edit as owner",
    )
    .await;
    assert!(edited["has_unpublished_changes"].as_bool().unwrap());

    // Strip edit_query from the owner's role; ownership reach is unchanged.
    let downgrade = admin
        .post(format!("{}/api/roles/{role_id}", app.address))
        .json(&serde_json::json!({ "permissions": ["view_query", "create_query"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        downgrade.status().as_u16(),
        200,
        "role downgrade must succeed"
    );

    let list = ok_json(
        owner_client
            .get(format!("{}/api/queries", app.address))
            .send()
            .await
            .unwrap(),
        "list as capability-stripped owner",
    )
    .await;
    let list_row = list_rows(&list)
        .into_iter()
        .find(|q| q["id"].as_i64() == Some(query_id))
        .expect("query must be in list");

    let get = ok_json(
        owner_client
            .get(format!("{}/api/queries/{query_id}", app.address))
            .send()
            .await
            .unwrap(),
        "get as capability-stripped owner",
    )
    .await;

    // Pinned behavior: list's owner short-circuit serves the draft; get drops the capability-less
    // owner to the published version.
    assert_eq!(
        list_row["query"], "SELECT 'draft-marker'",
        "list still shows the draft to its owner even after edit_query is stripped"
    );
    assert_eq!(
        get["query"], "SELECT 'published-marker'",
        "get drops the capability-less owner to the published version"
    );
    assert_ne!(
        list_row["query"], get["query"],
        "list and get disagree for an owner who has lost edit_query"
    );
}

// ── execute_saved only repoints the published pointer for a parameter-free run ─────────

/// A parameterized `POST /api/queries/:id/results` must never move the org-wide published pointer
/// (it would leak filtered rows onto dashboard tiles); a parameter-free run must. Drives the
/// cache-hit branch via seeded `query_results` rows.
#[tokio::test]
async fn execute_saved_repoints_published_pointer_only_when_parameter_free() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    use arcodash_core::hash::gen_query_hash;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "params-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();
    let org_id = org_id_for_ds(&app.db, ds_id).await;

    let sql = "SELECT 1 AS val";
    let created = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Run Query",
                "query": sql,
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = created["id"].as_i64().unwrap();

    let publish = client
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": created["working_version"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(publish.status().as_u16(), 200);

    let hash_canonical = gen_query_hash(sql, &serde_json::Value::Null);
    let hash_parameterized = gen_query_hash(sql, &serde_json::json!({ "region": "EU" }));
    assert_ne!(
        hash_canonical, hash_parameterized,
        "fixture needs two distinct cache keys"
    );

    let canonical_result = seed_result(
        &app.db,
        org_id,
        ds_id,
        &hash_canonical,
        sql,
        serde_json::json!({ "columns": [{ "name": "val" }], "rows": [{ "val": "canonical" }] }),
    )
    .await;
    let eu_result = seed_result(
        &app.db,
        org_id,
        ds_id,
        &hash_parameterized,
        sql,
        serde_json::json!({ "columns": [{ "name": "val" }], "rows": [{ "val": "eu" }] }),
    )
    .await;

    let exec1 = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/results", app.address))
            .json(&serde_json::json!({ "parameters": null, "max_age": 86400 }))
            .send()
            .await
            .unwrap(),
        "execute without parameters",
    )
    .await;
    assert_eq!(
        exec1["query_result"]["id"].as_i64(),
        Some(canonical_result),
        "must be a cache hit against the canonical row"
    );

    let published1 = ok_json(
        client
            .get(format!(
                "{}/api/queries/{query_id}/results?published_only=true",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "published_only after canonical run",
    )
    .await;
    assert_eq!(
        published1["query_result"]["id"].as_i64(),
        Some(canonical_result),
        "a parameter-free run must move the published pointer"
    );

    // With parameters: a distinct cache hit, but must NOT move the published pointer.
    let exec2 = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/results", app.address))
            .json(&serde_json::json!({ "parameters": {"region": "EU"}, "max_age": 86400 }))
            .send()
            .await
            .unwrap(),
        "execute with parameters",
    )
    .await;
    assert_eq!(
        exec2["query_result"]["id"].as_i64(),
        Some(eu_result),
        "must be a cache hit against the EU row"
    );

    let published2 = ok_json(
        client
            .get(format!(
                "{}/api/queries/{query_id}/results?published_only=true",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "published_only after parameterized run",
    )
    .await;
    assert_eq!(
        published2["query_result"]["id"].as_i64(),
        Some(canonical_result),
        "REQUIRED: a parameterized run must not move the published pointer — it must still be \
         the canonical result"
    );
}

// ── revert/restore/discard_changes null the pointer(s) they invalidate ─────────────────

/// A version-swap (revert/restore/discard) must null every pointer whose content it replaced, else
/// rows would show under the wrong SQL. Walked as one flow since each requires a published query.
#[tokio::test]
async fn revert_restore_discard_null_the_stale_result_pointer() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "pointer-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();
    let org_id = org_id_for_ds(&app.db, ds_id).await;

    let created = ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Pointer Query",
                "query": "SELECT 1 AS v1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = created["id"].as_i64().unwrap();

    let r1 = seed_result(
        &app.db,
        org_id,
        ds_id,
        "pointer-r1",
        "SELECT 1 AS v1",
        serde_json::json!({ "columns": [{ "name": "v1" }], "rows": [{ "v1": 1 }] }),
    )
    .await;
    set_pointers(&app.db, query_id, Some(r1), None).await;

    let publish1 = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": created["working_version"] }))
            .send()
            .await
            .unwrap(),
        "first publish",
    )
    .await;
    assert_eq!(get_pointers(&app.db, query_id).await, (Some(r1), Some(r1)));
    let working_version = publish1["working_version"].as_i64().unwrap() as i32;

    // discard: a plain edit leaves the draft pointer stale; discarding must null it.
    let edited1 = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({
                "query": "SELECT 2 AS v2",
                "expected_version": working_version
            }))
            .send()
            .await
            .unwrap(),
        "edit before discard",
    )
    .await;
    let working_version = edited1["working_version"].as_i64().unwrap() as i32;

    let discarded = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/discard", app.address))
            .json(&serde_json::json!({ "expected_version": working_version }))
            .send()
            .await
            .unwrap(),
        "discard",
    )
    .await;
    let (latest, published_latest) = get_pointers(&app.db, query_id).await;
    assert_eq!(
        latest, None,
        "REQUIRED: discard must null the draft pointer it just invalidated"
    );
    assert_eq!(
        published_latest,
        Some(r1),
        "discard must not touch the published pointer"
    );
    let working_version = discarded["working_version"].as_i64().unwrap() as i32;

    // restore: edit + republish, seed a fresh pointer, restore to v1. Restore also rewrites
    // published_*, so both pointers must be null.
    let edited2 = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({
                "query": "SELECT 3 AS v3",
                "expected_version": working_version
            }))
            .send()
            .await
            .unwrap(),
        "edit before second publish",
    )
    .await;
    let working_version = edited2["working_version"].as_i64().unwrap() as i32;
    let republished = ok_json(
        client
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": working_version }))
            .send()
            .await
            .unwrap(),
        "second publish",
    )
    .await;
    let working_version = republished["working_version"].as_i64().unwrap() as i32;

    let r2 = seed_result(
        &app.db,
        org_id,
        ds_id,
        "pointer-r2",
        "SELECT 3 AS v3",
        serde_json::json!({ "columns": [{ "name": "v3" }], "rows": [{ "v3": 3 }] }),
    )
    .await;
    set_pointers(&app.db, query_id, Some(r2), Some(r2)).await;

    let versions = ok_json(
        client
            .get(format!("{}/api/queries/{query_id}/version", app.address))
            .send()
            .await
            .unwrap(),
        "list versions",
    )
    .await;
    let v1_change_id = versions
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["version"].as_i64() == Some(1))
        .expect("version 1 present")["change_id"]
        .as_i64()
        .unwrap();

    client
        .post(format!("{}/api/queries/{query_id}/restore", app.address))
        .json(&serde_json::json!({
            "change_id": v1_change_id,
            "expected_version": working_version
        }))
        .send()
        .await
        .unwrap();
    let (latest, published_latest) = get_pointers(&app.db, query_id).await;
    assert_eq!(
        latest, None,
        "REQUIRED: restore must null the draft pointer it just invalidated"
    );
    assert_eq!(
        published_latest, None,
        "REQUIRED: restore must null the published pointer it just invalidated"
    );

    // revert: revert never touches published_*, so it stays null.
    let r3 = seed_result(
        &app.db,
        org_id,
        ds_id,
        "pointer-r3",
        "SELECT 1 AS v1",
        serde_json::json!({ "columns": [{ "name": "v1" }], "rows": [{ "v1": 1 }] }),
    )
    .await;
    set_pointers(&app.db, query_id, Some(r3), None).await;

    client
        .post(format!("{}/api/queries/{query_id}/revert", app.address))
        .json(&serde_json::json!({ "version": 1 }))
        .send()
        .await
        .unwrap();
    let (latest, published_latest) = get_pointers(&app.db, query_id).await;
    assert_eq!(
        latest, None,
        "REQUIRED: revert must null the draft pointer it just invalidated"
    );
    assert_eq!(
        published_latest, None,
        "revert must not touch the published pointer"
    );
}

// ── parameter_values is gated exactly like get ──────────────────────────────────────────

/// `GET /api/queries/:id/parameter_values` follows the same published-vs-draft resolution as `get`:
/// a draft-only parameter is invisible to a non-editor.
#[tokio::test]
async fn parameter_values_hides_a_draft_only_definition_from_a_non_editor() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    use arcodash_core::hash::gen_query_hash;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "param-values-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create ds",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();
    let org_id = org_id_for_ds(&app.db, ds_id).await;

    let source_sql = "SELECT name, value FROM opts";
    let source = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Options Source",
                "query": source_sql,
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create source query",
    )
    .await;
    let source_id = source["id"].as_i64().unwrap();
    let source_publish = admin
        .post(format!("{}/api/queries/{source_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": source["working_version"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(source_publish.status().as_u16(), 200);
    seed_result(
        &app.db,
        org_id,
        ds_id,
        &gen_query_hash(source_sql, &serde_json::Value::Null),
        source_sql,
        serde_json::json!({
            "columns": [{ "name": "name" }, { "name": "value" }],
            "rows": [{ "name": "Alpha", "value": "a" }]
        }),
    )
    .await;

    // Consumer query: published without a parameter, then a QueryDropdown parameter is added to the
    // draft only.
    let consumer = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Consumer",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create consumer query",
    )
    .await;
    let consumer_id = consumer["id"].as_i64().unwrap();
    let consumer_publish = admin
        .post(format!("{}/api/queries/{consumer_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": consumer["working_version"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(consumer_publish.status().as_u16(), 200);

    let updated = ok_json(
        admin
            .post(format!("{}/api/queries/{consumer_id}", app.address))
            .json(&serde_json::json!({
                "query": "SELECT * FROM t WHERE k = {{ pick }}",
                "options": {
                    "parameters": [
                        { "name": "pick", "title": "Pick", "type": "query", "query_id": source_id }
                    ]
                }
            }))
            .send()
            .await
            .unwrap(),
        "add a draft-only parameter",
    )
    .await;
    assert!(
        updated["has_unpublished_changes"].as_bool().unwrap(),
        "fixture must actually have a pending draft edit"
    );

    // Non-editor: view_query + data-source access, no edit_query or ACL grant.
    let viewer_group = make_capability_group(&admin, &app.address, &["view_query"]).await;
    let unrestrict = admin
        .put(format!(
            "{}/api/groups/{viewer_group}/data_source_access",
            app.address
        ))
        .json(&serde_json::json!({ "restricted": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(unrestrict.status().as_u16(), 200);
    let (_viewer_uid, viewer_client) = invite_and_login(
        &app,
        &admin,
        &format!("viewer-{}@test.local", uuid::Uuid::new_v4().simple()),
        "Viewer",
        "P@ssw0rd!",
        vec![viewer_group],
    )
    .await;

    let blocked = viewer_client
        .get(format!(
            "{}/api/queries/{consumer_id}/parameter_values?name=pick",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        blocked.status().as_u16(),
        404,
        "REQUIRED: a non-editor must not see a parameter defined only in the unpublished draft"
    );

    let allowed = ok_json(
        admin
            .get(format!(
                "{}/api/queries/{consumer_id}/parameter_values?name=pick",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "parameter_values as editor",
    )
    .await;
    let values = allowed["values"]
        .as_array()
        .expect("an editor must get real values back");
    assert_eq!(values.len(), 1, "expected 1 option; got: {allowed}");
    assert_eq!(values[0]["name"], "Alpha");
}
