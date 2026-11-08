//! Integration tests: query snippet CRUD + permission boundaries.
//!
//! Reads require `view_snippets`; writes require `manage_snippets`. The seeded "admin" group
//! has `admin` (implies everything). Non-admin fixtures are built with `user_with_role`, which
//! attaches a fresh custom role carrying exactly the given permission set, and are driven over
//! HTTP via the `Authorization: Key <api_key>` header (exempt from CSRF, see
//! `middleware::csrf::should_bypass_csrf`) rather than session cookies.

mod common;
use common::{api_key_client, default_org_id, ok_json};
use common::{spawn_app, user_with_role};

// ── helpers ───────────────────────────────────────────────────────────────────

/// Insert a snippet belonging to a *different* org and return its id.
/// Used to test cross-org isolation (the default-org user must get 404).
///
/// The snippet has FKs to `organizations` and `users`, so we create a real
/// second org + a user in it, then insert the snippet referencing both. A raw
/// INSERT with a fabricated org_id/user_id would violate those FKs and fail.
async fn insert_other_org_snippet(app: &common::TestApp) -> i64 {
    use arcodash_core::entities::{organization, query_snippet, user};
    use sea_orm::ActiveValue::{NotSet, Set};
    use sea_orm::EntityTrait;

    let other_org = organization::Entity::insert(organization::ActiveModel {
        id: NotSet,
        name: Set("Other Org".to_string()),
        slug: Set(format!("other-{}", uuid::Uuid::new_v4().simple())),
        settings: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    })
    .exec(&app.db)
    .await
    .expect("other org insert failed");
    let other_org_id = other_org.last_insert_id;

    let api_key = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let other_user = user::Entity::insert(user::ActiveModel {
        id: NotSet,
        org_id: Set(other_org_id),
        name: Set("Other User".to_string()),
        email: Set(format!(
            "other-{}@test.local",
            uuid::Uuid::new_v4().simple()
        )),
        password_hash: Set(None),
        groups: Set(vec![]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(&api_key)),
        disabled_at: Set(None),
        is_invitation_pending: Set(false),
        profile_image_url: Set(None),
        details: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
        digest_enabled: Set(false),
        digest_unsubscribe_token: Set(None),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .expect("other user insert failed");

    let snippet = query_snippet::Entity::insert(query_snippet::ActiveModel {
        id: NotSet,
        org_id: Set(other_org_id),
        trigger: Set("other-org-trigger".to_string()),
        description: Set(None),
        snippet: Set("SELECT other".to_string()),
        user_id: Set(other_user.last_insert_id),
        created_at: NotSet,
        updated_at: NotSet,
    })
    .exec(&app.db)
    .await
    .expect("other-org snippet insert failed");

    snippet.last_insert_id
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// POST /api/query_snippets as admin → 200 with created snippet fields.
#[tokio::test]
async fn create_snippet_as_admin_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let resp = client
        .post(format!("{}/api/query_snippets", app.address))
        .json(&serde_json::json!({
            "trigger": "sel",
            "snippet": "SELECT * FROM",
            "description": "quick select"
        }))
        .send()
        .await
        .unwrap();

    let body = ok_json(resp, "create snippet").await;
    assert_eq!(body["trigger"], "sel");
    assert_eq!(body["snippet"], "SELECT * FROM");
    assert_eq!(body["description"], "quick select");
    assert!(!body["id"].is_null());

    // Creating a snippet must leave an events row.
    use arcodash_core::entities::event;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let snippet_id = body["id"].as_i64().unwrap();
    let recorded = event::Entity::find()
        .filter(event::Column::Action.eq("snippet.created"))
        .filter(event::Column::ObjectType.eq("snippet"))
        .filter(event::Column::ObjectId.eq(snippet_id))
        .one(&app.db)
        .await
        .unwrap();
    assert!(recorded.is_some(), "snippet.created must be recorded");
}

/// GET /api/query_snippets → org-wide list includes the newly created snippet.
#[tokio::test]
async fn list_snippets_includes_created_snippet() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/query_snippets", app.address))
            .json(&serde_json::json!({
                "trigger": "listtest",
                "snippet": "SELECT 1"
            }))
            .send()
            .await
            .unwrap(),
        "create snippet for list test",
    )
    .await;
    let created_id = created["id"].as_i64().unwrap();

    let list_resp = client
        .get(format!("{}/api/query_snippets", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(list_resp.status().as_u16(), 200);

    let list: serde_json::Value = list_resp.json().await.unwrap();
    let items = list.as_array().expect("list must be a JSON array");
    assert!(
        items.iter().any(|s| s["id"].as_i64() == Some(created_id)),
        "snippet id={created_id} not found in list"
    );
}

/// GET /api/query_snippets/:id → 200 with the right data.
#[tokio::test]
async fn get_snippet_by_id_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/query_snippets", app.address))
            .json(&serde_json::json!({
                "trigger": "getone",
                "snippet": "SELECT id FROM foo"
            }))
            .send()
            .await
            .unwrap(),
        "create snippet for get test",
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    let body = ok_json(
        client
            .get(format!("{}/api/query_snippets/{id}", app.address))
            .send()
            .await
            .unwrap(),
        "get snippet by id",
    )
    .await;

    assert_eq!(body["id"], id);
    assert_eq!(body["trigger"], "getone");
    assert_eq!(body["snippet"], "SELECT id FROM foo");
}

/// GET /api/query_snippets/:id for a snippet in a different org → 404.
#[tokio::test]
async fn get_snippet_cross_org_returns_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let other_id = insert_other_org_snippet(&app).await;

    let client = app.admin_client().await;
    let resp = client
        .get(format!("{}/api/query_snippets/{other_id}", app.address))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        404,
        "snippet from another org must return 404"
    );
}

/// POST /api/query_snippets/:id (update) by a user holding only `create_query` (no
/// `manage_snippets`) → 403: `create_query` alone is not sufficient for snippet writes.
#[tokio::test]
async fn update_snippet_without_manage_snippets_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    // Admin creates the snippet.
    let admin = app.admin_client().await;
    let created = ok_json(
        admin
            .post(format!("{}/api/query_snippets", app.address))
            .json(&serde_json::json!({
                "trigger": "updateperm",
                "snippet": "SELECT 1"
            }))
            .send()
            .await
            .unwrap(),
        "admin create snippet",
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    let org_id = default_org_id(&app.db).await;
    let (user, _role_id) = user_with_role(&app.db, org_id, &["create_query"]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let resp = client
        .post(format!("{}/api/query_snippets/{id}", app.address))
        .json(&serde_json::json!({ "snippet": "SELECT 2" }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        403,
        "update with only create_query (no manage_snippets) must be 403"
    );
}

/// POST /api/query_snippets/:id (update) by a user holding `manage_snippets` (not admin) → 200.
#[tokio::test]
async fn update_snippet_with_manage_snippets_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let created = ok_json(
        admin
            .post(format!("{}/api/query_snippets", app.address))
            .json(&serde_json::json!({
                "trigger": "updatewithmanage",
                "snippet": "SELECT 1"
            }))
            .send()
            .await
            .unwrap(),
        "admin create snippet",
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    let org_id = default_org_id(&app.db).await;
    // Only `manage_snippets` granted — relies on the catalog's manage_snippets -> view_snippets
    // implication to pass the internal read gate `update` sits behind.
    let (user, _role_id) = user_with_role(&app.db, org_id, &["manage_snippets"]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let updated = ok_json(
        client
            .post(format!("{}/api/query_snippets/{id}", app.address))
            .json(&serde_json::json!({ "snippet": "SELECT updated by manager" }))
            .send()
            .await
            .unwrap(),
        "update with manage_snippets",
    )
    .await;
    assert_eq!(updated["snippet"], "SELECT updated by manager");
}

/// POST /api/query_snippets/:id (update) by admin (has create_query via admin implies all) → 200.
#[tokio::test]
async fn update_snippet_with_permission_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/query_snippets", app.address))
            .json(&serde_json::json!({
                "trigger": "updateme",
                "snippet": "SELECT 1"
            }))
            .send()
            .await
            .unwrap(),
        "create snippet for update test",
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    let updated = ok_json(
        client
            .post(format!("{}/api/query_snippets/{id}", app.address))
            .json(&serde_json::json!({ "snippet": "SELECT updated" }))
            .send()
            .await
            .unwrap(),
        "update snippet",
    )
    .await;

    assert_eq!(updated["id"], id);
    assert_eq!(updated["snippet"], "SELECT updated");
    assert_eq!(updated["trigger"], "updateme", "trigger unchanged");
}

/// DELETE /api/query_snippets/:id by a user holding only `create_query` (no `manage_snippets`)
/// → 403.
#[tokio::test]
async fn delete_snippet_without_manage_snippets_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let created = ok_json(
        admin
            .post(format!("{}/api/query_snippets", app.address))
            .json(&serde_json::json!({
                "trigger": "deleteperm",
                "snippet": "SELECT 1"
            }))
            .send()
            .await
            .unwrap(),
        "admin create snippet for delete perm test",
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    let org_id = default_org_id(&app.db).await;
    let (user, _role_id) = user_with_role(&app.db, org_id, &["create_query"]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let resp = client
        .delete(format!("{}/api/query_snippets/{id}", app.address))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        403,
        "delete with only create_query (no manage_snippets) must be 403"
    );
}

/// DELETE /api/query_snippets/:id by a user holding `manage_snippets` (not admin) → 204.
#[tokio::test]
async fn delete_snippet_with_manage_snippets_returns_204() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let created = ok_json(
        admin
            .post(format!("{}/api/query_snippets", app.address))
            .json(&serde_json::json!({
                "trigger": "deletewithmanage",
                "snippet": "SELECT 1"
            }))
            .send()
            .await
            .unwrap(),
        "admin create snippet for manage_snippets delete test",
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    let org_id = default_org_id(&app.db).await;
    let (user, _role_id) = user_with_role(&app.db, org_id, &["manage_snippets"]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let resp = client
        .delete(format!("{}/api/query_snippets/{id}", app.address))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        204,
        "delete with manage_snippets must be 204"
    );
}

/// DELETE /api/query_snippets/:id with create_query (admin) → 204.
#[tokio::test]
async fn delete_snippet_with_permission_returns_204() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/query_snippets", app.address))
            .json(&serde_json::json!({
                "trigger": "deleteme",
                "snippet": "SELECT 1"
            }))
            .send()
            .await
            .unwrap(),
        "create snippet for delete test",
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    let resp = client
        .delete(format!("{}/api/query_snippets/{id}", app.address))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        204,
        "delete with permission must be 204"
    );

    // Confirm it's gone.
    let get_resp = client
        .get(format!("{}/api/query_snippets/{id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        get_resp.status().as_u16(),
        404,
        "deleted snippet must return 404 on subsequent GET"
    );
}

// ── view_snippets read gate ──────────────────────────────────────────────────

/// GET /api/query_snippets by a user with no `view_snippets` → 403.
#[tokio::test]
async fn list_snippets_without_view_snippets_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let org_id = default_org_id(&app.db).await;
    let (user, _role_id) = user_with_role(&app.db, org_id, &[]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let resp = client
        .get(format!("{}/api/query_snippets", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "list without view_snippets must be 403"
    );
}

/// GET /api/query_snippets by a user holding `view_snippets` → 200.
#[tokio::test]
async fn list_snippets_with_view_snippets_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let org_id = default_org_id(&app.db).await;
    let (user, _role_id) = user_with_role(&app.db, org_id, &["view_snippets"]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let resp = client
        .get(format!("{}/api/query_snippets", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "list with view_snippets must be 200"
    );
}

/// GET /api/query_snippets/:id by a user with no `view_snippets` → 403.
#[tokio::test]
async fn get_snippet_without_view_snippets_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let created = ok_json(
        admin
            .post(format!("{}/api/query_snippets", app.address))
            .json(&serde_json::json!({ "trigger": "getgate", "snippet": "SELECT 1" }))
            .send()
            .await
            .unwrap(),
        "admin create snippet for read-gate test",
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    let org_id = default_org_id(&app.db).await;
    let (user, _role_id) = user_with_role(&app.db, org_id, &[]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let resp = client
        .get(format!("{}/api/query_snippets/{id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "get without view_snippets must be 403"
    );
}

/// GET /api/query_snippets/:id by a user holding `view_snippets` → 200.
#[tokio::test]
async fn get_snippet_with_view_snippets_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let created = ok_json(
        admin
            .post(format!("{}/api/query_snippets", app.address))
            .json(&serde_json::json!({ "trigger": "getgateok", "snippet": "SELECT 1" }))
            .send()
            .await
            .unwrap(),
        "admin create snippet for read-gate test",
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    let org_id = default_org_id(&app.db).await;
    let (user, _role_id) = user_with_role(&app.db, org_id, &["view_snippets"]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let resp = client
        .get(format!("{}/api/query_snippets/{id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "get with view_snippets must be 200"
    );
}

// ── manage_snippets write gate on create ─────────────────────────────────────

/// POST /api/query_snippets by a user holding only `create_query` (no `manage_snippets`) → 403.
#[tokio::test]
async fn create_snippet_without_manage_snippets_returns_403() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let org_id = default_org_id(&app.db).await;
    let (user, _role_id) = user_with_role(&app.db, org_id, &["create_query"]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let resp = client
        .post(format!("{}/api/query_snippets", app.address))
        .json(&serde_json::json!({ "trigger": "nope", "snippet": "SELECT 1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "create with only create_query (no manage_snippets) must be 403"
    );
}

/// POST /api/query_snippets by a user holding `manage_snippets` (not admin) → 200.
#[tokio::test]
async fn create_snippet_with_manage_snippets_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let org_id = default_org_id(&app.db).await;
    let (user, _role_id) = user_with_role(&app.db, org_id, &["manage_snippets"]).await;
    let client = api_key_client(&common::test_api_key(&user.email));

    let resp = client
        .post(format!("{}/api/query_snippets", app.address))
        .json(&serde_json::json!({ "trigger": "yep", "snippet": "SELECT 1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "create with manage_snippets must be 200"
    );
}
