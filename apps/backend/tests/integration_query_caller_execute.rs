//! The query `caller` block carries execute visibility (`can_execute` /
//! `execute_blocked_reason`). For an admin who owns the data source and query, execute is
//! allowed on both the direct GET and the list endpoint, and the blocked-reason key is absent.

mod common;
use common::default_org_id;
use common::{spawn_app, test_api_key, user_with_role};

#[tokio::test]
async fn caller_reports_can_execute_on_direct_get_and_list() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    let client = app.admin_client().await;

    let ds = client
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({"name": "exec-ds", "type": "pg", "options": {}}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let ds_id = ds["id"].as_i64().unwrap();

    let q = client
        .post(format!("{}/api/queries", app.address))
        .json(&serde_json::json!({
            "data_source_id": ds_id,
            "name": "Exec Caller Query",
            "query": "SELECT 1",
            "options": {}
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let q_id = q["id"].as_i64().unwrap();

    let direct = client
        .get(format!("{}/api/queries/{}", app.address, q_id))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(
        direct["caller"]["can_execute"],
        serde_json::json!(true),
        "admin owner can execute: {}",
        direct["caller"]
    );
    assert!(
        direct["caller"]["execute_blocked_reason"].is_null(),
        "no blocked reason when execute is allowed: {}",
        direct["caller"]
    );

    let list = client
        .get(format!("{}/api/queries", app.address))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"].as_i64() == Some(q_id))
        .expect("created query present in list");
    assert_eq!(
        row["caller"]["can_execute"],
        serde_json::json!(true),
        "list row reports can_execute: {}",
        row["caller"]
    );
    assert_eq!(
        direct["caller"], row["caller"],
        "caller block matches between direct GET and list"
    );
}

async fn restrict_group(db: &sea_orm::DatabaseConnection, group_id: i64) {
    use arcodash_core::entities::group;
    use sea_orm::{ActiveModelTrait, EntityTrait};
    let g = group::Entity::find_by_id(group_id)
        .one(db)
        .await
        .unwrap()
        .expect("group not found");
    let mut active: group::ActiveModel = g.into();
    active.data_source_access_restricted = sea_orm::ActiveValue::Set(true);
    active.update(db).await.expect("group restrict failed");
}

async fn add_unrestricted_powerless_group(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    user: arcodash_core::entities::user::Model,
) -> arcodash_core::entities::user::Model {
    use arcodash_core::entities::{group, user};
    use sea_orm::{ActiveModelTrait, EntityTrait};
    let group_id = group::Entity::insert(group::ActiveModel {
        org_id: sea_orm::ActiveValue::Set(org_id),
        name: sea_orm::ActiveValue::Set(format!("unrestricted-{}", uuid::Uuid::new_v4().simple())),
        permissions: sea_orm::ActiveValue::Set(vec![]),
        r#type: sea_orm::ActiveValue::Set("regular".to_string()),
        data_source_access_restricted: sea_orm::ActiveValue::Set(false),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("group insert failed")
    .last_insert_id;

    let mut groups = user.groups.clone();
    groups.push(group_id);
    let mut active: user::ActiveModel = user.into();
    active.groups = sea_orm::ActiveValue::Set(groups);
    active.update(db).await.expect("user update failed")
}

/// The `caller.can_execute` the API reports must agree with what an actual run does. A user
/// whose `execute_query` capability comes from a restricted group that does NOT allowlist the
/// query's data source — while a *different*, unrestricted group lends only workspace-visibility
/// reach, no capability — must see `can_execute: false` on the direct GET, and the real
/// `POST /api/queries/:id/results` must 404 (the same opaque "no access" shape a stranger with no
/// grant at all gets), not silently succeed on borrowed reach.
#[tokio::test]
async fn caller_can_execute_agrees_with_the_actual_run_when_capability_and_reach_split_groups() {
    use sea_orm::{ActiveModelTrait, EntityTrait};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    // A real HTTP request's org resolution falls back to the app's single seeded ("default")
    // org, not an arbitrary `make_org` row — the user must live there or API-key auth 401s
    // before ever reaching the authorization check under test.
    let org_id = default_org_id(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["edit_query", "execute_query", "create_query"],
    )
    .await;

    let ds_id = arcodash_core::entities::data_source::Entity::insert(
        arcodash_core::test_support::new_data_source(org_id),
    )
    .exec(&app.db)
    .await
    .expect("data source insert failed")
    .last_insert_id;

    let mut q_active = arcodash_core::test_support::new_query(org_id, ds_id, owner.id);
    q_active.api_key = sea_orm::ActiveValue::Set(uuid::Uuid::new_v4().simple().to_string());
    let q = q_active.insert(&app.db).await.expect("query insert failed");
    // Publish so the query is workspace-visible to a Viewer with no explicit ACL grant — an ACL
    // grant would instead make a denied run 403 (friendly upgrade), not the plain 404 this test
    // checks for.
    let q = arcodash_backend::services::query_service::publish(&app.db, &owner, q, 0)
        .await
        .expect("publish failed");

    let (user, _) = user_with_role(&app.db, org_id, &["view_query", "execute_query"]).await;
    let restricted_group_id = user.groups[0];
    restrict_group(&app.db, restricted_group_id).await;
    // Deliberately no `data_source_group` row for `ds_id` — the restricted group's own allowlist
    // excludes this data source.

    let user = add_unrestricted_powerless_group(&app.db, org_id, user).await;

    let key = test_api_key(&user.email);
    let get_resp = reqwest::Client::new()
        .get(format!(
            "{}/api/queries/{}?api_key={key}",
            app.address, q.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        get_resp.status().as_u16(),
        200,
        "workspace-visibility reach must still show the query"
    );
    let body: serde_json::Value = get_resp.json().await.unwrap();
    assert_eq!(
        body["caller"]["can_execute"],
        serde_json::json!(false),
        "capability and reach on different groups must report can_execute: false: {}",
        body["caller"]
    );

    let run_resp = reqwest::Client::new()
        .post(format!(
            "{}/api/queries/{}/results?api_key={key}",
            app.address, q.id
        ))
        .json(&serde_json::json!({ "parameters": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        run_resp.status().as_u16(),
        404,
        "the actual run must agree with can_execute: false and 404, not silently succeed"
    );
}

/// `GET /api/queries` batches the per-row Edit/Execute authz checks and the data-source reach
/// check (rather than resolving each per row) — this must not change the answer for any
/// reachable `execute_blocked_reason` value, or for the allowed case. One shared, published
/// query; three users exercise every `(capability_ok, data_source_ok)` combination a non-admin
/// can actually reach, and each user's list row must exactly match what a direct `GET`
/// reports.
#[tokio::test]
async fn query_list_caller_blocks_are_unchanged_by_batching() {
    use sea_orm::{ActiveModelTrait, EntityTrait};

    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["edit_query", "execute_query", "create_query", "view_query"],
    )
    .await;

    let ds_id = arcodash_core::entities::data_source::Entity::insert(
        arcodash_core::test_support::new_data_source(org_id),
    )
    .exec(&app.db)
    .await
    .expect("data source insert failed")
    .last_insert_id;

    let mut q_active = arcodash_core::test_support::new_query(org_id, ds_id, owner.id);
    q_active.api_key = sea_orm::ActiveValue::Set(uuid::Uuid::new_v4().simple().to_string());
    let q = q_active.insert(&app.db).await.expect("query insert failed");
    let q = arcodash_backend::services::query_service::publish(&app.db, &owner, q, 0)
        .await
        .expect("publish failed");

    // A second, decoy query so the list endpoint actually has more than one row to batch over.
    let mut decoy_active = arcodash_core::test_support::new_query(org_id, ds_id, owner.id);
    decoy_active.api_key = sea_orm::ActiveValue::Set(uuid::Uuid::new_v4().simple().to_string());
    let decoy = decoy_active
        .insert(&app.db)
        .await
        .expect("decoy insert failed");
    arcodash_backend::services::query_service::publish(&app.db, &owner, decoy, 0)
        .await
        .expect("decoy publish failed");

    // `execute_blocked_reason: "capability"` (capability_ok=false, data_source_ok=true) is not
    // reachable by a non-admin under the current formula: `data_source_ok` is
    // `any_group_grants_with_reach(.., "execute_query", ..)`, which requires *some* group to hold
    // `execute_query` -- but capability_ok is false only when *no* group holds it anywhere, which
    // forces `data_source_ok` false too. So only "data_source", "both", and "allowed" are covered
    // here -- that's every state the batched path can actually produce.
    let cases: [(&[&str], bool, Option<&str>); 3] = [
        (&["view_query", "execute_query"], false, Some("data_source")),
        (&["view_query"], false, Some("both")),
        (&["view_query", "execute_query"], true, None),
    ];

    for (perms, ds_reachable, expected_reason) in cases {
        let (user, _) = user_with_role(&app.db, org_id, perms).await;
        let restricted_group_id = user.groups[0];
        if !ds_reachable {
            restrict_group(&app.db, restricted_group_id).await;
            // No `data_source_group` row for `ds_id` — the restricted group's allowlist excludes
            // it.
        }
        // An unrestricted, permission-less second group so a capability-missing user still has
        // workspace-visibility reach to see the published query at all.
        let user = add_unrestricted_powerless_group(&app.db, org_id, user).await;
        let key = test_api_key(&user.email);

        let direct = reqwest::Client::new()
            .get(format!(
                "{}/api/queries/{}?api_key={key}",
                app.address, q.id
            ))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(
            direct["caller"]["execute_blocked_reason"],
            match expected_reason {
                Some(r) => serde_json::json!(r),
                None => serde_json::Value::Null,
            },
            "direct GET reason for perms {perms:?}, ds_reachable={ds_reachable}: {}",
            direct["caller"]
        );

        let list = reqwest::Client::new()
            .get(format!("{}/api/queries?api_key={key}", app.address))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        let row = list
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"].as_i64() == Some(q.id))
            .expect("shared query present in this caller's list");
        assert_eq!(
            direct["caller"], row["caller"],
            "list row caller must match direct GET for perms {perms:?}, ds_reachable={ds_reachable}"
        );
    }
}
