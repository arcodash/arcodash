//! Dashboard edit-and-commit: publish/discard as snapshot copies, the draft-vs-published change
//! summary, and the batched layout write.

mod common;
use common::{api_key_client, default_org_id};
use common::{make_org, test_api_key, user_with_role};

use arcodash_backend::authz::authorize::{Action, DashboardVerb};
use arcodash_backend::services::{dashboard_service, permission_service, widget_service};
use arcodash_core::entities::dashboard;
use sea_orm::EntityTrait;
use serde_json::json;

async fn make_dashboard(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    owner: &arcodash_core::entities::user::Model,
    name: &str,
) -> dashboard::Model {
    dashboard_service::create(db, org_id, owner, name.to_string())
        .await
        .expect("create dashboard failed")
}

async fn add_text_widget(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    owner: &arcodash_core::entities::user::Model,
    dashboard_id: i64,
    text: &str,
) -> arcodash_core::entities::widget::Model {
    widget_service::create(
        db,
        org_id,
        owner,
        dashboard_id,
        None,
        Some(text.to_string()),
        1,
        json!({ "position": { "col": 0, "row": 0, "sizeX": 3, "sizeY": 2 } }),
    )
    .await
    .expect("create widget failed")
}

async fn reload(db: &sea_orm::DatabaseConnection, id: i64) -> dashboard::Model {
    dashboard::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn first_publish_fills_snapshot() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["create_dashboard", "edit_dashboard", "view_dashboard"],
    )
    .await;

    let d = make_dashboard(&app.db, org_id, &owner, "Sales Overview").await;
    assert!(
        !dashboard_service::is_published(&d),
        "new dashboard must start never-published"
    );
    add_text_widget(&app.db, org_id, &owner, d.id, "Widget A").await;
    add_text_widget(&app.db, org_id, &owner, d.id, "Widget B").await;

    let published = dashboard_service::publish(&app.db, &owner, d)
        .await
        .expect("publish must succeed with a name + widgets");
    assert!(dashboard_service::is_published(&published));

    let snapshot_json = published.published_snapshot.clone().unwrap();
    let snapshot: dashboard_service::DashboardSnapshot =
        serde_json::from_value(snapshot_json).expect("snapshot must deserialize");
    assert_eq!(snapshot.name, "Sales Overview");
    assert_eq!(snapshot.widgets.len(), 2, "both widgets must be captured");
    let texts: Vec<Option<String>> = snapshot.widgets.iter().map(|w| w.text.clone()).collect();
    assert!(texts.contains(&Some("Widget A".to_string())));
    assert!(texts.contains(&Some("Widget B".to_string())));
}

/// "Publish changes" is a plain overwrite: the old snapshot content is gone.
#[tokio::test]
async fn publish_changes_overwrites_with_no_trace_of_prior_content() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["create_dashboard", "edit_dashboard", "view_dashboard"],
    )
    .await;

    let d = make_dashboard(&app.db, org_id, &owner, "Ops Dashboard").await;
    let old_widget = add_text_widget(&app.db, org_id, &owner, d.id, "Old Tile Content").await;
    let d = dashboard_service::publish(&app.db, &owner, d)
        .await
        .expect("first publish failed");

    widget_service::delete(&app.db, org_id, &owner, old_widget.id)
        .await
        .expect("delete widget failed");
    add_text_widget(&app.db, org_id, &owner, d.id, "New Tile Content").await;
    dashboard_service::update(
        &app.db,
        org_id,
        &owner,
        &d.slug,
        Some("Ops Dashboard Renamed".to_string()),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("rename failed");

    let d = reload(&app.db, d.id).await;
    let republished = dashboard_service::publish(&app.db, &owner, d)
        .await
        .expect("publish changes failed");

    let snapshot: dashboard_service::DashboardSnapshot =
        serde_json::from_value(republished.published_snapshot.clone().unwrap()).unwrap();
    assert_eq!(snapshot.name, "Ops Dashboard Renamed");
    assert_eq!(
        snapshot.widgets.len(),
        1,
        "old widget must be gone, only the new one remains"
    );
    assert_eq!(
        snapshot.widgets[0].text,
        Some("New Tile Content".to_string())
    );
    assert!(
        !snapshot
            .widgets
            .iter()
            .any(|w| w.text.as_deref() == Some("Old Tile Content")),
        "no trace of the prior widget's content may survive a republish"
    );
}

#[tokio::test]
async fn publish_rejected_without_name_or_widgets() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["create_dashboard", "edit_dashboard", "view_dashboard"],
    )
    .await;

    let empty = make_dashboard(&app.db, org_id, &owner, "No Tiles Yet").await;
    let err = dashboard_service::publish(&app.db, &owner, empty)
        .await
        .expect_err("publish with zero widgets must fail");
    assert_eq!(err.status.as_u16(), 400);

    let unnamed = make_dashboard(&app.db, org_id, &owner, "").await;
    add_text_widget(&app.db, org_id, &owner, unnamed.id, "A tile").await;
    let unnamed = reload(&app.db, unnamed.id).await;
    let err = dashboard_service::publish(&app.db, &owner, unnamed)
        .await
        .expect_err("publish with an empty name must fail");
    assert_eq!(err.status.as_u16(), 400);
}

/// Discard rebuilds the live widgets + meta to match the published snapshot.
#[tokio::test]
async fn discard_rebuilds_draft_to_match_snapshot() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["create_dashboard", "edit_dashboard", "view_dashboard"],
    )
    .await;

    let d = make_dashboard(&app.db, org_id, &owner, "Reconcile Me").await;
    let keep = add_text_widget(&app.db, org_id, &owner, d.id, "Keep").await;
    let to_move = add_text_widget(&app.db, org_id, &owner, d.id, "Move Me").await;
    let to_remove = add_text_widget(&app.db, org_id, &owner, d.id, "Remove Me").await;
    let d = dashboard_service::publish(&app.db, &owner, d)
        .await
        .expect("publish failed");

    widget_service::update(
        &app.db,
        org_id,
        &owner,
        to_move.id,
        None,
        None,
        Some(json!({ "position": { "col": 5, "row": 5, "sizeX": 3, "sizeY": 2 } })),
    )
    .await
    .expect("move widget failed");
    widget_service::delete(&app.db, org_id, &owner, to_remove.id)
        .await
        .expect("delete widget failed");
    add_text_widget(&app.db, org_id, &owner, d.id, "Brand New").await;

    let d = reload(&app.db, d.id).await;
    let snapshot: dashboard_service::DashboardSnapshot =
        serde_json::from_value(d.published_snapshot.clone().unwrap()).unwrap();

    let discarded = dashboard_service::discard_changes(&app.db, &owner, d)
        .await
        .expect("discard failed");

    let live = dashboard_service::load_widgets_with_viz(&app.db, discarded.id)
        .await
        .unwrap();
    assert_eq!(
        live.len(),
        snapshot.widgets.len(),
        "widget count must match the snapshot after discard"
    );
    let mut live_texts: Vec<Option<String>> = live.iter().map(|w| w.widget.text.clone()).collect();
    let mut snap_texts: Vec<Option<String>> =
        snapshot.widgets.iter().map(|w| w.text.clone()).collect();
    live_texts.sort();
    snap_texts.sort();
    assert_eq!(
        live_texts, snap_texts,
        "widget set must equal the snapshot's"
    );
    assert!(
        live.iter().any(|w| w.widget.id == keep.id),
        "unrelated widget must survive discard untouched"
    );
}

/// A discard whose snapshot references a since-deleted visualization must recover what it can
/// instead of 500ing on the dead foreign key, repeatedly.
#[tokio::test]
async fn discard_recovers_after_visualization_delete_drops_a_dashboard_widget() {
    use arcodash_backend::services::{query_service, visualization_service};
    use arcodash_core::entities::visualization;
    use arcodash_core::test_support::{new_data_source, new_query};
    use sea_orm::{ActiveModelTrait, ActiveValue::NotSet, ActiveValue::Set};

    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_dashboard",
            "edit_dashboard",
            "view_dashboard",
            "create_query",
            "edit_query",
            "view_query",
        ],
    )
    .await;

    let ds = new_data_source(org_id).insert(&app.db).await.unwrap();
    let mut q = new_query(org_id, ds.id, owner.id);
    q.api_key = Set(format!(
        "discard-recovery-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let q = q.insert(&app.db).await.unwrap();
    let q = query_service::publish(&app.db, &owner, q, 0)
        .await
        .expect("publish query failed");

    let viz = visualization::ActiveModel {
        id: NotSet,
        query_id: Set(q.id),
        r#type: Set("table".to_string()),
        name: Set("Doomed viz".to_string()),
        description: Set(None),
        options: Set(json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    }
    .insert(&app.db)
    .await
    .expect("visualization insert failed");

    let d = make_dashboard(&app.db, org_id, &owner, "Discard Recovery").await;
    let doomed = widget_service::create(
        &app.db,
        org_id,
        &owner,
        d.id,
        Some(viz.id),
        None,
        3,
        json!({ "position": { "col": 0, "row": 0, "sizeX": 3, "sizeY": 2 } }),
    )
    .await
    .expect("create widget on doomed visualization failed");
    let survivor = add_text_widget(&app.db, org_id, &owner, d.id, "Survives").await;

    let d = dashboard_service::publish(&app.db, &owner, d)
        .await
        .expect("publish failed");

    // The cascade removes `doomed` from under the snapshot that still references it.
    visualization_service::delete(&app.db, org_id, &owner, viz.id)
        .await
        .expect("visualization delete failed");

    let discarded =
        dashboard_service::discard_changes(&app.db, &owner, reload(&app.db, d.id).await)
            .await
            .expect("discard must recover, not 500 on the dangling FK");

    let live = dashboard_service::load_widgets_with_viz(&app.db, discarded.id)
        .await
        .unwrap();
    assert!(
        live.iter().any(|w| w.widget.id == survivor.id),
        "the surviving text widget must be restored"
    );
    assert!(
        !live.iter().any(|w| w.widget.id == doomed.id),
        "the widget referencing the deleted visualization must not be reinserted"
    );

    // A second discard on the same dangling snapshot must also succeed.
    dashboard_service::discard_changes(&app.db, &owner, reload(&app.db, d.id).await)
        .await
        .expect("repeat discard must also succeed");
}

#[tokio::test]
async fn non_editor_viewer_grant_denied_publish_and_discard() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["create_dashboard", "edit_dashboard", "view_dashboard"],
    )
    .await;
    let (viewer, _) = user_with_role(&app.db, org_id, &["view_dashboard"]).await;

    let d = make_dashboard(&app.db, org_id, &owner, "Owner's Dashboard").await;
    add_text_widget(&app.db, org_id, &owner, d.id, "A tile").await;
    let d = dashboard_service::publish(&app.db, &owner, d)
        .await
        .expect("publish failed");

    permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "dashboard",
        d.id,
        Some(viewer.id),
        None,
        "viewer",
    )
    .await
    .expect("grant failed");

    let err = dashboard_service::publish(&app.db, &viewer, d.clone())
        .await
        .expect_err("a viewer grant must not be able to publish");
    assert!(
        err.status.as_u16() == 403 || err.status.as_u16() == 404,
        "publish denial must be 403 or 404, got {}",
        err.status
    );

    let err = dashboard_service::discard_changes(&app.db, &viewer, d.clone())
        .await
        .expect_err("a viewer grant must not be able to discard");
    assert!(
        err.status.as_u16() == 403 || err.status.as_u16() == 404,
        "discard denial must be 403 or 404, got {}",
        err.status
    );

    arcodash_backend::authz::authorize::authorize(
        &app.db,
        &viewer,
        Action::Dashboard {
            verb: DashboardVerb::View,
            id: d.id,
        },
    )
    .await
    .expect("viewer grant must still allow viewing");
}

/// `has_unpublished_changes`/`change_summary` reflect the draft-vs-snapshot diff before and after a
/// publish.
#[tokio::test]
async fn has_unpublished_changes_and_change_summary_correct() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["create_dashboard", "edit_dashboard", "view_dashboard"],
    )
    .await;

    let d = make_dashboard(&app.db, org_id, &owner, "Diff Me").await;
    add_text_widget(&app.db, org_id, &owner, d.id, "Stays Same").await;
    let removed = add_text_widget(&app.db, org_id, &owner, d.id, "Will Be Removed").await;
    let d = dashboard_service::publish(&app.db, &owner, d)
        .await
        .expect("publish failed");

    let live = dashboard_service::load_widgets_with_viz(&app.db, d.id)
        .await
        .unwrap();
    assert!(
        !dashboard_service::has_unpublished_changes(&d, &live),
        "a freshly-published dashboard must report no pending changes"
    );

    add_text_widget(&app.db, org_id, &owner, d.id, "Newly Added").await;
    widget_service::delete(&app.db, org_id, &owner, removed.id)
        .await
        .unwrap();

    let live = dashboard_service::load_widgets_with_viz(&app.db, d.id)
        .await
        .unwrap();
    assert!(
        dashboard_service::has_unpublished_changes(&d, &live),
        "adding/removing a tile must be detected as a pending change"
    );
    let snapshot: dashboard_service::DashboardSnapshot =
        serde_json::from_value(d.published_snapshot.clone().unwrap()).unwrap();
    let summary = dashboard_service::change_summary(&live, &snapshot);
    assert_eq!(summary.added, 1, "one tile was added");
    assert_eq!(summary.removed, 1, "one tile was removed");

    let d = reload(&app.db, d.id).await;
    let republished = dashboard_service::publish(&app.db, &owner, d)
        .await
        .expect("publish changes failed");
    let live = dashboard_service::load_widgets_with_viz(&app.db, republished.id)
        .await
        .unwrap();
    assert!(
        !dashboard_service::has_unpublished_changes(&republished, &live),
        "publishing changes must clear the pending state"
    );
}

/// The batched layout write is all-or-nothing: a mid-batch failure leaves every prior position
/// untouched.
#[tokio::test]
async fn batched_layout_write_is_all_or_nothing() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["create_dashboard", "edit_dashboard", "view_dashboard"],
    )
    .await;

    let d = make_dashboard(&app.db, org_id, &owner, "Layout Dashboard").await;
    let w1 = add_text_widget(&app.db, org_id, &owner, d.id, "One").await;
    let w2 = add_text_widget(&app.db, org_id, &owner, d.id, "Two").await;
    let w3 = add_text_widget(&app.db, org_id, &owner, d.id, "Three").await;

    let positions = vec![
        dashboard_service::LayoutPosition {
            widget_id: w1.id,
            position: json!({ "col": 1, "row": 1, "sizeX": 2, "sizeY": 2 }),
        },
        dashboard_service::LayoutPosition {
            widget_id: w2.id,
            position: json!({ "col": 3, "row": 1, "sizeX": 2, "sizeY": 2 }),
        },
        dashboard_service::LayoutPosition {
            widget_id: w3.id,
            position: json!({ "col": 5, "row": 1, "sizeX": 2, "sizeY": 2 }),
        },
    ];
    let updated = dashboard_service::save_layout(&app.db, org_id, &owner, &d.slug, positions)
        .await
        .expect("valid batch must succeed");
    assert_eq!(updated.len(), 3);
    for w in &updated {
        let body = serde_json::to_string(&w.options).unwrap();
        assert!(
            !body.contains("version"),
            "no version field anywhere in the layout write"
        );
    }
    let after_success = dashboard_service::load_widgets_with_viz(&app.db, d.id)
        .await
        .unwrap();
    let w1_pos_after_success = after_success
        .iter()
        .find(|w| w.widget.id == w1.id)
        .unwrap()
        .widget
        .options
        .clone();
    assert_eq!(w1_pos_after_success["position"]["col"], 1);

    let bogus_id = w1.id + w2.id + w3.id + 100_000;
    let positions = vec![
        dashboard_service::LayoutPosition {
            widget_id: w1.id,
            position: json!({ "col": 9, "row": 9, "sizeX": 2, "sizeY": 2 }),
        },
        dashboard_service::LayoutPosition {
            widget_id: bogus_id,
            position: json!({ "col": 9, "row": 9, "sizeX": 2, "sizeY": 2 }),
        },
    ];
    let err = dashboard_service::save_layout(&app.db, org_id, &owner, &d.slug, positions)
        .await
        .expect_err("a batch naming a nonexistent widget must fail");
    assert_eq!(err.status.as_u16(), 404);

    // w1's position must be unchanged; the failed batch rolled back entirely.
    let after_failure = dashboard_service::load_widgets_with_viz(&app.db, d.id)
        .await
        .unwrap();
    let w1_pos_after_failure = after_failure
        .iter()
        .find(|w| w.widget.id == w1.id)
        .unwrap()
        .widget
        .options
        .clone();
    assert_eq!(
        w1_pos_after_failure["position"]["col"], 1,
        "a failed batch must not partially apply — w1 must keep its prior position"
    );
}

#[tokio::test]
async fn full_publish_edit_publish_discard_round_trip_via_http() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let created: serde_json::Value = client
        .post(format!("{}/api/dashboards", app.address))
        .json(&json!({ "name": "Round Trip Dashboard" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let slug = created["slug"].as_str().unwrap().to_string();
    let dashboard_id = created["id"].as_i64().unwrap();
    assert_eq!(created["published"], false);

    let resp = client
        .post(format!("{}/api/dashboards/{slug}/publish", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    let w1: serde_json::Value = client
        .post(format!("{}/api/widgets", app.address))
        .json(&json!({ "dashboard_id": dashboard_id, "text": "Tile One", "width": 1 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    client
        .post(format!("{}/api/widgets", app.address))
        .json(&json!({ "dashboard_id": dashboard_id, "text": "Tile Two", "width": 1 }))
        .send()
        .await
        .unwrap();

    let published: serde_json::Value = client
        .post(format!("{}/api/dashboards/{slug}/publish", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(published["published"], true);
    assert_eq!(published["has_unpublished_changes"], false);
    assert_eq!(published["widgets"].as_array().unwrap().len(), 2);

    client
        .post(format!("{}/api/widgets", app.address))
        .json(&json!({ "dashboard_id": dashboard_id, "text": "Tile Three", "width": 1 }))
        .send()
        .await
        .unwrap();

    // The default read must still serve the OLD published arrangement.
    let still_published: serde_json::Value = client
        .get(format!("{}/api/dashboards/{slug}", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        still_published["widgets"].as_array().unwrap().len(),
        2,
        "default read must serve the published snapshot, not the pending 3-tile draft"
    );

    let pending: serde_json::Value = client
        .get(format!("{}/api/dashboards/{slug}?draft=true", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pending["published"], true);
    assert_eq!(pending["has_unpublished_changes"], true);
    assert_eq!(pending["pending_change_summary"]["added"], 1);
    assert_eq!(pending["widgets"].as_array().unwrap().len(), 3);

    let republished: serde_json::Value = client
        .post(format!("{}/api/dashboards/{slug}/publish", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(republished["has_unpublished_changes"], false);
    assert_eq!(republished["widgets"].as_array().unwrap().len(), 3);

    client
        .delete(format!(
            "{}/api/widgets/{}",
            app.address,
            w1["id"].as_i64().unwrap()
        ))
        .send()
        .await
        .unwrap();

    let after_delete: serde_json::Value = client
        .get(format!("{}/api/dashboards/{slug}?draft=true", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after_delete["has_unpublished_changes"], true);
    assert_eq!(after_delete["widgets"].as_array().unwrap().len(), 2);

    // The default read still shows the last published arrangement.
    let still_published_after_delete: serde_json::Value = client
        .get(format!("{}/api/dashboards/{slug}", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        still_published_after_delete["widgets"]
            .as_array()
            .unwrap()
            .len(),
        3,
        "default read must still show the last published arrangement, not the pending delete"
    );

    let discarded: serde_json::Value = client
        .post(format!("{}/api/dashboards/{slug}/discard", app.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(discarded["has_unpublished_changes"], false);
    assert_eq!(
        discarded["widgets"].as_array().unwrap().len(),
        3,
        "discard must restore the deleted tile from the last publish"
    );

    for body in [&published, &pending, &republished, &discarded] {
        let text = serde_json::to_string(body).unwrap();
        assert!(
            !text.contains("working_version"),
            "no CAS token for dashboards"
        );
        assert!(
            !text.contains("\"version\""),
            "no version field for dashboards"
        );
    }
}

/// A never-published dashboard has no snapshot, so an editor's default read serves the live draft
/// widgets.
#[tokio::test]
async fn never_published_dashboard_view_mode_serves_draft_widgets_to_editor() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["create_dashboard", "edit_dashboard", "view_dashboard"],
    )
    .await;

    let d = make_dashboard(&app.db, org_id, &owner, "Never Published").await;
    assert!(!dashboard_service::is_published(&d));
    add_text_widget(&app.db, org_id, &owner, d.id, "Draft Tile").await;

    let client = api_key_client(&test_api_key(&owner.email));
    let raw = client
        .get(format!("{}/api/dashboards/{}", app.address, d.slug))
        .send()
        .await
        .unwrap();
    let status = raw.status();
    let body = raw.text().await.unwrap();
    let resp: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("status {status}: {body} ({e})"));
    assert_eq!(status.as_u16(), 200, "body: {body}");

    assert_eq!(resp["published"], false);
    assert_eq!(
        resp["widgets"].as_array().unwrap().len(),
        1,
        "editor's View-mode read of a never-published dashboard must serve the draft widgets, \
         not the empty published-snapshot fallback"
    );
    assert_eq!(resp["widgets"][0]["text"], "Draft Tile");
}

/// The never-published fallback must not leak draft content to a non-editor.
#[tokio::test]
async fn never_published_dashboard_view_mode_stays_empty_for_non_editor() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["create_dashboard", "edit_dashboard", "view_dashboard"],
    )
    .await;
    let (viewer, _) = user_with_role(&app.db, org_id, &["view_dashboard"]).await;

    let d = make_dashboard(&app.db, org_id, &owner, "Never Published").await;
    add_text_widget(&app.db, org_id, &owner, d.id, "Draft Tile").await;

    permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "dashboard",
        d.id,
        Some(viewer.id),
        None,
        "viewer",
    )
    .await
    .expect("grant failed");

    let client = api_key_client(&test_api_key(&viewer.email));
    let resp: serde_json::Value = client
        .get(format!("{}/api/dashboards/{}", app.address, d.slug))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["published"], false);
    assert_eq!(
        resp["widgets"].as_array().unwrap().len(),
        0,
        "a non-editor must never see draft widgets, even on a never-published dashboard"
    );
}
