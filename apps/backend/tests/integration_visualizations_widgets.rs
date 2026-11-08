//! Integration tests: visualization CRUD and widget CRUD (parent-query / parent-dashboard access
//! gates).

mod common;
use common::{authed_client, make_org, spawn_app, test_api_key, user_with_role};

use sea_orm::EntityTrait;

/// Bare client with no cookies, no CSRF — used for the retired-embed-route check.
fn bare() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(false)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Invite a non-admin user (no groups, so no data-source access) and log in.
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
    let invite_body: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    let user_id = invite_body["id"].as_i64().expect("invite must return id");

    let token = {
        use arcodash_core::entities::user;
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
    assert_eq!(
        accept_resp.status().as_u16(),
        200,
        "accept invitation: {}",
        accept_resp.text().await.unwrap_or_default()
    );

    let (session, csrf_login) = app.login(email, password).await;
    (user_id, authed_client(&app.address, &session, &csrf_login))
}

/// Create a data source + query as admin. Returns `(data_source_id, query_id)`.
async fn admin_query(app: &common::TestApp, admin: &reqwest::Client, name: &str) -> (i64, i64) {
    let ds: serde_json::Value = admin
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": format!("{name}-ds"), "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let q_resp = admin
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": name,
            "query": "SELECT 1",
            "options": {}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(q_resp.status().as_u16(), 200);
    let q: serde_json::Value = q_resp.json().await.unwrap();
    (ds_id, q["id"].as_i64().unwrap())
}

/// Create a visualization via the API as admin. Returns the viz id.
async fn create_viz(app: &common::TestApp, admin: &reqwest::Client, query_id: i64) -> i64 {
    let resp = admin
        .post(format!("{}/api/visualizations", app.address))
        .json(&serde_json::json!({
            "query_id": query_id,
            "type": "table",
            "name": "Viz",
            "options": {}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    v["id"].as_i64().unwrap()
}

/// POST /api/visualizations {query_id,type,name,options} → 200 with the created viz.
#[tokio::test]
async fn create_visualization_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let (_ds, query_id) = admin_query(&app, &admin, "Viz Create Query").await;

    let resp = admin
        .post(format!("{}/api/visualizations", app.address))
        .json(&serde_json::json!({
            "query_id": query_id,
            "type": "chart",
            "name": "My Chart",
            "options": { "globalSeriesType": "line" }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "create viz: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["query_id"].as_i64(), Some(query_id));
    assert_eq!(v["type"], "chart");
    assert_eq!(v["name"], "My Chart");
    assert_eq!(v["options"]["globalSeriesType"], "line");
}

/// POST /api/visualizations for a query the caller can't access → 404.
#[tokio::test]
async fn create_visualization_for_inaccessible_query_is_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let (_ds, query_id) = admin_query(&app, &admin, "Private Query").await;

    let (_uid, user) = invite_and_login(
        &app,
        &admin,
        "viz-noaccess@test.local",
        "No Access",
        "P@ssw0rd!",
        vec![],
    )
    .await;

    let resp = user
        .post(format!("{}/api/visualizations", app.address))
        .json(&serde_json::json!({
            "query_id": query_id,
            "type": "table",
            "name": "Sneaky",
            "options": {}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "create viz on inaccessible query must be 404 (no enumeration)"
    );
}

/// POST /api/visualizations/:id → 200 with changed fields.
#[tokio::test]
async fn update_visualization_changes_fields() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let (_ds, query_id) = admin_query(&app, &admin, "Viz Update Query").await;
    let viz_id = create_viz(&app, &admin, query_id).await;

    let resp = admin
        .post(format!("{}/api/visualizations/{viz_id}", app.address))
        .json(&serde_json::json!({
            "name": "Renamed Viz",
            "type": "chart",
            "options": { "x": 1 }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "update viz: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["name"], "Renamed Viz");
    assert_eq!(v["type"], "chart");
    assert_eq!(v["options"]["x"], 1);
}

/// DELETE /api/visualizations/:id → 204, and it's gone afterward.
#[tokio::test]
async fn delete_visualization_returns_204() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let (_ds, query_id) = admin_query(&app, &admin, "Viz Delete Query").await;
    let viz_id = create_viz(&app, &admin, query_id).await;

    let resp = admin
        .delete(format!("{}/api/visualizations/{viz_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204, "delete viz must be 204");

    let again = admin
        .post(format!("{}/api/visualizations/{viz_id}", app.address))
        .json(&serde_json::json!({ "name": "X" }))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status().as_u16(), 404, "deleted viz must be gone");
}

/// GET /api/queries/:id/visualizations → lists the created viz.
#[tokio::test]
async fn list_visualizations_for_query() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let (_ds, query_id) = admin_query(&app, &admin, "Viz List Query").await;
    let viz_id = create_viz(&app, &admin, query_id).await;

    let resp = admin
        .get(format!(
            "{}/api/queries/{query_id}/visualizations",
            app.address
        ))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "list viz: {body}");
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    let items = list.as_array().expect("viz list must be array");
    assert!(
        items.iter().any(|v| v["id"].as_i64() == Some(viz_id)),
        "created viz {viz_id} must be listed; got {list}"
    );
}

/// GET /api/queries/:id/visualizations on an inaccessible query → 404 (access-checked on parent).
#[tokio::test]
async fn list_visualizations_for_inaccessible_query_is_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let (_ds, query_id) = admin_query(&app, &admin, "Private List Query").await;

    let (_uid, user) = invite_and_login(
        &app,
        &admin,
        "vizlist-noaccess@test.local",
        "No Access",
        "P@ssw0rd!",
        vec![],
    )
    .await;

    let resp = user
        .get(format!(
            "{}/api/queries/{query_id}/visualizations",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "listing viz for an inaccessible query must be 404"
    );
}

async fn create_dashboard(app: &common::TestApp, admin: &reqwest::Client, name: &str) -> i64 {
    let resp = admin
        .post(format!("{}/api/dashboards", app.address))
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let d: serde_json::Value = resp.json().await.unwrap();
    d["id"].as_i64().unwrap()
}

/// POST /api/widgets {dashboard_id,text,width omitted} → 200, width defaults to 1.
#[tokio::test]
async fn create_widget_defaults_width_to_1() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let dash_id = create_dashboard(&app, &admin, "Widget Dash").await;

    let resp = admin
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": dash_id, "text": "## Heading" }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "create widget: {body}");
    let w: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(w["dashboard_id"].as_i64(), Some(dash_id));
    assert_eq!(w["text"], "## Heading");
    assert_eq!(w["width"].as_i64(), Some(1), "width must default to 1");
    assert!(w["visualization_id"].is_null());
}

/// POST /api/widgets with a visualization_id → 200, widget references the viz.
#[tokio::test]
async fn create_widget_with_visualization() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let dash_id = create_dashboard(&app, &admin, "Viz Widget Dash").await;
    let (_ds, query_id) = admin_query(&app, &admin, "Widget Viz Query").await;
    admin
        .post(format!("{}/api/queries/{query_id}/publish", app.address))
        .json(&serde_json::json!({ "expected_version": 0 }))
        .send()
        .await
        .unwrap();
    let viz_id = create_viz(&app, &admin, query_id).await;

    let resp = admin
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({
            "dashboard_id": dash_id,
            "visualization_id": viz_id,
            "width": 2
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "create viz widget: {body}");
    let w: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(w["visualization_id"].as_i64(), Some(viz_id));
    assert_eq!(w["width"].as_i64(), Some(2));
}

/// POST /api/widgets for a dashboard the caller can't mutate → 404.
#[tokio::test]
async fn create_widget_on_unowned_dashboard_is_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let dash_id = create_dashboard(&app, &admin, "Admin Dash").await;

    let (_uid, user) = invite_and_login(
        &app,
        &admin,
        "widget-nonowner@test.local",
        "Non Owner",
        "P@ssw0rd!",
        vec![],
    )
    .await;

    let resp = user
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": dash_id, "text": "hi" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "non-owner non-admin (no capability, no reach) adding a widget must be 404, matching \
         `authorize`'s not-found convention for every denied object-bound action"
    );
}

/// POST /api/widgets for a dashboard that doesn't exist in the org → 404.
#[tokio::test]
async fn create_widget_on_missing_dashboard_is_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let resp = admin
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": 999_999, "text": "hi" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "widget on a nonexistent dashboard must be 404"
    );
}

/// POST /api/widgets/:id → 200 with updated text/width.
#[tokio::test]
async fn update_widget_changes_text_and_width() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let dash_id = create_dashboard(&app, &admin, "Update Widget Dash").await;

    let created: serde_json::Value = admin
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": dash_id, "text": "before" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let widget_id = created["id"].as_i64().unwrap();

    let resp = admin
        .post(format!("{}/api/widgets/{widget_id}", app.address))
        .json(&serde_json::json!({ "text": "after", "width": 3 }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "update widget: {body}");
    let w: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(w["text"], "after");
    assert_eq!(w["width"].as_i64(), Some(3));
}

/// A later options-only `POST /api/widgets/:id` must merge into `options.position` (the layout),
/// not overwrite it.
#[tokio::test]
async fn update_widget_options_preserves_the_layout_position() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let created_dash: serde_json::Value = admin
        .post(format!("{}/api/dashboards", app.address))
        .json(&serde_json::json!({ "name": "Layout Preserve Dash" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dash_id = created_dash["id"].as_i64().unwrap();
    let slug = created_dash["slug"].as_str().unwrap().to_string();

    let created: serde_json::Value = admin
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": dash_id, "text": "tile" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let widget_id = created["id"].as_i64().unwrap();

    let layout_resp = admin
        .put(format!("{}/api/dashboards/{slug}/layout", app.address))
        .json(&serde_json::json!({
            "positions": [{ "widget_id": widget_id, "position": { "x": 2, "y": 3, "w": 4, "h": 5 } }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(layout_resp.status().as_u16(), 200);

    let resp = admin
        .post(format!("{}/api/widgets/{widget_id}", app.address))
        .json(&serde_json::json!({ "options": { "chart_type": "bar" } }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "update widget options: {body}");
    let w: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        w["options"]["position"],
        serde_json::json!({ "x": 2, "y": 3, "w": 4, "h": 5 }),
        "an unrelated options edit must not wipe the layout position: {w}"
    );
    assert_eq!(w["options"]["chart_type"], "bar");
}

/// DELETE /api/widgets/:id → 204, and it's gone afterward.
#[tokio::test]
async fn delete_widget_returns_204() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let dash_id = create_dashboard(&app, &admin, "Delete Widget Dash").await;

    let created: serde_json::Value = admin
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": dash_id, "text": "doomed" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let widget_id = created["id"].as_i64().unwrap();

    let resp = admin
        .delete(format!("{}/api/widgets/{widget_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204, "delete widget must be 204");

    let again = admin
        .post(format!("{}/api/widgets/{widget_id}", app.address))
        .json(&serde_json::json!({ "text": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status().as_u16(), 404, "deleted widget must be gone");
}

/// The query-level visualization embed endpoint was retired; the route no longer exists.
#[tokio::test]
async fn visualization_embed_route_is_gone() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let (_ds, query_id) = admin_query(&app, &admin, "Embed Retired Query").await;
    let viz_id = create_viz(&app, &admin, query_id).await;

    let resp = bare()
        .get(format!("{}/api/visualizations/{viz_id}/embed", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "the retired embed route must no longer resolve"
    );
}

/// Creating a widget and a visualization each leave an `events` row.
#[tokio::test]
async fn widget_and_visualization_create_are_audited() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let dash_id = create_dashboard(&app, &admin, "Audited Widgets Dash").await;
    let (_ds, query_id) = admin_query(&app, &admin, "Audited Viz Query").await;
    let viz_id = create_viz(&app, &admin, query_id).await;

    let widget_resp = admin
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": dash_id, "text": "audited tile" }))
        .send()
        .await
        .unwrap();
    assert_eq!(widget_resp.status().as_u16(), 200);
    let widget_id = widget_resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();

    use arcodash_core::entities::event;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let widget_event = event::Entity::find()
        .filter(event::Column::Action.eq("widget.created"))
        .filter(event::Column::ObjectType.eq("widget"))
        .filter(event::Column::ObjectId.eq(widget_id))
        .one(&app.db)
        .await
        .unwrap();
    assert!(
        widget_event.is_some(),
        "widget.created must be recorded for the new widget"
    );

    let viz_event = event::Entity::find()
        .filter(event::Column::Action.eq("visualization.created"))
        .filter(event::Column::ObjectType.eq("visualization"))
        .filter(event::Column::ObjectId.eq(viz_id))
        .one(&app.db)
        .await
        .unwrap();
    assert!(
        viz_event.is_some(),
        "visualization.created must be recorded for the new visualization"
    );
}

/// `POST /api/queries/:id/fork` copies every visualization row onto the new query.
#[tokio::test]
async fn fork_copies_visualizations() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let (_ds, query_id) = admin_query(&app, &admin, "Fork Viz Source").await;
    let viz_id = create_viz(&app, &admin, query_id).await;

    let fork_resp = admin
        .post(format!("{}/api/queries/{query_id}/fork", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(fork_resp.status().as_u16(), 200);
    let fork: serde_json::Value = fork_resp.json().await.unwrap();
    let fork_id = fork["id"].as_i64().unwrap();
    assert!(
        fork["latest_query_data_id"].is_null(),
        "a fresh fork must never have run yet"
    );

    use arcodash_core::entities::visualization;
    use sea_orm::{ColumnTrait, QueryFilter};
    let copied = visualization::Entity::find()
        .filter(visualization::Column::QueryId.eq(fork_id))
        .all(&app.db)
        .await
        .unwrap();
    assert_eq!(
        copied.len(),
        1,
        "fork must copy the source query's visualization rows"
    );
    assert_eq!(copied[0].r#type, "table");
    assert_eq!(copied[0].name, "Viz");
    assert_ne!(
        copied[0].id, viz_id,
        "the copy must be a new row, not a reference to the source visualization"
    );
}

// Cross-org admin widget mutation: an org-A admin must not update/delete an org-B widget by numeric
// id, even though `authorize` short-circuits for admins. A real two-org request needs
// `spawn_app_with_org_resolver` and a test-only header-reading resolver.
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

/// `Authorization: Key <key>` client (CSRF-exempt) that also sends the test-only `X-Test-Org-Id`
/// header.
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

async fn create_dashboard_as(client: &reqwest::Client, address: &str, name: &str) -> i64 {
    let resp = client
        .post(format!("{address}/api/dashboards"))
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "create dashboard: {body}");
    let d: serde_json::Value = serde_json::from_str(&body).unwrap();
    d["id"].as_i64().unwrap()
}

async fn create_widget_as(client: &reqwest::Client, address: &str, dashboard_id: i64) -> i64 {
    let resp = client
        .post(format!("{address}/api/widgets"))
        .json(&serde_json::json!({ "dashboard_id": dashboard_id, "text": "org-b tile" }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "create widget: {body}");
    let w: serde_json::Value = serde_json::from_str(&body).unwrap();
    w["id"].as_i64().unwrap()
}

#[tokio::test]
async fn org_a_admin_cannot_update_org_b_widget() {
    let app = skip_if_none!(
        common::spawn_app_with_org_resolver(std::sync::Arc::new(HeaderOrgResolver)).await
    );
    app.cleanup().await;

    let org_a = make_org(&app.db).await;
    let (admin_a, _) = user_with_role(&app.db, org_a, &["admin"]).await;
    let client_a = api_key_client(&test_api_key(&admin_a.email), org_a);

    let org_b = make_org(&app.db).await;
    let (owner_b, _) =
        user_with_role(&app.db, org_b, &["create_dashboard", "edit_dashboard"]).await;
    let client_b = api_key_client(&test_api_key(&owner_b.email), org_b);
    let dash_b = create_dashboard_as(&client_b, &app.address, "Org B Dash").await;
    let widget_b = create_widget_as(&client_b, &app.address, dash_b).await;

    let resp = client_a
        .post(format!("{}/api/widgets/{widget_b}", app.address))
        .json(&serde_json::json!({ "text": "hijacked" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "an org-A admin updating an org-B widget by id must 404, not succeed cross-org"
    );

    use arcodash_core::entities::widget;
    let still = widget::Entity::find_by_id(widget_b)
        .one(&app.db)
        .await
        .unwrap()
        .expect("widget must still exist");
    assert_eq!(
        still.text.as_deref(),
        Some("org-b tile"),
        "must be unchanged"
    );
}

#[tokio::test]
async fn org_a_admin_cannot_delete_org_b_widget() {
    let app = skip_if_none!(
        common::spawn_app_with_org_resolver(std::sync::Arc::new(HeaderOrgResolver)).await
    );
    app.cleanup().await;

    let org_a = make_org(&app.db).await;
    let (admin_a, _) = user_with_role(&app.db, org_a, &["admin"]).await;
    let client_a = api_key_client(&test_api_key(&admin_a.email), org_a);

    let org_b = make_org(&app.db).await;
    let (owner_b, _) =
        user_with_role(&app.db, org_b, &["create_dashboard", "edit_dashboard"]).await;
    let client_b = api_key_client(&test_api_key(&owner_b.email), org_b);
    let dash_b = create_dashboard_as(&client_b, &app.address, "Org B Dash 2").await;
    let widget_b = create_widget_as(&client_b, &app.address, dash_b).await;

    let resp = client_a
        .delete(format!("{}/api/widgets/{widget_b}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "an org-A admin deleting an org-B widget by id must 404, not succeed cross-org"
    );

    use arcodash_core::entities::widget;
    let still = widget::Entity::find_by_id(widget_b)
        .one(&app.db)
        .await
        .unwrap();
    assert!(
        still.is_some(),
        "widget must survive the cross-org delete attempt"
    );
}

/// Positive control: an admin can act within their own org on a widget they don't own.
#[tokio::test]
async fn same_org_admin_can_still_update_a_widget_they_do_not_own() {
    let app = skip_if_none!(
        common::spawn_app_with_org_resolver(std::sync::Arc::new(HeaderOrgResolver)).await
    );
    app.cleanup().await;

    let org = make_org(&app.db).await;
    let (admin, _) = user_with_role(&app.db, org, &["admin"]).await;
    let admin_client = api_key_client(&test_api_key(&admin.email), org);

    let (owner, _) = user_with_role(&app.db, org, &["create_dashboard", "edit_dashboard"]).await;
    let owner_client = api_key_client(&test_api_key(&owner.email), org);
    let dash_id = create_dashboard_as(&owner_client, &app.address, "Shared Org Dash").await;
    let widget_id = create_widget_as(&owner_client, &app.address, dash_id).await;

    let resp = admin_client
        .post(format!("{}/api/widgets/{widget_id}", app.address))
        .json(&serde_json::json!({ "text": "admin edit" }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(
        status, 200,
        "same-org admin acting on a widget they don't own must still succeed: {body}"
    );
}
