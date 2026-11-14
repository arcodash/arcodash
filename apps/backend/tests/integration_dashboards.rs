//! Integration tests: dashboard CRUD, favorites, tags, and public share/revoke.

mod common;
use common::ok_json;
use common::{authed_client, spawn_app, TestApp};

/// Invite a fresh non-admin user, accept the invitation, log them in, and return an authenticated
/// client.
async fn non_admin_client(app: &TestApp, admin: &reqwest::Client, email: &str) -> reqwest::Client {
    non_admin_client_in_groups(app, admin, email, &[]).await
}

/// Like [`non_admin_client`], but placed in the given groups.
async fn non_admin_client_in_groups(
    app: &TestApp,
    admin: &reqwest::Client,
    email: &str,
    groups: &[i64],
) -> reqwest::Client {
    let password = "N0nAdm1n!Pass";
    let invited = ok_json(
        admin
            .post(format!("{}/api/users", app.address))
            .json(&serde_json::json!({ "name": "Non Admin", "email": email, "groups": groups }))
            .send()
            .await
            .unwrap(),
        "invite non-admin user",
    )
    .await;
    let user_id = invited["id"].as_i64().unwrap();

    use sea_orm::EntityTrait;
    let token = arcodash_core::entities::user::Entity::find_by_id(user_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .details["invitation_token"]
        .as_str()
        .expect("invitation_token must be set on invited user")
        .to_string();

    let bare = reqwest::Client::builder()
        .cookie_store(false)
        .build()
        .unwrap();
    let accept = bare
        .post(format!("{}/api/users/invitation/accept", app.address))
        .json(&serde_json::json!({ "token": token, "name": "Non Admin", "password": password }))
        .send()
        .await
        .unwrap();
    assert!(
        accept.status().is_success(),
        "accept invitation failed: {}",
        accept.status()
    );

    let (session, csrf) = app.login(email, password).await;
    authed_client(&app.address, &session, &csrf)
}

#[tokio::test]
async fn create_dashboard_returns_200() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let body = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "My Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;

    assert!(!body["id"].is_null(), "id must be set");
    assert!(
        !body["slug"].as_str().unwrap_or("").is_empty(),
        "slug must be generated"
    );
    assert_eq!(body["name"], "My Dashboard");
    assert_eq!(
        body["published"], false,
        "new dashboards are never-published by default"
    );
    assert_eq!(
        body["has_unpublished_changes"], false,
        "a never-published dashboard has nothing to have diverged from"
    );
    assert_eq!(
        body["widgets"].as_array().map(|a| a.len()).unwrap_or(99),
        0,
        "new dashboard has empty widgets"
    );
}

#[tokio::test]
async fn list_dashboards_includes_created() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Listed Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard for list test",
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    let list: serde_json::Value = client
        .get(format!("{}/api/dashboards", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let items = list.as_array().expect("list must be array");
    let found = items.iter().find(|d| d["id"].as_i64() == Some(id));
    assert!(found.is_some(), "created dashboard id={id} must be in list");
    assert_eq!(
        found.unwrap()["is_favorite"],
        false,
        "is_favorite must be false before starring"
    );
}

/// A published widget carries its own `created_at`/`updated_at`, not the dashboard's.
#[tokio::test]
async fn published_widget_carries_its_own_timestamps_not_the_dashboards() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Widget Timestamp Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    let slug = created["slug"].as_str().unwrap().to_string();
    let dashboard_created_at = created["created_at"].as_str().unwrap().to_string();

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let widget = ok_json(
        client
            .post(format!("{}/api/widgets", app.address))
            .json(&serde_json::json!({
                "dashboard_id": created["id"], "text": "Timestamped tile", "width": 1
            }))
            .send()
            .await
            .unwrap(),
        "create widget",
    )
    .await;
    let widget_created_at = widget["created_at"].as_str().unwrap().to_string();
    assert_ne!(
        widget_created_at, dashboard_created_at,
        "sanity: widget must be created strictly after the dashboard"
    );

    ok_json(
        client
            .post(format!("{}/api/dashboards/{slug}/publish", app.address))
            .send()
            .await
            .unwrap(),
        "publish dashboard",
    )
    .await;

    let published = ok_json(
        client
            .get(format!("{}/api/dashboards/{slug}", app.address))
            .send()
            .await
            .unwrap(),
        "get published dashboard",
    )
    .await;
    let published_widgets = published["widgets"].as_array().unwrap();
    assert_eq!(published_widgets.len(), 1);
    assert_eq!(
        published_widgets[0]["created_at"].as_str().unwrap(),
        widget_created_at,
        "published widget's created_at must match the widget's own, not the dashboard's"
    );
    assert_ne!(
        published_widgets[0]["created_at"].as_str().unwrap(),
        dashboard_created_at,
        "published widget's created_at must NOT be the dashboard's own timestamp"
    );
}

#[tokio::test]
async fn get_dashboard_by_slug() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Slug Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard for slug test",
    )
    .await;
    let slug = created["slug"].as_str().unwrap().to_string();

    let body = ok_json(
        client
            .get(format!("{}/api/dashboards/{slug}", app.address))
            .send()
            .await
            .unwrap(),
        "get by slug",
    )
    .await;

    assert_eq!(body["slug"], slug);
    assert!(body["widgets"].is_array(), "widgets must be an array");

    let miss = client
        .get(format!(
            "{}/api/dashboards/nonexistent-slug-xyz",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        miss.status().as_u16(),
        404,
        "nonexistent slug must return 404"
    );
}

#[tokio::test]
async fn update_dashboard_by_owner() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Update Me" }))
            .send()
            .await
            .unwrap(),
        "create dashboard for update test",
    )
    .await;
    let slug = created["slug"].as_str().unwrap().to_string();

    let updated = ok_json(
        client
            .post(format!("{}/api/dashboards/{slug}", app.address))
            .json(&serde_json::json!({
                "name": "Updated Name",
                "tags": ["alpha", "beta"],
                "layout": [{"col": 0, "row": 0}]
            }))
            .send()
            .await
            .unwrap(),
        "update dashboard",
    )
    .await;

    assert_eq!(updated["name"], "Updated Name");
    let tags = updated["tags"].as_array().unwrap();
    assert!(
        tags.iter().any(|t| t.as_str() == Some("alpha")),
        "alpha tag must be set"
    );
    assert!(
        tags.iter().any(|t| t.as_str() == Some("beta")),
        "beta tag must be set"
    );
}

/// `is_published` in an update body is rejected as 422 (`deny_unknown_fields`) and mutates nothing.
#[tokio::test]
async fn update_dashboard_rejects_is_published_field() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Publish Field Test" }))
            .send()
            .await
            .unwrap(),
        "create dashboard for is_published rejection test",
    )
    .await;
    let slug = created["slug"].as_str().unwrap().to_string();
    let published_before = created["published"].clone();

    let resp = client
        .post(format!("{}/api/dashboards/{slug}", app.address))
        .json(&serde_json::json!({ "is_published": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        422,
        "is_published in the update body must be rejected as 422"
    );

    let after = ok_json(
        client
            .get(format!("{}/api/dashboards/{slug}", app.address))
            .send()
            .await
            .unwrap(),
        "get dashboard after rejected is_published update",
    )
    .await;
    assert_eq!(
        after["published"], published_before,
        "publish state must be unchanged by the rejected update"
    );
}

/// Update and archive by a non-owner non-admin are rejected (403/404); unauthenticated too.
#[tokio::test]
async fn update_and_archive_by_non_owner_is_forbidden() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;
    let created = ok_json(
        admin_client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Admin Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    let slug = created["slug"].as_str().unwrap().to_string();

    let other = non_admin_client(&app, &admin_client, "non-owner-update@test.local").await;

    let resp = other
        .post(format!("{}/api/dashboards/{slug}", app.address))
        .json(&serde_json::json!({ "name": "Unauthorized Rename" }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert!(
        status == 403 || status == 404,
        "non-owner non-admin update must return 403/404, got {status}"
    );

    let del = other
        .delete(format!("{}/api/dashboards/{slug}", app.address))
        .send()
        .await
        .unwrap();
    let del_status = del.status().as_u16();
    assert!(
        del_status == 403 || del_status == 404,
        "non-owner non-admin archive must return 403/404, got {del_status}"
    );

    let after = ok_json(
        admin_client
            .get(format!("{}/api/dashboards/{slug}", app.address))
            .send()
            .await
            .unwrap(),
        "get dashboard after forbidden writes",
    )
    .await;
    assert_eq!(after["name"], "Admin Dashboard", "name must be unchanged");
    assert_eq!(
        after["is_archived"], false,
        "dashboard must not be archived"
    );

    let anon = reqwest::Client::builder().build().unwrap();
    let sess = anon
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = common::extract_set_cookie(sess.headers(), "csrf_token").unwrap_or_default();
    let anon_resp = anon
        .post(format!("{}/api/dashboards/{slug}", app.address))
        .header("X-CSRF-Token", &csrf)
        .header("Cookie", format!("csrf_token={csrf}"))
        .json(&serde_json::json!({ "name": "Unauthorized Rename" }))
        .send()
        .await
        .unwrap();
    let anon_status = anon_resp.status().as_u16();
    assert!(
        anon_status == 401 || anon_status == 403 || anon_status == 404,
        "unauthenticated update must return 401/403/404, got {anon_status}"
    );
}

#[tokio::test]
async fn archive_dashboard_removes_from_list() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Archive Me Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard for archive test",
    )
    .await;
    let id = created["id"].as_i64().unwrap();
    let slug = created["slug"].as_str().unwrap().to_string();

    let del = client
        .delete(format!("{}/api/dashboards/{slug}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status().as_u16(), 204, "DELETE must return 204");

    let list: serde_json::Value = client
        .get(format!("{}/api/dashboards", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = list.as_array().expect("list must be array");
    let found = items.iter().any(|d| d["id"].as_i64() == Some(id));
    assert!(
        !found,
        "archived dashboard id={id} must not appear in default list"
    );
}

#[tokio::test]
async fn star_unstar_dashboard() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Favored Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard for favorite test",
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    let star = client
        .post(format!("{}/api/dashboards/{id}/favorite", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(star.status().as_u16(), 204, "star must return 204");

    let favs: serde_json::Value = client
        .get(format!("{}/api/dashboards/favorites", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let fav_arr = favs.as_array().expect("favorites must be array");
    assert!(
        fav_arr.iter().any(|d| d["id"].as_i64() == Some(id)),
        "starred dashboard id={id} must appear in favorites"
    );

    let list: serde_json::Value = client
        .get(format!("{}/api/dashboards", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list_arr = list.as_array().expect("list must be array");
    let in_list = list_arr.iter().find(|d| d["id"].as_i64() == Some(id));
    assert!(in_list.is_some());
    assert_eq!(
        in_list.unwrap()["is_favorite"],
        true,
        "is_favorite must be true after starring"
    );

    let unstar = client
        .delete(format!("{}/api/dashboards/{id}/favorite", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(unstar.status().as_u16(), 204, "unstar must return 204");

    let favs2: serde_json::Value = client
        .get(format!("{}/api/dashboards/favorites", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let fav2_arr = favs2.as_array().expect("favorites must be array");
    assert!(
        !fav2_arr.iter().any(|d| d["id"].as_i64() == Some(id)),
        "unstarred dashboard must not appear in favorites"
    );
}

/// A user with no reach can neither favorite a dashboard nor surface a planted favorite row; a user
/// with reach still can.
#[tokio::test]
async fn favorite_respects_the_access_filter() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;

    let created = ok_json(
        admin_client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Restricted Favorite Target" }))
            .send()
            .await
            .unwrap(),
        "create dashboard for favorite-access test",
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter};
    let mut active: arcodash_core::entities::dashboard::ActiveModel =
        arcodash_core::entities::dashboard::Entity::find_by_id(id)
            .one(&app.db)
            .await
            .unwrap()
            .unwrap()
            .into();
    active.visibility = sea_orm::ActiveValue::Set("restricted".to_string());
    active.update(&app.db).await.unwrap();

    let default_org = arcodash_core::entities::organization::Entity::find()
        .filter(arcodash_core::entities::organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist");
    let viewers_group = arcodash_core::entities::group::Entity::find()
        .filter(arcodash_core::entities::group::Column::OrgId.eq(default_org.id))
        .filter(arcodash_core::entities::group::Column::Name.eq("viewers"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("builtin viewers group must exist");
    let outsider = non_admin_client_in_groups(
        &app,
        &admin_client,
        "no-reach-favorite@test.local",
        &[viewers_group.id],
    )
    .await;

    // Caller has `view_dashboard` but no reach: favorite must 404.
    let star_resp = outsider
        .post(format!("{}/api/dashboards/{id}/favorite", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        star_resp.status().as_u16(),
        404,
        "favoriting a no-reach dashboard must 404"
    );

    // A favorite row planted directly must never surface for the outsider.
    let outsider_row = arcodash_core::entities::user::Entity::find()
        .filter(arcodash_core::entities::user::Column::Email.eq("no-reach-favorite@test.local"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("outsider user must exist");
    arcodash_core::entities::favorite::ActiveModel {
        org_id: sea_orm::ActiveValue::Set(default_org.id),
        object_type: sea_orm::ActiveValue::Set("dashboard".to_string()),
        object_id: sea_orm::ActiveValue::Set(id),
        user_id: sea_orm::ActiveValue::Set(outsider_row.id),
        created_at: sea_orm::ActiveValue::Set(chrono::Utc::now().into()),
        updated_at: sea_orm::ActiveValue::Set(chrono::Utc::now().into()),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();

    let favs = outsider
        .get(format!("{}/api/dashboards/favorites", app.address))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let fav_arr = favs.as_array().expect("favorites must be array");
    assert!(
        !fav_arr.iter().any(|d| d["id"].as_i64() == Some(id)),
        "a planted favorite row for a no-reach dashboard must never surface in the list"
    );

    let reachable = ok_json(
        admin_client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Reachable Favorite Target" }))
            .send()
            .await
            .unwrap(),
        "create reachable dashboard",
    )
    .await;
    let reachable_id = reachable["id"].as_i64().unwrap();
    let reachable_slug = reachable["slug"].as_str().unwrap().to_string();
    // A draft dashboard is invisible to non-owners; publish so the outsider has reach.
    admin_client
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": reachable_id, "text": "tile", "width": 1 }))
        .send()
        .await
        .unwrap();
    let publish_resp = admin_client
        .post(format!(
            "{}/api/dashboards/{reachable_slug}/publish",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        publish_resp.status().as_u16(),
        200,
        "publish reachable dashboard"
    );

    let reachable_star = outsider
        .post(format!(
            "{}/api/dashboards/{reachable_id}/favorite",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        reachable_star.status().as_u16(),
        204,
        "favoriting a reachable dashboard must still succeed"
    );
}

#[tokio::test]
async fn tags_endpoint_returns_dashboard_tags() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Tagged Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard for tags test",
    )
    .await;
    let slug = created["slug"].as_str().unwrap().to_string();

    ok_json(
        client
            .post(format!("{}/api/dashboards/{slug}", app.address))
            .json(&serde_json::json!({ "tags": ["science", "reports"] }))
            .send()
            .await
            .unwrap(),
        "set tags on dashboard",
    )
    .await;

    let resp = ok_json(
        client
            .get(format!("{}/api/dashboards/tags", app.address))
            .send()
            .await
            .unwrap(),
        "get tags",
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

/// Dashboard mutations (create, update, publish, share, revoke) each record an `events` row matched
/// on `action` + `object_type` + the runtime dashboard id.
#[tokio::test]
async fn dashboard_lifecycle_actions_are_audited() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Audited Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard for audit test",
    )
    .await;
    let id = created["id"].as_i64().unwrap();
    let slug = created["slug"].as_str().unwrap().to_string();

    ok_json(
        client
            .post(format!("{}/api/dashboards/{slug}", app.address))
            .json(&serde_json::json!({ "name": "Audited Dashboard (renamed)" }))
            .send()
            .await
            .unwrap(),
        "update dashboard for audit test",
    )
    .await;

    client
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": id, "text": "tile", "width": 1 }))
        .send()
        .await
        .unwrap();
    ok_json(
        client
            .post(format!("{}/api/dashboards/{slug}/publish", app.address))
            .send()
            .await
            .unwrap(),
        "publish dashboard for audit test",
    )
    .await;

    ok_json(
        client
            .post(format!("{}/api/dashboards/{slug}/share", app.address))
            .send()
            .await
            .unwrap(),
        "share dashboard for audit test",
    )
    .await;
    let revoke = client
        .delete(format!("{}/api/dashboards/{slug}/share", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status().as_u16(), 204);

    use arcodash_core::entities::event;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let action_recorded = |action: &'static str| {
        let db = &app.db;
        async move {
            event::Entity::find()
                .filter(event::Column::Action.eq(action))
                .filter(event::Column::ObjectType.eq("dashboard"))
                .filter(event::Column::ObjectId.eq(id))
                .one(db)
                .await
                .unwrap()
                .is_some()
        }
    };
    for action in [
        "dashboard.created",
        "dashboard.updated",
        "dashboard.published",
        "dashboard.share_token_minted",
        "dashboard.link_revoked",
    ] {
        assert!(
            action_recorded(action).await,
            "expected an events row for action={action} object_type=dashboard object_id={id}"
        );
    }

    let updated_event = event::Entity::find()
        .filter(event::Column::Action.eq("dashboard.updated"))
        .filter(event::Column::ObjectType.eq("dashboard"))
        .filter(event::Column::ObjectId.eq(id))
        .one(&app.db)
        .await
        .unwrap()
        .expect("dashboard.updated event");
    let changed = updated_event
        .additional_properties
        .get("changed")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| {
            panic!(
                "dashboard.updated missing changed array: {:?}",
                updated_event.additional_properties
            )
        });
    assert!(
        changed.iter().any(|v| v.as_str() == Some("name")),
        "renaming a dashboard must list \"name\" in dashboard.updated's changed field: {changed:?}"
    );
}

/// Share returns `secret_token` + `public_url`; the anonymous public GET returns the dashboard with
/// widgets.
#[tokio::test]
async fn share_and_public_get() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Public Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard for share test",
    )
    .await;
    let slug = created["slug"].as_str().unwrap().to_string();

    let unpublished_share = client
        .post(format!("{}/api/dashboards/{slug}/share", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unpublished_share.status().as_u16(),
        400,
        "sharing a never-published dashboard must be refused"
    );
    client
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": created["id"], "text": "A tile", "width": 1 }))
        .send()
        .await
        .unwrap();
    ok_json(
        client
            .post(format!("{}/api/dashboards/{slug}/publish", app.address))
            .send()
            .await
            .unwrap(),
        "publish dashboard for share test",
    )
    .await;

    let share_resp = ok_json(
        client
            .post(format!("{}/api/dashboards/{slug}/share", app.address))
            .send()
            .await
            .unwrap(),
        "share dashboard",
    )
    .await;

    let token = share_resp["secret_token"]
        .as_str()
        .expect("share must return secret_token");
    assert!(!token.is_empty(), "secret_token must not be empty");
    assert!(
        share_resp["public_url"].as_str().is_some(),
        "share must return public_url"
    );

    let anon = reqwest::Client::builder().build().unwrap();
    let pub_resp = anon
        .get(format!("{}/api/dashboards/public/{token}", app.address))
        .send()
        .await
        .unwrap();
    let status = pub_resp.status().as_u16();
    let body_text = pub_resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "public_get must return 200; body: {body_text}");
    let pub_body: serde_json::Value = serde_json::from_str(&body_text)
        .unwrap_or_else(|e| panic!("public_get body not JSON ({e}): {body_text}"));
    assert!(
        pub_body["widgets"].is_array(),
        "public dashboard must include widgets array"
    );
}

#[tokio::test]
async fn revoke_share_makes_public_get_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let created = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Revoke Share Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard for revoke test",
    )
    .await;
    let slug = created["slug"].as_str().unwrap().to_string();
    client
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": created["id"], "text": "A tile", "width": 1 }))
        .send()
        .await
        .unwrap();
    ok_json(
        client
            .post(format!("{}/api/dashboards/{slug}/publish", app.address))
            .send()
            .await
            .unwrap(),
        "publish dashboard for revoke test",
    )
    .await;

    let share_resp = ok_json(
        client
            .post(format!("{}/api/dashboards/{slug}/share", app.address))
            .send()
            .await
            .unwrap(),
        "share dashboard for revoke test",
    )
    .await;
    let token = share_resp["secret_token"].as_str().unwrap().to_string();

    let revoke = client
        .delete(format!("{}/api/dashboards/{slug}/share", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status().as_u16(), 204, "revoke must return 204");

    let anon = reqwest::Client::builder().build().unwrap();
    let pub_resp = anon
        .get(format!("{}/api/dashboards/public/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        pub_resp.status().as_u16(),
        404,
        "public_get after revoke must return 404"
    );
}

/// GET /api/dashboards/:slug never echoes the legacy `secret_token` to a plain viewer; the owner's
/// share/revoke round-trip still works.
#[tokio::test]
async fn get_dashboard_never_leaks_share_secret_token() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;

    let created = ok_json(
        admin_client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Token Leak Check" }))
            .send()
            .await
            .unwrap(),
        "create dashboard for token-leak test",
    )
    .await;
    let slug = created["slug"].as_str().unwrap().to_string();
    admin_client
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": created["id"], "text": "A tile", "width": 1 }))
        .send()
        .await
        .unwrap();
    ok_json(
        admin_client
            .post(format!("{}/api/dashboards/{slug}/publish", app.address))
            .send()
            .await
            .unwrap(),
        "publish dashboard for token-leak test",
    )
    .await;

    let share_resp = ok_json(
        admin_client
            .post(format!("{}/api/dashboards/{slug}/share", app.address))
            .send()
            .await
            .unwrap(),
        "share dashboard for token-leak test",
    )
    .await;
    let token = share_resp["secret_token"].as_str().unwrap().to_string();
    assert!(!token.is_empty());

    // A plain viewer must not find the token anywhere in the raw response body.
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let default_org = arcodash_core::entities::organization::Entity::find()
        .filter(arcodash_core::entities::organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist");
    let viewers_group = arcodash_core::entities::group::Entity::find()
        .filter(arcodash_core::entities::group::Column::OrgId.eq(default_org.id))
        .filter(arcodash_core::entities::group::Column::Name.eq("viewers"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("builtin viewers group must exist");
    let viewer = non_admin_client_in_groups(
        &app,
        &admin_client,
        "viewer-token-leak@test.local",
        &[viewers_group.id],
    )
    .await;
    let get_resp = viewer
        .get(format!("{}/api/dashboards/{slug}", app.address))
        .send()
        .await
        .unwrap();
    let status = get_resp.status().as_u16();
    let body_text = get_resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "viewer get dashboard: {body_text}");
    assert!(
        !body_text.contains(&token),
        "GET /api/dashboards/:slug body must not contain the share secret_token anywhere: {body_text}"
    );

    let owner_get = admin_client
        .get(format!("{}/api/dashboards/{slug}", app.address))
        .send()
        .await
        .unwrap();
    let owner_body = owner_get.text().await.unwrap_or_default();
    assert!(
        !owner_body.contains(&token),
        "owner's own GET /api/dashboards/:slug body must not contain the share secret_token either: {owner_body}"
    );

    let anon = reqwest::Client::builder().build().unwrap();
    let pub_resp = anon
        .get(format!("{}/api/dashboards/public/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        pub_resp.status().as_u16(),
        200,
        "public read via token must still work"
    );

    let revoke = admin_client
        .delete(format!("{}/api/dashboards/{slug}/share", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status().as_u16(), 204, "revoke must return 204");

    let pub_resp_after = anon
        .get(format!("{}/api/dashboards/public/{token}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        pub_resp_after.status().as_u16(),
        404,
        "public read after revoke must 404"
    );
}

/// Share/revoke by a non-owner non-admin is rejected (403/404); unauthenticated too.
#[tokio::test]
async fn share_revoke_by_non_owner_is_forbidden() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;

    let created = ok_json(
        admin_client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Auth Check Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard for auth test",
    )
    .await;
    let slug = created["slug"].as_str().unwrap().to_string();

    let other = non_admin_client(&app, &admin_client, "non-owner-share@test.local").await;

    let share_resp = other
        .post(format!("{}/api/dashboards/{slug}/share", app.address))
        .send()
        .await
        .unwrap();
    let status = share_resp.status().as_u16();
    assert!(
        status == 403 || status == 404,
        "non-owner non-admin share must return 403/404, got {status}"
    );

    let revoke_resp = other
        .delete(format!("{}/api/dashboards/{slug}/share", app.address))
        .send()
        .await
        .unwrap();
    let revoke_status = revoke_resp.status().as_u16();
    assert!(
        revoke_status == 403 || revoke_status == 404,
        "non-owner non-admin revoke must return 403/404, got {revoke_status}"
    );

    let anon = reqwest::Client::builder().build().unwrap();
    let sess = anon
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = common::extract_set_cookie(sess.headers(), "csrf_token").unwrap_or_default();
    let anon_share = anon
        .post(format!("{}/api/dashboards/{slug}/share", app.address))
        .header("X-CSRF-Token", &csrf)
        .header("Cookie", format!("csrf_token={csrf}"))
        .send()
        .await
        .unwrap();
    let anon_status = anon_share.status().as_u16();
    assert!(
        anon_status == 401 || anon_status == 403 || anon_status == 404,
        "unauthenticated share must return 401/403/404, got {anon_status}"
    );
}

/// A dashboard tile shows the PUBLISHED SQL/data source, never a pending draft edit, even for the
/// query's owner.
#[tokio::test]
async fn dashboard_tile_shows_published_query_not_pending_draft_edit() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds: serde_json::Value = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "tile-leak-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Tile Query",
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

    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish",
    )
    .await;

    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}", app.address))
            .json(&serde_json::json!({
                "query": "SELECT secret_column FROM pending_draft_table",
                "expected_version": 1
            }))
            .send()
            .await
            .unwrap(),
        "edit draft (unpublished)",
    )
    .await;

    let viz = ok_json(
        admin
            .post(format!("{}/api/visualizations", app.address))
            .json(&serde_json::json!({
                "query_id": query_id,
                "type": "table",
                "name": "Tile Viz",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create visualization",
    )
    .await;
    let viz_id = viz["id"].as_i64().unwrap();

    let dash = ok_json(
        admin
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Tile Leak Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    let dash_id = dash["id"].as_i64().unwrap();
    let dash_slug = dash["slug"].as_str().unwrap().to_string();

    ok_json(
        admin
            .post(format!("{}/api/widgets", app.address))
            .json(&serde_json::json!({
                "dashboard_id": dash_id,
                "visualization_id": viz_id,
                "width": 2
            }))
            .send()
            .await
            .unwrap(),
        "create widget",
    )
    .await;

    // A published dashboard is a viewer surface; publish before asserting served tile content.
    ok_json(
        admin
            .post(format!(
                "{}/api/dashboards/{dash_slug}/publish",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "publish dashboard",
    )
    .await;

    let body = ok_json(
        admin
            .get(format!("{}/api/dashboards/{dash_slug}", app.address))
            .send()
            .await
            .unwrap(),
        "get dashboard",
    )
    .await;

    let widgets = body["widgets"]
        .as_array()
        .expect("widgets must be an array");
    assert_eq!(widgets.len(), 1);
    let tile_query = &widgets[0]["visualization"]["query"];
    assert!(
        tile_query.get("query").is_none(),
        "a widget's query summary must never carry the SQL text at all: {tile_query}"
    );
    assert_eq!(tile_query["data_source_id"], ds_id);
    assert!(
        !body.to_string().contains("pending_draft_table"),
        "the pending draft edit must never reach a dashboard tile read, published or otherwise"
    );
}

/// Deleting a visualization's query cascades the visualization away;
/// `load_published_widgets_with_viz` must prune the ghost tile at read time while the text-only
/// tile survives.
#[tokio::test]
async fn published_dashboard_prunes_ghost_tile_after_its_query_is_deleted() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "ghost-tile-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Ghost Tile Query",
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

    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish query",
    )
    .await;

    let viz = ok_json(
        admin
            .post(format!("{}/api/visualizations", app.address))
            .json(&serde_json::json!({
                "query_id": query_id,
                "type": "table",
                "name": "Ghost Viz",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create visualization",
    )
    .await;
    let viz_id = viz["id"].as_i64().unwrap();

    let dash = ok_json(
        admin
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Ghost Tile Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    let dash_id = dash["id"].as_i64().unwrap();
    let dash_slug = dash["slug"].as_str().unwrap().to_string();

    ok_json(
        admin
            .post(format!("{}/api/widgets", app.address))
            .json(&serde_json::json!({
                "dashboard_id": dash_id,
                "visualization_id": viz_id,
                "width": 2
            }))
            .send()
            .await
            .unwrap(),
        "create viz widget",
    )
    .await;
    ok_json(
        admin
            .post(format!("{}/api/widgets", app.address))
            .json(&serde_json::json!({
                "dashboard_id": dash_id,
                "text": "Surviving text tile",
                "width": 1
            }))
            .send()
            .await
            .unwrap(),
        "create text widget",
    )
    .await;

    ok_json(
        admin
            .post(format!(
                "{}/api/dashboards/{dash_slug}/publish",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "publish dashboard",
    )
    .await;

    let before = ok_json(
        admin
            .get(format!("{}/api/dashboards/{dash_slug}", app.address))
            .send()
            .await
            .unwrap(),
        "get published dashboard before delete",
    )
    .await;
    assert_eq!(before["widgets"].as_array().unwrap().len(), 2);

    let del = admin
        .delete(format!("{}/api/queries/{query_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status().as_u16(), 204);

    let after = ok_json(
        admin
            .get(format!("{}/api/dashboards/{dash_slug}", app.address))
            .send()
            .await
            .unwrap(),
        "get published dashboard after delete",
    )
    .await;
    let widgets = after["widgets"].as_array().unwrap();
    assert_eq!(
        widgets.len(),
        1,
        "the ghost viz tile must be pruned, leaving only the text tile: {widgets:#?}"
    );
    assert_eq!(widgets[0]["visualization_id"], serde_json::Value::Null);
    assert_eq!(widgets[0]["text"], "Surviving text tile");
}

/// The raw dashboard body never contains widget SQL, for an editor or a zero-permission `viewers`
/// member.
#[tokio::test]
async fn get_dashboard_body_never_contains_widget_sql() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "no-sql-leak-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let distinctive_sql = "SELECT totally_distinctive_column_9f3a FROM widget_source_table";
    let q = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "No SQL Leak Query",
                "query": distinctive_sql,
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query",
    )
    .await;
    let query_id = q["id"].as_i64().unwrap();
    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish query",
    )
    .await;

    let viz = ok_json(
        admin
            .post(format!("{}/api/visualizations", app.address))
            .json(&serde_json::json!({
                "query_id": query_id,
                "type": "table",
                "name": "No SQL Leak Viz",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create visualization",
    )
    .await;
    let viz_id = viz["id"].as_i64().unwrap();

    let dash = ok_json(
        admin
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "No SQL Leak Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    let dash_id = dash["id"].as_i64().unwrap();
    let dash_slug = dash["slug"].as_str().unwrap().to_string();

    ok_json(
        admin
            .post(format!("{}/api/widgets", app.address))
            .json(&serde_json::json!({ "dashboard_id": dash_id, "visualization_id": viz_id, "width": 2 }))
            .send()
            .await
            .unwrap(),
        "create widget",
    )
    .await;
    ok_json(
        admin
            .post(format!(
                "{}/api/dashboards/{dash_slug}/publish",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "publish dashboard",
    )
    .await;

    let editor_resp = admin
        .get(format!("{}/api/dashboards/{dash_slug}", app.address))
        .send()
        .await
        .unwrap();
    let editor_body = editor_resp.text().await.unwrap_or_default();
    assert!(
        !editor_body.contains("totally_distinctive_column_9f3a"),
        "an editor's GET must not carry the widget SQL either: {editor_body}"
    );

    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let default_org = arcodash_core::entities::organization::Entity::find()
        .filter(arcodash_core::entities::organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist");
    let viewers_group = arcodash_core::entities::group::Entity::find()
        .filter(arcodash_core::entities::group::Column::OrgId.eq(default_org.id))
        .filter(arcodash_core::entities::group::Column::Name.eq("viewers"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("builtin viewers group must exist");
    let viewer = non_admin_client_in_groups(
        &app,
        &admin,
        "viewer-no-sql-leak@test.local",
        &[viewers_group.id],
    )
    .await;
    let viewer_resp = viewer
        .get(format!("{}/api/dashboards/{dash_slug}", app.address))
        .send()
        .await
        .unwrap();
    let status = viewer_resp.status().as_u16();
    let viewer_body = viewer_resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "viewer get dashboard: {viewer_body}");
    assert!(
        !viewer_body.contains("totally_distinctive_column_9f3a"),
        "a zero-permission viewer's GET must not carry the widget SQL: {viewer_body}"
    );
}

/// A widget's query summary carries `parameter_names` matching every `{{token}}` in its served SQL.
#[tokio::test]
async fn dashboard_widget_query_summary_carries_parameter_names() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "param-names-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Param Names Query",
                "query": "SELECT * FROM t WHERE region = {{ region }} AND d BETWEEN {{ d.start }} AND {{ d.end }}",
                "options": {
                    "parameters": [
                        { "name": "region", "title": "Region", "type": "text", "value": "us" },
                        { "name": "d", "title": "Date range", "type": "date-range" }
                    ]
                }
            }))
            .send()
            .await
            .unwrap(),
        "create parameterized query",
    )
    .await;
    let query_id = q["id"].as_i64().unwrap();
    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish query",
    )
    .await;

    let viz = ok_json(
        admin
            .post(format!("{}/api/visualizations", app.address))
            .json(&serde_json::json!({
                "query_id": query_id,
                "type": "table",
                "name": "Param Names Viz",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create visualization",
    )
    .await;
    let viz_id = viz["id"].as_i64().unwrap();

    let dash = ok_json(
        admin
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Param Names Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    let dash_id = dash["id"].as_i64().unwrap();
    let dash_slug = dash["slug"].as_str().unwrap().to_string();

    ok_json(
        admin
            .post(format!("{}/api/widgets", app.address))
            .json(&serde_json::json!({ "dashboard_id": dash_id, "visualization_id": viz_id, "width": 2 }))
            .send()
            .await
            .unwrap(),
        "create widget",
    )
    .await;
    ok_json(
        admin
            .post(format!(
                "{}/api/dashboards/{dash_slug}/publish",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "publish dashboard",
    )
    .await;

    let body = ok_json(
        admin
            .get(format!("{}/api/dashboards/{dash_slug}", app.address))
            .send()
            .await
            .unwrap(),
        "get dashboard",
    )
    .await;
    let widgets = body["widgets"].as_array().unwrap();
    assert_eq!(widgets.len(), 1);
    let parameter_names = widgets[0]["visualization"]["query"]["parameter_names"]
        .as_array()
        .expect("parameter_names must be an array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        parameter_names,
        vec!["d".to_string(), "region".to_string()],
        "parameter_names must match every {{{{token}}}} in the served SQL, sorted: {parameter_names:?}"
    );
}

/// A viewer whose group can't reach a query's data source must not be told its data is readable;
/// the owner is the contrasting `true` case.
#[tokio::test]
async fn viewer_can_read_data_reflects_data_source_restriction() {
    use arcodash_backend::services::dashboard_service;
    use arcodash_core::entities::{change, group, visualization, widget};
    use arcodash_core::test_support::{
        new_data_source, new_query, seed_published, seed_published_dashboard,
    };
    use sea_orm::{ActiveModelTrait, ActiveValue::NotSet, ActiveValue::Set, EntityTrait};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let org_id = common::make_org(&app.db).await;
    let (owner, _) =
        common::user_with_role(&app.db, org_id, &["view_query", "view_dashboard"]).await;
    let (viewer, _) =
        common::user_with_role(&app.db, org_id, &["view_query", "view_dashboard"]).await;

    // Restrict the viewer's group with no allowlist row, so its only reach is the org-open default
    // (gated on data-source access).
    let viewer_group_id = viewer.groups[0];
    let g = group::Entity::find_by_id(viewer_group_id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("viewer group not found");
    let mut active: group::ActiveModel = g.into();
    active.data_source_access_restricted = Set(true);
    active.update(&app.db).await.expect("group restrict failed");

    let ds = new_data_source(org_id)
        .insert(&app.db)
        .await
        .expect("data source insert failed");

    // Insert as a draft, then publish via a real `changes` row; the org-open default only applies
    // to published queries.
    let query = new_query(org_id, ds.id, owner.id)
        .insert(&app.db)
        .await
        .expect("query insert failed");
    let change = change::ActiveModel {
        id: NotSet,
        object_type: Set("query".to_string()),
        object_id: Set(query.id),
        user_id: Set(owner.id),
        change: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    }
    .insert(&app.db)
    .await
    .expect("change insert failed");
    let query = seed_published(query.into(), Some(change.id))
        .update(&app.db)
        .await
        .expect("query publish update failed");

    let viz = visualization::ActiveModel {
        id: NotSet,
        query_id: Set(query.id),
        r#type: Set("table".to_string()),
        name: Set("Restricted DS Viz".to_string()),
        description: Set(None),
        options: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    }
    .insert(&app.db)
    .await
    .expect("visualization insert failed");

    let widget_am = widget::ActiveModel {
        id: NotSet,
        dashboard_id: NotSet, // assigned by seed_published_dashboard
        visualization_id: Set(Some(viz.id)),
        text: Set(None),
        width: Set(1),
        options: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    };
    seed_published_dashboard(&app.db, org_id, owner.id, vec![widget_am]).await;

    let access = dashboard_service::load_viewer_query_access(&app.db, &owner, &[query.id])
        .await
        .expect("load_viewer_query_access (owner) failed");
    assert_eq!(
        access.get(&query.id).copied(),
        Some(true),
        "the query's owner must be able to read its own widget's data regardless of the \
         data-source restriction on any other group"
    );

    let access = dashboard_service::load_viewer_query_access(&app.db, &viewer, &[query.id])
        .await
        .expect("load_viewer_query_access (viewer) failed");
    assert_eq!(
        access.get(&query.id).copied(),
        Some(false),
        "a viewer whose group can't reach the widget's data source must not be told the data \
         is readable, even though the workspace-visible query itself stays visible"
    );
}

/// A widget with `options.monitors=[id]` carries that monitor's tile in `widgets[].monitors`.
#[tokio::test]
async fn dashboard_widget_serves_monitor_tiles_with_state_and_name() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "mon-tile-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Monitor Tile Query",
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
    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish query",
    )
    .await;

    let monitor = ok_json(
        admin
            .post(format!("{}/api/monitors", app.address))
            .json(&serde_json::json!({
                "query_id": query_id,
                "name": "Watched Monitor",
                "options": { "column": "value", "op": ">", "value": 5.0 }
            }))
            .send()
            .await
            .unwrap(),
        "create monitor",
    )
    .await;
    let monitor_id = monitor["id"].as_i64().unwrap();

    let dash = ok_json(
        admin
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Monitor Tile Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    let dash_id = dash["id"].as_i64().unwrap();
    let dash_slug = dash["slug"].as_str().unwrap().to_string();

    ok_json(
        admin
            .post(format!("{}/api/widgets", app.address))
            .json(&serde_json::json!({
                "dashboard_id": dash_id,
                "text": "monitor widget",
                "width": 1,
                "options": { "monitors": [monitor_id] }
            }))
            .send()
            .await
            .unwrap(),
        "create widget with monitor tile",
    )
    .await;

    let body = ok_json(
        admin
            .get(format!("{}/api/dashboards/{dash_slug}", app.address))
            .send()
            .await
            .unwrap(),
        "get dashboard",
    )
    .await;

    let monitors = body["widgets"][0]["monitors"]
        .as_array()
        .expect("monitors must be an array");
    assert_eq!(monitors.len(), 1);
    assert_eq!(monitors[0]["id"], monitor_id);
    assert_eq!(monitors[0]["name"], "Watched Monitor");
    assert!(monitors[0]["state"].is_string());
}

/// A monitor the viewer has no grant on is absent from the widget's tiles while the widget still
/// serializes.
#[tokio::test]
async fn dashboard_widget_drops_monitors_the_viewer_has_no_grant_on() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let ds = ok_json(
        admin
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "mon-tile-priv-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        admin
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Private Monitor Query",
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
    ok_json(
        admin
            .post(format!("{}/api/queries/{query_id}/publish", app.address))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish query",
    )
    .await;

    let monitor = ok_json(
        admin
            .post(format!("{}/api/monitors", app.address))
            .json(&serde_json::json!({
                "query_id": query_id,
                "name": "Admin-Only Monitor",
                "options": { "column": "value", "op": ">", "value": 5.0 }
            }))
            .send()
            .await
            .unwrap(),
        "create monitor",
    )
    .await;
    let monitor_id = monitor["id"].as_i64().unwrap();

    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter};
    let mut active: arcodash_core::entities::monitor::ActiveModel =
        arcodash_core::entities::monitor::Entity::find_by_id(monitor_id)
            .one(&app.db)
            .await
            .unwrap()
            .unwrap()
            .into();
    active.visibility = sea_orm::ActiveValue::Set("restricted".to_string());
    active.update(&app.db).await.unwrap();

    let dash = ok_json(
        admin
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Private Monitor Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    let dash_id = dash["id"].as_i64().unwrap();
    let dash_slug = dash["slug"].as_str().unwrap().to_string();

    ok_json(
        admin
            .post(format!("{}/api/widgets", app.address))
            .json(&serde_json::json!({
                "dashboard_id": dash_id,
                "text": "monitor widget",
                "width": 1,
                "options": { "monitors": [monitor_id] }
            }))
            .send()
            .await
            .unwrap(),
        "create widget with monitor tile",
    )
    .await;
    let publish_resp = admin
        .post(format!(
            "{}/api/dashboards/{dash_slug}/publish",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(publish_resp.status().as_u16(), 200, "publish dashboard");

    let default_org = arcodash_core::entities::organization::Entity::find()
        .filter(arcodash_core::entities::organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist");
    let viewers_group = arcodash_core::entities::group::Entity::find()
        .filter(arcodash_core::entities::group::Column::OrgId.eq(default_org.id))
        .filter(arcodash_core::entities::group::Column::Name.eq("viewers"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("builtin viewers group must exist");
    let viewer = non_admin_client_in_groups(
        &app,
        &admin,
        "no-reach-monitor-tile@test.local",
        &[viewers_group.id],
    )
    .await;

    let body = ok_json(
        viewer
            .get(format!("{}/api/dashboards/{dash_slug}", app.address))
            .send()
            .await
            .unwrap(),
        "get dashboard as viewer",
    )
    .await;

    assert_eq!(body["widgets"].as_array().unwrap().len(), 1);
    let monitors = body["widgets"][0]["monitors"]
        .as_array()
        .expect("monitors key must still be an array, just empty");
    assert!(
        monitors.is_empty(),
        "a monitor the viewer has no grant on must be dropped, not surfaced: {monitors:?}"
    );
}

/// Missing, empty or garbage `options.monitors` must never 500; the widget still serializes.
#[tokio::test]
async fn dashboard_widget_monitors_garbage_or_empty_never_500s() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let dash = ok_json(
        admin
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Garbage Monitors Dashboard" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    let dash_id = dash["id"].as_i64().unwrap();
    let dash_slug = dash["slug"].as_str().unwrap().to_string();

    ok_json(
        admin
            .post(format!("{}/api/widgets", app.address))
            .json(&serde_json::json!({
                "dashboard_id": dash_id, "text": "no-key", "width": 1, "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create widget with no monitors key",
    )
    .await;

    ok_json(
        admin
            .post(format!("{}/api/widgets", app.address))
            .json(&serde_json::json!({
                "dashboard_id": dash_id, "text": "empty", "width": 1,
                "options": { "monitors": [] }
            }))
            .send()
            .await
            .unwrap(),
        "create widget with empty monitors array",
    )
    .await;

    ok_json(
        admin
            .post(format!("{}/api/widgets", app.address))
            .json(&serde_json::json!({
                "dashboard_id": dash_id, "text": "garbage", "width": 1,
                "options": { "monitors": "not-an-array" }
            }))
            .send()
            .await
            .unwrap(),
        "create widget with garbage monitors value",
    )
    .await;

    let resp = admin
        .get(format!("{}/api/dashboards/{dash_slug}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "must never 500 on bad options.monitors"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    let widgets = body["widgets"].as_array().unwrap();
    assert_eq!(widgets.len(), 3);
    for w in widgets {
        // Absent (JSON null) or empty is acceptable, never a non-empty list.
        assert!(
            w["monitors"].is_null() || w["monitors"].as_array().is_some_and(|a| a.is_empty()),
            "widget {:?} must have no monitor tiles: {:?}",
            w["text"],
            w["monitors"]
        );
    }
}

mod check_summary {
    use super::{non_admin_client_in_groups, ok_json, spawn_app, TestApp};
    use crate::skip_if_none;
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, QueryFilter,
        Statement,
    };

    /// Published query with a viz widget on a published dashboard and one monitor on the query.
    /// Returns `(dashboard slug, monitor id)`.
    async fn dashboard_with_monitored_widget(
        app: &TestApp,
        admin: &reqwest::Client,
    ) -> (String, i64) {
        let ds_id = ok_json(
            admin
                .post(format!("{}/api/data_sources", app.address))
                .json(
                    &serde_json::json!({ "name": "check-summary-ds", "type": "pg", "options": {} }),
                )
                .send()
                .await
                .unwrap(),
            "create data source",
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let query_id = ok_json(
            admin
                .post(format!("{}/api/queries", app.address))
                .json(&serde_json::json!({
                    "data_source_id": ds_id,
                    "name": "Check Summary Query",
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
            admin
                .post(format!("{}/api/queries/{query_id}/publish", app.address))
                .json(&serde_json::json!({ "expected_version": 0 }))
                .send()
                .await
                .unwrap(),
            "publish query",
        )
        .await;
        let viz_id = ok_json(
            admin
                .post(format!("{}/api/visualizations", app.address))
                .json(&serde_json::json!({
                    "query_id": query_id,
                    "type": "table",
                    "name": "Check Summary Viz",
                    "options": {}
                }))
                .send()
                .await
                .unwrap(),
            "create visualization",
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let monitor_id = ok_json(
            admin
                .post(format!("{}/api/monitors", app.address))
                .json(&serde_json::json!({
                    "query_id": query_id,
                    "name": "Check Summary Monitor",
                    "options": { "column": "value", "op": ">", "value": 5.0 }
                }))
                .send()
                .await
                .unwrap(),
            "create monitor",
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let dash = ok_json(
            admin
                .post(format!("{}/api/dashboards", app.address))
                .json(&serde_json::json!({ "name": "Check Summary Dashboard" }))
                .send()
                .await
                .unwrap(),
            "create dashboard",
        )
        .await;
        let dash_id = dash["id"].as_i64().unwrap();
        let slug = dash["slug"].as_str().unwrap().to_string();
        ok_json(
            admin
                .post(format!("{}/api/widgets", app.address))
                .json(&serde_json::json!({
                    "dashboard_id": dash_id,
                    "visualization_id": viz_id,
                    "width": 1
                }))
                .send()
                .await
                .unwrap(),
            "create widget",
        )
        .await;
        let publish = admin
            .post(format!("{}/api/dashboards/{slug}/publish", app.address))
            .send()
            .await
            .unwrap();
        assert_eq!(publish.status().as_u16(), 200, "publish dashboard");
        (slug, monitor_id)
    }

    async fn set_state(app: &TestApp, monitor_id: i64, state: &str) {
        app.db
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE monitors SET state = $1 WHERE id = $2",
                [state.into(), monitor_id.into()],
            ))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn widget_carries_check_summary() {
        let app = skip_if_none!(spawn_app().await);
        app.cleanup().await;
        let admin = app.admin_client().await;
        let (slug, monitor_id) = dashboard_with_monitored_widget(&app, &admin).await;
        set_state(&app, monitor_id, "triggered").await;

        let body = ok_json(
            admin
                .get(format!("{}/api/dashboards/{slug}", app.address))
                .send()
                .await
                .unwrap(),
            "get dashboard",
        )
        .await;
        assert_eq!(
            body["widgets"][0]["check_summary"],
            serde_json::json!({ "total": 1, "ok": 0, "triggered": 1, "unevaluable": 0 })
        );
    }

    #[tokio::test]
    async fn check_summary_respects_monitor_visibility() {
        let app = skip_if_none!(spawn_app().await);
        app.cleanup().await;
        let admin = app.admin_client().await;
        let (slug, monitor_id) = dashboard_with_monitored_widget(&app, &admin).await;
        set_state(&app, monitor_id, "triggered").await;

        let mut active: arcodash_core::entities::monitor::ActiveModel =
            arcodash_core::entities::monitor::Entity::find_by_id(monitor_id)
                .one(&app.db)
                .await
                .unwrap()
                .unwrap()
                .into();
        active.visibility = sea_orm::ActiveValue::Set("restricted".to_string());
        active.update(&app.db).await.unwrap();

        let default_org = arcodash_core::entities::organization::Entity::find()
            .filter(arcodash_core::entities::organization::Column::Slug.eq("default"))
            .one(&app.db)
            .await
            .unwrap()
            .expect("default org must exist");
        let viewers = arcodash_core::entities::group::Entity::find()
            .filter(arcodash_core::entities::group::Column::OrgId.eq(default_org.id))
            .filter(arcodash_core::entities::group::Column::Name.eq("viewers"))
            .one(&app.db)
            .await
            .unwrap()
            .expect("builtin viewers group must exist");
        let viewer = non_admin_client_in_groups(
            &app,
            &admin,
            "check-summary-viewer@test.local",
            &[viewers.id],
        )
        .await;

        let as_viewer = ok_json(
            viewer
                .get(format!("{}/api/dashboards/{slug}", app.address))
                .send()
                .await
                .unwrap(),
            "get dashboard as viewer",
        )
        .await;
        assert_eq!(as_viewer["widgets"].as_array().unwrap().len(), 1);
        assert!(
            as_viewer["widgets"][0].get("check_summary").is_none(),
            "a monitor the viewer cannot see must not be counted: {}",
            as_viewer["widgets"][0]
        );

        let as_admin = ok_json(
            admin
                .get(format!("{}/api/dashboards/{slug}", app.address))
                .send()
                .await
                .unwrap(),
            "get dashboard as admin",
        )
        .await;
        assert_eq!(as_admin["widgets"][0]["check_summary"]["total"], 1);
    }
}

mod freshness {
    use super::{ok_json, spawn_app};
    use crate::skip_if_none;
    use arcodash_backend::services::dashboard_service;
    use arcodash_core::entities::{query, query_result, run, schedule};
    use chrono::{DateTime, Duration, Utc};
    use sea_orm::{
        ActiveModelTrait,
        ActiveValue::{NotSet, Set},
        DatabaseConnection, EntityTrait,
    };

    async fn create_ds(client: &reqwest::Client, base: &str) -> i64 {
        ok_json(
            client
                .post(format!("{base}/api/data_sources"))
                .json(&serde_json::json!({ "name": "freshness-ds", "type": "pg", "options": {} }))
                .send()
                .await
                .unwrap(),
            "create data source",
        )
        .await["id"]
            .as_i64()
            .unwrap()
    }

    async fn create_query(client: &reqwest::Client, base: &str, ds_id: i64) -> i64 {
        ok_json(
            client
                .post(format!("{base}/api/queries"))
                .json(&serde_json::json!({
                    "data_source_id": ds_id,
                    "name": "Freshness Test Query",
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
            .unwrap()
    }

    async fn default_org_id(db: &DatabaseConnection) -> i64 {
        use arcodash_core::entities::organization;
        use sea_orm::{ColumnTrait, QueryFilter};
        organization::Entity::find()
            .filter(organization::Column::Slug.eq("default"))
            .one(db)
            .await
            .unwrap()
            .unwrap()
            .id
    }

    async fn seed_query_result_at(
        db: &DatabaseConnection,
        org_id: i64,
        ds_id: i64,
        retrieved_at: DateTime<Utc>,
    ) -> i64 {
        let now = Utc::now();
        query_result::ActiveModel {
            id: NotSet,
            org_id: Set(org_id),
            data_source_id: Set(ds_id),
            query_hash: Set(format!("hash-{}", uuid::Uuid::new_v4().simple())),
            query: Set("SELECT 1".to_string()),
            data: Set(Some(
                serde_json::json!({ "columns": [{ "name": "val" }], "rows": [{ "val": 1 }] }),
            )),
            data_location: Set(None),
            data_size_bytes: Set(None),
            runtime: Set(Some(0.01)),
            retrieved_at: Set(retrieved_at.into()),
            created_at: Set(now.into()),
            updated_at: Set(now.into()),
        }
        .insert(db)
        .await
        .unwrap()
        .id
    }

    async fn set_query_result_pointer(db: &DatabaseConnection, query_id: i64, qr_id: i64) {
        let q = query::Entity::find_by_id(query_id)
            .one(db)
            .await
            .unwrap()
            .unwrap();
        let mut active: query::ActiveModel = q.into();
        active.latest_query_data_id = Set(Some(qr_id));
        active.update(db).await.unwrap();
    }

    async fn seed_query_run(
        db: &DatabaseConnection,
        org_id: i64,
        query_id: i64,
        status: &str,
        finished_at: Option<DateTime<Utc>>,
    ) -> i64 {
        let now = Utc::now();
        run::ActiveModel {
            id: NotSet,
            org_id: Set(org_id),
            entity_type: Set("query".to_string()),
            entity_id: Set(query_id),
            schedule_id: Set(None),
            data_source_id: Set(None),
            scheduled_for: Set(None),
            triggered_by: Set("schedule".to_string()),
            status: Set(status.to_string()),
            waiting_on_run_id: Set(None),
            job_id: Set(None),
            worker_id: Set(None),
            lease_expires_at: Set(None),
            started_at: Set(Some(now.into())),
            finished_at: Set(finished_at.map(Into::into)),
            duration_ms: Set(Some(10)),
            row_count: Set(Some(1)),
            error: Set(None),
            data_age_secs: Set(None),
            query_result_id: Set(None),
            condition_met: Set(None),
            created_at: Set(now.into()),
            enqueued_at: Set(None),
        }
        .insert(db)
        .await
        .unwrap()
        .id
    }

    #[allow(clippy::too_many_arguments)]
    async fn seed_query_schedule(
        db: &DatabaseConnection,
        org_id: i64,
        query_id: i64,
        spec: serde_json::Value,
        enabled: bool,
        paused_until: Option<DateTime<Utc>>,
        next_run_at: Option<DateTime<Utc>>,
    ) -> i64 {
        let now = Utc::now();
        schedule::ActiveModel {
            org_id: Set(org_id),
            entity_type: Set("query".to_string()),
            entity_id: Set(query_id),
            spec: Set(spec),
            timezone: Set("UTC".to_string()),
            enabled: Set(enabled),
            paused_until: Set(paused_until.map(Into::into)),
            auto_paused_at: Set(None),
            overlap_policy: Set("skip".to_string()),
            next_run_at: Set(next_run_at.map(Into::into)),
            consecutive_failures: Set(0),
            created_at: Set(now.into()),
            updated_at: Set(now.into()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn freshness_present_for_a_scheduled_query_with_a_good_result() {
        let app = skip_if_none!(spawn_app().await);
        app.cleanup().await;
        let client = app.admin_client().await;
        let org_id = default_org_id(&app.db).await;

        let ds_id = create_ds(&client, &app.address).await;
        let query_id = create_query(&client, &app.address, ds_id).await;

        let retrieved_at = Utc::now() - Duration::minutes(5);
        let qr_id = seed_query_result_at(&app.db, org_id, ds_id, retrieved_at).await;
        set_query_result_pointer(&app.db, query_id, qr_id).await;
        seed_query_run(&app.db, org_id, query_id, "success", Some(retrieved_at)).await;
        seed_query_schedule(
            &app.db,
            org_id,
            query_id,
            serde_json::json!({ "kind": "interval", "every_secs": 900 }),
            true,
            None,
            Some(Utc::now() + Duration::minutes(10)),
        )
        .await;

        let map = dashboard_service::load_widget_freshness(&app.db, &[query_id])
            .await
            .unwrap();
        let fr = map.get(&query_id).expect("freshness must be present");
        assert_eq!(fr.last_run_status, "success");
        assert_eq!(fr.expected_interval_secs, Some(900));
        assert_eq!(
            fr.retrieved_at.timestamp_micros(),
            retrieved_at.timestamp_micros(),
            "retrieved_at must equal the serving result's own timestamp"
        );
    }

    /// A newer FAILED run must not blank the tile or move `retrieved_at` off the older good result.
    #[tokio::test]
    async fn last_run_status_diverges_from_retrieved_at_when_newest_run_failed() {
        let app = skip_if_none!(spawn_app().await);
        app.cleanup().await;
        let client = app.admin_client().await;
        let org_id = default_org_id(&app.db).await;

        let ds_id = create_ds(&client, &app.address).await;
        let query_id = create_query(&client, &app.address, ds_id).await;

        let good_retrieved_at = Utc::now() - Duration::hours(3);
        let qr_id = seed_query_result_at(&app.db, org_id, ds_id, good_retrieved_at).await;
        set_query_result_pointer(&app.db, query_id, qr_id).await;
        // Older success run, then a newer failed run with no result; the pointer must not move.
        seed_query_run(
            &app.db,
            org_id,
            query_id,
            "success",
            Some(good_retrieved_at),
        )
        .await;
        seed_query_run(&app.db, org_id, query_id, "failed", Some(Utc::now())).await;

        let map = dashboard_service::load_widget_freshness(&app.db, &[query_id])
            .await
            .unwrap();
        let fr = map.get(&query_id).expect("freshness must be present");
        assert_eq!(
            fr.last_run_status, "failed",
            "the newest run's failure must be reported"
        );
        assert_eq!(
            fr.retrieved_at.timestamp_micros(),
            good_retrieved_at.timestamp_micros(),
            "retrieved_at must still be the last-GOOD result — a failed run has none of its own \
             and must never blank or move the pointer"
        );
    }

    /// A timed-out run reports its own status, not "success".
    #[tokio::test]
    async fn last_run_status_reports_timed_out_rather_than_collapsing_to_success() {
        let app = skip_if_none!(spawn_app().await);
        app.cleanup().await;
        let client = app.admin_client().await;
        let org_id = default_org_id(&app.db).await;

        let ds_id = create_ds(&client, &app.address).await;
        let query_id = create_query(&client, &app.address, ds_id).await;

        let good_retrieved_at = Utc::now() - Duration::hours(3);
        let qr_id = seed_query_result_at(&app.db, org_id, ds_id, good_retrieved_at).await;
        set_query_result_pointer(&app.db, query_id, qr_id).await;
        seed_query_run(
            &app.db,
            org_id,
            query_id,
            "success",
            Some(good_retrieved_at),
        )
        .await;
        seed_query_run(&app.db, org_id, query_id, "timed_out", Some(Utc::now())).await;

        let map = dashboard_service::load_widget_freshness(&app.db, &[query_id])
            .await
            .unwrap();
        let fr = map.get(&query_id).expect("freshness must be present");
        assert_eq!(
            fr.last_run_status, "timed_out",
            "a timed-out newest run must be reported as-is, not normalized to \"success\""
        );
    }

    /// `cancelled` and `skipped` runs pass through untouched.
    #[tokio::test]
    async fn last_run_status_passes_through_cancelled_and_skipped() {
        let app = skip_if_none!(spawn_app().await);
        app.cleanup().await;
        let client = app.admin_client().await;
        let org_id = default_org_id(&app.db).await;

        for status in ["cancelled", "skipped"] {
            let ds_id = create_ds(&client, &app.address).await;
            let query_id = create_query(&client, &app.address, ds_id).await;
            let retrieved_at = Utc::now() - Duration::minutes(5);
            let qr_id = seed_query_result_at(&app.db, org_id, ds_id, retrieved_at).await;
            set_query_result_pointer(&app.db, query_id, qr_id).await;
            seed_query_run(&app.db, org_id, query_id, status, Some(retrieved_at)).await;

            let map = dashboard_service::load_widget_freshness(&app.db, &[query_id])
                .await
                .unwrap();
            let fr = map.get(&query_id).expect("freshness must be present");
            assert_eq!(fr.last_run_status, status);
        }
    }

    #[tokio::test]
    async fn expected_interval_secs_null_for_unscheduled_query() {
        let app = skip_if_none!(spawn_app().await);
        app.cleanup().await;
        let client = app.admin_client().await;
        let org_id = default_org_id(&app.db).await;

        let ds_id = create_ds(&client, &app.address).await;
        let query_id = create_query(&client, &app.address, ds_id).await;

        let qr_id = seed_query_result_at(&app.db, org_id, ds_id, Utc::now()).await;
        set_query_result_pointer(&app.db, query_id, qr_id).await;
        // No schedule or run row: default to "success".

        let map = dashboard_service::load_widget_freshness(&app.db, &[query_id])
            .await
            .unwrap();
        let fr = map.get(&query_id).expect("freshness must be present");
        assert_eq!(fr.expected_interval_secs, None);
        assert_eq!(fr.last_run_status, "success");
    }

    #[tokio::test]
    async fn expected_interval_secs_null_for_disabled_or_paused_schedule() {
        let app = skip_if_none!(spawn_app().await);
        app.cleanup().await;
        let client = app.admin_client().await;
        let org_id = default_org_id(&app.db).await;

        let ds_id = create_ds(&client, &app.address).await;

        let disabled_query_id = create_query(&client, &app.address, ds_id).await;
        let qr1 = seed_query_result_at(&app.db, org_id, ds_id, Utc::now()).await;
        set_query_result_pointer(&app.db, disabled_query_id, qr1).await;
        seed_query_schedule(
            &app.db,
            org_id,
            disabled_query_id,
            serde_json::json!({ "kind": "interval", "every_secs": 300 }),
            false,
            None,
            None,
        )
        .await;

        let paused_query_id = create_query(&client, &app.address, ds_id).await;
        let qr2 = seed_query_result_at(&app.db, org_id, ds_id, Utc::now()).await;
        set_query_result_pointer(&app.db, paused_query_id, qr2).await;
        seed_query_schedule(
            &app.db,
            org_id,
            paused_query_id,
            serde_json::json!({ "kind": "interval", "every_secs": 300 }),
            true,
            Some(Utc::now() + Duration::hours(1)),
            None,
        )
        .await;

        let map = dashboard_service::load_widget_freshness(
            &app.db,
            &[disabled_query_id, paused_query_id],
        )
        .await
        .unwrap();
        assert_eq!(
            map.get(&disabled_query_id).unwrap().expected_interval_secs,
            None
        );
        assert_eq!(
            map.get(&paused_query_id).unwrap().expected_interval_secs,
            None
        );
    }

    /// `cron`/`at` schedules use the gap to the next fire as the cadence proxy; `None` when either
    /// side is missing.
    #[tokio::test]
    async fn expected_interval_secs_gap_for_cron_schedule() {
        let app = skip_if_none!(spawn_app().await);
        app.cleanup().await;
        let client = app.admin_client().await;
        let org_id = default_org_id(&app.db).await;

        let ds_id = create_ds(&client, &app.address).await;
        let query_id = create_query(&client, &app.address, ds_id).await;

        let last_finished = Utc::now() - Duration::hours(24);
        let qr_id = seed_query_result_at(&app.db, org_id, ds_id, last_finished).await;
        set_query_result_pointer(&app.db, query_id, qr_id).await;
        seed_query_run(&app.db, org_id, query_id, "success", Some(last_finished)).await;
        seed_query_schedule(
            &app.db,
            org_id,
            query_id,
            serde_json::json!({ "kind": "cron", "expr": "0 30 9 * * *" }),
            true,
            None,
            Some(Utc::now()),
        )
        .await;

        let map = dashboard_service::load_widget_freshness(&app.db, &[query_id])
            .await
            .unwrap();
        assert_eq!(
            map.get(&query_id).unwrap().expected_interval_secs,
            Some(86_400)
        );
    }

    #[tokio::test]
    async fn no_freshness_when_query_never_produced_a_result() {
        let app = skip_if_none!(spawn_app().await);
        app.cleanup().await;
        let client = app.admin_client().await;

        let ds_id = create_ds(&client, &app.address).await;
        let query_id = create_query(&client, &app.address, ds_id).await;

        let map = dashboard_service::load_widget_freshness(&app.db, &[query_id])
            .await
            .unwrap();
        assert!(
            !map.contains_key(&query_id),
            "a never-executed query must be absent from the freshness map, not a false entry"
        );
    }

    /// One call across several queries returns a correctly-keyed map (batched join).
    #[tokio::test]
    async fn one_query_across_many_widgets() {
        let app = skip_if_none!(spawn_app().await);
        app.cleanup().await;
        let client = app.admin_client().await;
        let org_id = default_org_id(&app.db).await;

        let ds_id = create_ds(&client, &app.address).await;

        let q_a = create_query(&client, &app.address, ds_id).await;
        let qr_a = seed_query_result_at(&app.db, org_id, ds_id, Utc::now()).await;
        set_query_result_pointer(&app.db, q_a, qr_a).await;
        seed_query_schedule(
            &app.db,
            org_id,
            q_a,
            serde_json::json!({ "kind": "interval", "every_secs": 300 }),
            true,
            None,
            None,
        )
        .await;

        let q_b = create_query(&client, &app.address, ds_id).await;
        let qr_b = seed_query_result_at(&app.db, org_id, ds_id, Utc::now()).await;
        set_query_result_pointer(&app.db, q_b, qr_b).await;
        seed_query_schedule(
            &app.db,
            org_id,
            q_b,
            serde_json::json!({ "kind": "interval", "every_secs": 3600 }),
            true,
            None,
            None,
        )
        .await;

        let q_c = create_query(&client, &app.address, ds_id).await;
        let qr_c = seed_query_result_at(&app.db, org_id, ds_id, Utc::now()).await;
        set_query_result_pointer(&app.db, q_c, qr_c).await;

        let map = dashboard_service::load_widget_freshness(&app.db, &[q_a, q_b, q_c])
            .await
            .unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(map.get(&q_a).unwrap().expected_interval_secs, Some(300));
        assert_eq!(map.get(&q_b).unwrap().expected_interval_secs, Some(3600));
        assert_eq!(map.get(&q_c).unwrap().expected_interval_secs, None);
    }

    /// The public dashboard payload carries the same freshness as the authenticated one.
    #[tokio::test]
    async fn public_dashboard_widget_carries_the_same_freshness_as_authenticated() {
        let app = skip_if_none!(spawn_app().await);
        app.cleanup().await;
        let client = app.admin_client().await;
        let org_id = default_org_id(&app.db).await;

        let ds_id = create_ds(&client, &app.address).await;
        let query_id = create_query(&client, &app.address, ds_id).await;

        let retrieved_at = Utc::now() - Duration::minutes(5);
        let qr_id = seed_query_result_at(&app.db, org_id, ds_id, retrieved_at).await;
        set_query_result_pointer(&app.db, query_id, qr_id).await;
        // A widget's visualization requires a published query; first publish copies the draft's
        // result pointer into `published_latest_query_data_id`.
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

        let viz_id = ok_json(
            client
                .post(format!("{}/api/visualizations", app.address))
                .json(&serde_json::json!({
                    "query_id": query_id,
                    "type": "table",
                    "name": "Freshness Viz",
                    "options": {}
                }))
                .send()
                .await
                .unwrap(),
            "create visualization",
        )
        .await["id"]
            .as_i64()
            .unwrap();

        seed_query_run(&app.db, org_id, query_id, "success", Some(retrieved_at)).await;
        seed_query_schedule(
            &app.db,
            org_id,
            query_id,
            serde_json::json!({ "kind": "interval", "every_secs": 900 }),
            true,
            None,
            None,
        )
        .await;

        let dash = ok_json(
            client
                .post(format!("{}/api/dashboards", app.address))
                .json(&serde_json::json!({ "name": "Freshness Public Dashboard" }))
                .send()
                .await
                .unwrap(),
            "create dashboard",
        )
        .await;
        let dash_id = dash["id"].as_i64().unwrap();
        let dash_slug = dash["slug"].as_str().unwrap().to_string();

        ok_json(
            client
                .post(format!("{}/api/widgets", app.address))
                .json(&serde_json::json!({
                    "dashboard_id": dash_id,
                    "visualization_id": viz_id,
                    "width": 2
                }))
                .send()
                .await
                .unwrap(),
            "create widget",
        )
        .await;

        ok_json(
            client
                .post(format!(
                    "{}/api/dashboards/{dash_slug}/publish",
                    app.address
                ))
                .send()
                .await
                .unwrap(),
            "publish dashboard",
        )
        .await;

        let authed_body = ok_json(
            client
                .get(format!("{}/api/dashboards/{dash_slug}", app.address))
                .send()
                .await
                .unwrap(),
            "get dashboard",
        )
        .await;
        let authed_freshness = &authed_body["widgets"][0]["freshness"];
        assert_eq!(authed_freshness["last_run_status"], "success");
        assert_eq!(authed_freshness["expected_interval_secs"], 900);

        let share = ok_json(
            client
                .post(format!("{}/api/dashboards/{dash_slug}/share", app.address))
                .send()
                .await
                .unwrap(),
            "share dashboard",
        )
        .await;
        let token = share["secret_token"].as_str().unwrap().to_string();

        let anon = reqwest::Client::builder().build().unwrap();
        let public_body = ok_json(
            anon.get(format!("{}/api/dashboards/public/{token}", app.address))
                .send()
                .await
                .unwrap(),
            "public get dashboard",
        )
        .await;
        let public_freshness = &public_body["widgets"][0]["freshness"];
        assert_eq!(
            public_freshness["last_run_status"], authed_freshness["last_run_status"],
            "public and authenticated freshness must agree"
        );
        assert_eq!(
            public_freshness["expected_interval_secs"], authed_freshness["expected_interval_secs"],
            "public and authenticated freshness must agree"
        );
        assert_eq!(
            public_freshness["retrieved_at"], authed_freshness["retrieved_at"],
            "public and authenticated freshness must agree"
        );
    }
}

/// `GET /api/dashboards` prefetches caller roles in one batched pass; each row's `caller` block
/// must match a direct `GET /api/dashboards/{slug}` (mixed implicit reach and explicit editor
/// grant).
#[tokio::test]
async fn dashboard_list_caller_blocks_are_unchanged_by_batching() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;

    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let default_org = arcodash_core::entities::organization::Entity::find()
        .filter(arcodash_core::entities::organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("default org must exist");
    let viewers_group = arcodash_core::entities::group::Entity::find()
        .filter(arcodash_core::entities::group::Column::OrgId.eq(default_org.id))
        .filter(arcodash_core::entities::group::Column::Name.eq("viewers"))
        .one(&app.db)
        .await
        .unwrap()
        .expect("builtin viewers group must exist");

    let outsider_email = "mixed-reach-caller@test.local";
    let outsider =
        non_admin_client_in_groups(&app, &admin_client, outsider_email, &[viewers_group.id]).await;
    let outsider_id = arcodash_core::entities::user::Entity::find()
        .filter(arcodash_core::entities::user::Column::Email.eq(outsider_email))
        .one(&app.db)
        .await
        .unwrap()
        .expect("outsider user must exist")
        .id;

    // `can_edit` also needs the `edit_dashboard` capability on top of reach.
    let edit_role = ok_json(
        admin_client
            .post(format!("{}/api/roles", app.address))
            .json(&serde_json::json!({ "name": "Dash Editor", "permissions": ["edit_dashboard"] }))
            .send()
            .await
            .unwrap(),
        "create edit_dashboard role",
    )
    .await;
    let edit_role_id = edit_role["id"].as_i64().unwrap();
    let edit_group = ok_json(
        admin_client
            .post(format!("{}/api/groups", app.address))
            .json(&serde_json::json!({ "name": "dash-editors" }))
            .send()
            .await
            .unwrap(),
        "create dash-editors group",
    )
    .await;
    let edit_group_id = edit_group["id"].as_i64().unwrap();
    let attach = admin_client
        .post(format!("{}/api/groups/{edit_group_id}/roles", app.address))
        .json(&serde_json::json!({ "role_id": edit_role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(attach.status().as_u16(), 200, "attach edit_dashboard role");
    let add_member = admin_client
        .post(format!(
            "{}/api/groups/{edit_group_id}/members",
            app.address
        ))
        .json(&serde_json::json!({ "user_id": outsider_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        add_member.status().as_u16(),
        200,
        "add outsider to dash-editors"
    );

    let workspace_dash = ok_json(
        admin_client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Workspace Reach Dash" }))
            .send()
            .await
            .unwrap(),
        "create workspace dashboard",
    )
    .await;
    let workspace_id = workspace_dash["id"].as_i64().unwrap();
    let workspace_slug = workspace_dash["slug"].as_str().unwrap().to_string();
    admin_client
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": workspace_id, "text": "tile", "width": 1 }))
        .send()
        .await
        .unwrap();
    let publish1 = admin_client
        .post(format!(
            "{}/api/dashboards/{workspace_slug}/publish",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(publish1.status().as_u16(), 200, "publish workspace dash");

    let editor_dash = ok_json(
        admin_client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Editor Grant Dash" }))
            .send()
            .await
            .unwrap(),
        "create editor-grant dashboard",
    )
    .await;
    let editor_id = editor_dash["id"].as_i64().unwrap();
    let editor_slug = editor_dash["slug"].as_str().unwrap().to_string();
    admin_client
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": editor_id, "text": "tile", "width": 1 }))
        .send()
        .await
        .unwrap();
    let publish2 = admin_client
        .post(format!(
            "{}/api/dashboards/{editor_slug}/publish",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(publish2.status().as_u16(), 200, "publish editor-grant dash");
    let grant_resp = admin_client
        .post(format!("{}/api/dashboards/{editor_id}/acl", app.address))
        .json(&serde_json::json!({ "grantee_id": outsider_id, "access_type": "editor" }))
        .send()
        .await
        .unwrap();
    assert_eq!(grant_resp.status().as_u16(), 200, "grant editor acl");

    let list = outsider
        .get(format!("{}/api/dashboards", app.address))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let list_arr = list.as_array().expect("list must be array");

    for (id, slug) in [(workspace_id, &workspace_slug), (editor_id, &editor_slug)] {
        let direct = ok_json(
            outsider
                .get(format!("{}/api/dashboards/{slug}", app.address))
                .send()
                .await
                .unwrap(),
            "direct get dashboard",
        )
        .await;
        let row = list_arr
            .iter()
            .find(|d| d["id"].as_i64() == Some(id))
            .unwrap_or_else(|| panic!("dashboard {id} present in list"));
        assert_eq!(
            direct["caller"], row["caller"],
            "list row caller must match direct GET for dashboard {id}"
        );
    }

    // The two rows' caller blocks must differ, or the equality check passes vacuously.
    let workspace_row = list_arr
        .iter()
        .find(|d| d["id"].as_i64() == Some(workspace_id))
        .unwrap();
    let editor_row = list_arr
        .iter()
        .find(|d| d["id"].as_i64() == Some(editor_id))
        .unwrap();
    assert_ne!(
        workspace_row["caller"]["can_edit"], editor_row["caller"]["can_edit"],
        "workspace-only reach and an explicit editor grant must produce different can_edit values"
    );
}

/// Batched widget loading must report `has_unpublished_changes` correctly in `GET /api/dashboards`:
/// `true` when a published dashboard's draft diverges, `false` otherwise.
#[tokio::test]
async fn dashboard_list_has_unpublished_changes_is_unchanged_by_batching() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let admin_client = app.admin_client().await;

    let dash = ok_json(
        admin_client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Pending Changes Dash" }))
            .send()
            .await
            .unwrap(),
        "create dashboard",
    )
    .await;
    let id = dash["id"].as_i64().unwrap();
    let slug = dash["slug"].as_str().unwrap().to_string();

    admin_client
        .post(format!("{}/api/widgets", app.address))
        .json(&serde_json::json!({ "dashboard_id": id, "text": "original", "width": 1 }))
        .send()
        .await
        .unwrap();
    let publish = admin_client
        .post(format!("{}/api/dashboards/{slug}/publish", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(publish.status().as_u16(), 200, "publish dashboard");

    let list_before = admin_client
        .get(format!("{}/api/dashboards", app.address))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let row_before = list_before
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"].as_i64() == Some(id))
        .expect("dashboard present in list");
    assert_eq!(
        row_before["has_unpublished_changes"],
        serde_json::json!(false),
        "freshly published dashboard has no pending draft changes: {row_before}"
    );

    admin_client
        .post(format!("{}/api/widgets", app.address))
        .json(
            &serde_json::json!({ "dashboard_id": id, "text": "unpublished addition", "width": 1 }),
        )
        .send()
        .await
        .unwrap();

    let list_after = admin_client
        .get(format!("{}/api/dashboards", app.address))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let row_after = list_after
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"].as_i64() == Some(id))
        .expect("dashboard present in list");
    assert_eq!(
        row_after["has_unpublished_changes"],
        serde_json::json!(true),
        "an added widget after publish must show as a pending unpublished change: {row_after}"
    );
}
