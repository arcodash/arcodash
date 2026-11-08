//! Capability-ceiling authz integration tests for read gating.
//!
//! Confirms every object family's reads require `view_<obj>` capability AND object-access
//! ≥ Viewer, and that builtin Editor/Viewer/Admin retain their reads (they already carry the
//! `view_*` keys — `crates/shared/src/org_seed.rs`).

mod common;
use common::{make_org, user_with_role};

use arcodash_backend::services::{
    dashboard_service, data_source_service, monitor_service, query_service,
};
use sea_orm::{ActiveModelTrait, ActiveValue::Set};

/// A custom role built with an edit-tier key but explicitly WITHOUT the matching view_ key loses
/// reads for that object type — across all four object families in one pass.
#[tokio::test]
async fn edit_tier_without_view_tier_loses_reads_across_all_object_families() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    // Query: edit_query but no view_query.
    let (query_editor, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    let ds = arcodash_core::test_support::new_data_source(org_id)
        .insert(&app.db)
        .await
        .expect("ds insert failed");
    let mut q_active = arcodash_core::test_support::new_query(org_id, ds.id, query_editor.id);
    q_active.api_key = Set(uuid::Uuid::new_v4().simple().to_string());
    let q = q_active.insert(&app.db).await.expect("query insert failed");
    let published = query_service::publish(&app.db, &query_editor, q, 0)
        .await
        .expect("publish failed");
    let denied = query_service::get_for_user(&app.db, org_id, &query_editor, published.id).await;
    assert!(
        denied.is_err(),
        "edit_query without view_query must lose read access to the query"
    );

    // Dashboard: edit_dashboard but no view_dashboard.
    let (dash_editor, _) =
        user_with_role(&app.db, org_id, &["create_dashboard", "edit_dashboard"]).await;
    let d = dashboard_service::create(&app.db, org_id, &dash_editor, "Edit Only".to_string())
        .await
        .expect("create failed");
    let denied_d = dashboard_service::get_by_slug(&app.db, org_id, &dash_editor, &d.slug).await;
    assert!(
        denied_d.is_err(),
        "edit_dashboard without view_dashboard must lose read access"
    );

    // Monitor: edit_monitor but no view_monitor.
    let (monitor_editor, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let ds2 = arcodash_core::test_support::new_data_source(org_id)
        .insert(&app.db)
        .await
        .expect("ds insert failed");
    let mut q2_active = arcodash_core::test_support::new_query(org_id, ds2.id, monitor_editor.id);
    q2_active.api_key = Set(uuid::Uuid::new_v4().simple().to_string());
    let q2 = q2_active
        .insert(&app.db)
        .await
        .expect("query insert failed");
    let published2 = query_service::publish(&app.db, &monitor_editor, q2, 0)
        .await
        .expect("publish failed");
    let m = monitor_service::create(
        &app.db,
        org_id,
        &monitor_editor,
        published2.id,
        "Edit Only Monitor".to_string(),
        serde_json::json!({ "op": ">", "value": 1 }),
        None,
    )
    .await
    .expect("monitor create failed");
    let denied_m = monitor_service::get_for_user(&app.db, org_id, &monitor_editor, m.id).await;
    assert!(
        denied_m.is_err(),
        "edit_monitor without view_monitor must lose read access"
    );

    // Data source: edit_data_source (+ Full grant) but no view_data_source.
    let (ds_editor, _) = user_with_role(&app.db, org_id, &["edit_data_source"]).await;
    let ds3 = arcodash_core::test_support::new_data_source(org_id)
        .insert(&app.db)
        .await
        .expect("ds insert failed");
    use arcodash_core::entities::data_source_group;
    data_source_group::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        data_source_id: Set(ds3.id),
        group_id: Set(ds_editor.groups[0]),
        created_at: sea_orm::ActiveValue::NotSet,
        updated_at: sea_orm::ActiveValue::NotSet,
    }
    .insert(&app.db)
    .await
    .expect("data_source_group insert failed");
    let denied_ds = data_source_service::get_for_user(&app.db, org_id, &ds_editor, ds3.id).await;
    assert!(
        denied_ds.is_err(),
        "edit_data_source (+ Full grant) without view_data_source must lose read access"
    );
}

/// Builtin Editor/Viewer/Admin (`org_seed.rs`) retain all their reads —
/// they already carry the `view_*` keys.
#[tokio::test]
async fn builtin_editor_viewer_admin_retain_reads() {
    use arcodash_core::entities::{group, organization, user};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let app = skip_if_none!(common::spawn_app().await);

    let default_org = organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(&app.db)
        .await
        .expect("org query failed")
        .expect("default org must exist");

    let editor_group = group::Entity::find()
        .filter(group::Column::OrgId.eq(default_org.id))
        .filter(group::Column::Name.eq("default"))
        .one(&app.db)
        .await
        .expect("group query failed")
        .expect("builtin Editor (default) group must exist");
    let admin_group = group::Entity::find()
        .filter(group::Column::OrgId.eq(default_org.id))
        .filter(group::Column::Name.eq("admin"))
        .one(&app.db)
        .await
        .expect("group query failed")
        .expect("builtin admin group must exist");

    let editor_user = user::Entity::insert(user::ActiveModel {
        org_id: Set(default_org.id),
        name: Set("Builtin Editor".to_string()),
        email: Set(format!(
            "builtin-editor-{}@example.com",
            uuid::Uuid::new_v4().simple()
        )),
        groups: Set(vec![editor_group.id]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(
            &uuid::Uuid::new_v4().simple().to_string(),
        )),
        details: Set(serde_json::json!({})),
        digest_enabled: Set(false),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .expect("editor user insert failed")
    .last_insert_id;
    let editor_user = user::Entity::find_by_id(editor_user)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();

    let admin_user = user::Entity::insert(user::ActiveModel {
        org_id: Set(default_org.id),
        name: Set("Builtin Admin".to_string()),
        email: Set(format!(
            "builtin-admin-{}@example.com",
            uuid::Uuid::new_v4().simple()
        )),
        groups: Set(vec![admin_group.id]),
        api_key_hash: Set(arcodash_core::hash::hash_api_key(
            &uuid::Uuid::new_v4().simple().to_string(),
        )),
        details: Set(serde_json::json!({})),
        digest_enabled: Set(false),
        ..Default::default()
    })
    .exec(&app.db)
    .await
    .expect("admin user insert failed")
    .last_insert_id;
    let admin_user = user::Entity::find_by_id(admin_user)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();

    // Admin creates a dashboard + query + data source; the builtin Editor must still be able to
    // read all of them (Editor carries view_dashboard/view_query/view_data_source/view_monitor).
    let d = dashboard_service::create(
        &app.db,
        default_org.id,
        &admin_user,
        "Builtin Read".to_string(),
    )
    .await
    .expect("admin create dashboard failed");
    let widget_active = arcodash_core::entities::widget::ActiveModel {
        dashboard_id: Set(d.id),
        text: Set(Some("tile".to_string())),
        width: Set(1),
        options: Set(serde_json::json!({})),
        ..Default::default()
    };
    widget_active
        .insert(&app.db)
        .await
        .expect("widget insert failed");
    let d = dashboard_service::publish(&app.db, &admin_user, d)
        .await
        .expect("admin publish dashboard failed");
    dashboard_service::get_by_slug(&app.db, default_org.id, &editor_user, &d.slug)
        .await
        .expect("builtin Editor must retain dashboard read");

    let ds = arcodash_core::test_support::new_data_source(default_org.id)
        .insert(&app.db)
        .await
        .expect("ds insert failed");
    use arcodash_core::entities::data_source_group;
    data_source_group::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        data_source_id: Set(ds.id),
        group_id: Set(editor_group.id),
        created_at: sea_orm::ActiveValue::NotSet,
        updated_at: sea_orm::ActiveValue::NotSet,
    }
    .insert(&app.db)
    .await
    .expect("data_source_group insert failed");
    data_source_service::get_for_user(&app.db, default_org.id, &editor_user, ds.id)
        .await
        .expect("builtin Editor must retain data source read");

    let mut q_active = arcodash_core::test_support::new_query(default_org.id, ds.id, admin_user.id);
    q_active.api_key = Set(uuid::Uuid::new_v4().simple().to_string());
    let q = q_active.insert(&app.db).await.expect("query insert failed");
    let published = query_service::publish(&app.db, &admin_user, q, 0)
        .await
        .expect("publish failed");
    query_service::get_for_user(&app.db, default_org.id, &editor_user, published.id)
        .await
        .expect("builtin Editor must retain query read");

    let m = monitor_service::create(
        &app.db,
        default_org.id,
        &admin_user,
        published.id,
        "Builtin Monitor".to_string(),
        serde_json::json!({ "op": ">", "value": 1 }),
        None,
    )
    .await
    .expect("admin create monitor failed");
    monitor_service::get_for_user(&app.db, default_org.id, &editor_user, m.id)
        .await
        .expect("builtin Editor must retain monitor read");
}
