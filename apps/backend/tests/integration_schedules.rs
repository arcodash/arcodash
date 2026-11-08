//! Integration tests for the schedules/runs API (`/api/schedules`, `/api/runs`, query/monitor
//! run-now, `/api/queries/:id/consumers`). Single-org config means "cross-org" reduces to
//! "nonexistent id"; both take the same org-scoped 404 path.

mod common;
use common::{authed_client, spawn_app, TestApp};
use common::{default_org_id, ok_json};

async fn json_status(resp: reqwest::Response, expected: u16, context: &str) -> serde_json::Value {
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, expected, "{context}: {body}");
    if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("{context} body not JSON ({e}): {body}"))
    }
}

async fn create_ds(client: &reqwest::Client, base: &str) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/data_sources"))
            .json(&serde_json::json!({ "name": "sched-test-ds", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

/// Create a query, optionally publishing it, and return its id.
async fn create_query(client: &reqwest::Client, base: &str, ds_id: i64, publish: bool) -> i64 {
    let query_id = ok_json(
        client
            .post(format!("{base}/api/queries"))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Schedule Test Query",
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

    if publish {
        ok_json(
            client
                .post(format!("{base}/api/queries/{query_id}/publish"))
                .json(&serde_json::json!({ "expected_version": 0 }))
                .send()
                .await
                .unwrap(),
            "publish query",
        )
        .await;
    }
    query_id
}

fn valid_monitor_options() -> serde_json::Value {
    serde_json::json!({ "op": ">", "value": 100, "column": "count" })
}

async fn create_monitor(client: &reqwest::Client, base: &str, query_id: i64) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/monitors"))
            .json(&serde_json::json!({
                "query_id": query_id,
                "name": "Schedule Test Monitor",
                "options": valid_monitor_options(),
            }))
            .send()
            .await
            .unwrap(),
        "create monitor",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

async fn invite_user(client: &reqwest::Client, base: &str, email: &str) -> i64 {
    ok_json(
        client
            .post(format!("{base}/api/users"))
            .json(&serde_json::json!({ "name": "Other User", "email": email, "groups": [] }))
            .send()
            .await
            .unwrap(),
        "invite user",
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

/// Accept an invitation and log the new user in; returns `(user_id, authed client)`. The user has
/// no groups until `grant_capability`.
async fn onboard_user(
    app: &TestApp,
    admin: &reqwest::Client,
    email: &str,
    password: &str,
) -> (i64, reqwest::Client) {
    let user_id = invite_user(admin, &app.address, email).await;

    use sea_orm::EntityTrait;
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

async fn grant_query_view(admin: &reqwest::Client, base: &str, query_id: i64, user_id: i64) {
    let resp = admin
        .post(format!("{base}/api/queries/{query_id}/acl"))
        .json(&serde_json::json!({ "user_id": user_id, "access_type": "view" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "grant query view");
}

/// Give `user_id` a fresh role+group carrying exactly `perms`.
async fn grant_capability(db: &sea_orm::DatabaseConnection, user_id: i64, perms: &[&str]) {
    use arcodash_core::entities::{group, group_role, role, user};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

    let u = user::Entity::find_by_id(user_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let role_id = role::Entity::insert(role::ActiveModel {
        org_id: Set(u.org_id),
        name: Set(format!("cap-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(perms.iter().map(|s| s.to_string()).collect()),
        r#type: Set("custom".to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id;
    let group_id = group::Entity::insert(group::ActiveModel {
        org_id: Set(u.org_id),
        name: Set(format!("cap-group-{}", uuid::Uuid::new_v4().simple())),
        permissions: Set(vec![]),
        data_source_access_restricted: Set(false),
        r#type: Set("regular".to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id;
    group_role::Entity::insert(group_role::ActiveModel {
        group_id: Set(group_id),
        role_id: Set(role_id),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap();

    let mut groups = u.groups.clone();
    groups.push(group_id);
    let mut active: user::ActiveModel = u.into();
    active.groups = Set(groups);
    active.update(db).await.unwrap();
}

/// Directly insert a `schedules` row, for starting states (e.g. auto-pause) the API can't reach.
#[allow(clippy::too_many_arguments)]
async fn seed_schedule_row(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    entity_type: &str,
    entity_id: i64,
    enabled: bool,
    consecutive_failures: i32,
    auto_paused_at: Option<chrono::DateTime<chrono::Utc>>,
    paused_until: Option<chrono::DateTime<chrono::Utc>>,
) -> i64 {
    use arcodash_core::entities::schedule;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};

    let now = chrono::Utc::now();
    let m = schedule::ActiveModel {
        org_id: Set(org_id),
        entity_type: Set(entity_type.to_string()),
        entity_id: Set(entity_id),
        spec: Set(serde_json::json!({ "kind": "interval", "every_secs": 300 })),
        timezone: Set("UTC".to_string()),
        enabled: Set(enabled),
        paused_until: Set(paused_until.map(Into::into)),
        auto_paused_at: Set(auto_paused_at.map(Into::into)),
        overlap_policy: Set("skip".to_string()),
        next_run_at: Set(Some(now.into())),
        consecutive_failures: Set(consecutive_failures),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    m.id
}

/// Directly insert a terminal `runs` row. `query_result_id: None` simulates a failed/GC'd run.
#[allow(clippy::too_many_arguments)]
async fn seed_run_row(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    entity_type: &str,
    entity_id: i64,
    status: &str,
    query_result_id: Option<i64>,
) -> i64 {
    use arcodash_core::entities::run;
    use sea_orm::{
        ActiveModelTrait,
        ActiveValue::{NotSet, Set},
    };

    let now = chrono::Utc::now();
    let m = run::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        entity_type: Set(entity_type.to_string()),
        entity_id: Set(entity_id),
        schedule_id: Set(None),
        data_source_id: Set(None),
        scheduled_for: Set(None),
        triggered_by: Set("schedule".to_string()),
        status: Set(status.to_string()),
        waiting_on_run_id: Set(None),
        job_id: Set(None),
        worker_id: Set(None),
        lease_expires_at: Set(None),
        enqueued_at: NotSet,
        started_at: Set(Some(now.into())),
        finished_at: Set(Some(now.into())),
        duration_ms: Set(Some(42)),
        row_count: Set(Some(1)),
        error: Set(None),
        data_age_secs: Set(None),
        query_result_id: Set(query_result_id),
        condition_met: Set(None),
        created_at: Set(now.into()),
    }
    .insert(db)
    .await
    .unwrap();
    m.id
}

/// A `query_results` row with one column/row, formattable as CSV.
async fn seed_query_result(db: &sea_orm::DatabaseConnection, org_id: i64, ds_id: i64) -> i64 {
    use arcodash_core::entities::query_result;
    use sea_orm::{
        ActiveModelTrait,
        ActiveValue::{NotSet, Set},
    };

    let now = chrono::Utc::now();
    let m = query_result::ActiveModel {
        id: NotSet,
        org_id: Set(org_id),
        data_source_id: Set(ds_id),
        query_hash: Set(format!("hash-{}", uuid::Uuid::new_v4().simple())),
        query: Set("SELECT 1".to_string()),
        data: Set(Some(
            serde_json::json!({ "columns": [{ "name": "count" }], "rows": [{ "count": 1 }] }),
        )),
        data_location: Set(None),
        data_size_bytes: Set(None),
        runtime: Set(Some(0.01)),
        retrieved_at: Set(now.into()),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    }
    .insert(db)
    .await
    .unwrap();
    m.id
}

/// The most recent event row for `(org_id, action, object_id)`, if any.
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

#[tokio::test]
async fn create_and_get_round_trip() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;

    let created = json_status(
        client
            .post(format!("{}/api/schedules", app.address))
            .json(&serde_json::json!({
                "entity_type": "query", "entity_id": query_id,
                "spec": { "kind": "interval", "every_secs": 300 },
            }))
            .send()
            .await
            .unwrap(),
        201,
        "create schedule",
    )
    .await;
    assert!(!created["id"].is_null());
    assert_eq!(created["enabled"], true);
    assert!(created["last_run"].is_null());
    assert!(!created["next_run_at"].is_null());

    let org_id = default_org_id(&app.db).await;
    let schedule_id = created["id"].as_i64().unwrap();
    assert!(
        latest_event(&app.db, org_id, "schedule.created", schedule_id)
            .await
            .is_some(),
        "creating a schedule must record a schedule.created event"
    );

    let list = ok_json(
        client
            .get(format!(
                "{}/api/schedules?entity_type=query&entity_id={query_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "get schedule",
    )
    .await;
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["entity_type"], "query");
    assert_eq!(rows[0]["entity_id"], query_id);
    assert!(rows[0]["last_run"].is_null());
}

/// A PATCH changing only `overlap_policy` must leave `next_run_at` byte-identical, not recompute
/// (and jitter) it.
#[tokio::test]
async fn patch_overlap_policy_only_leaves_next_run_at_untouched() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;

    let created = json_status(
        client
            .post(format!("{}/api/schedules", app.address))
            .json(&serde_json::json!({
                "entity_type": "query", "entity_id": query_id,
                "spec": { "kind": "interval", "every_secs": 300 },
                "overlap_policy": "skip",
            }))
            .send()
            .await
            .unwrap(),
        201,
        "create schedule",
    )
    .await;
    let schedule_id = created["id"].as_i64().unwrap();
    let next_run_at_before = created["next_run_at"].as_str().unwrap().to_string();

    let updated = json_status(
        client
            .patch(format!("{}/api/schedules/{schedule_id}", app.address))
            .json(&serde_json::json!({ "overlap_policy": "coalesce" }))
            .send()
            .await
            .unwrap(),
        200,
        "patch overlap_policy only",
    )
    .await;
    assert_eq!(
        updated["overlap_policy"], "coalesce",
        "the edit itself must apply"
    );
    assert_eq!(
        updated["next_run_at"].as_str().unwrap(),
        next_run_at_before,
        "an overlap_policy-only patch must leave next_run_at byte-identical"
    );
}

#[tokio::test]
async fn validation_matrix_rejects_bad_specs_and_policies() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;

    let cases = [
        serde_json::json!({ "entity_type": "query", "entity_id": query_id,
            "spec": { "kind": "interval", "every_secs": 30 } }),
        serde_json::json!({ "entity_type": "query", "entity_id": query_id,
            "spec": { "kind": "cron", "expr": "not a cron" } }),
        serde_json::json!({ "entity_type": "query", "entity_id": query_id,
            "spec": { "kind": "interval", "every_secs": 300 }, "timezone": "Not/AZone" }),
        serde_json::json!({ "entity_type": "query", "entity_id": query_id,
            "spec": { "kind": "interval", "every_secs": 300 }, "overlap_policy": "parallel" }),
        serde_json::json!({ "entity_type": "query", "entity_id": query_id,
            "spec": { "kind": "interval", "every_secs": 300 }, "overlap_policy": "replace" }),
    ];
    for (i, body) in cases.iter().enumerate() {
        let resp = client
            .post(format!("{}/api/schedules", app.address))
            .json(body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            400,
            "case {i} ({body}) must 400: {}",
            resp.text().await.unwrap_or_default()
        );
    }
}

#[tokio::test]
async fn create_on_unpublished_query_is_422() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, false).await;

    let resp = client
        .post(format!("{}/api/schedules", app.address))
        .json(&serde_json::json!({
            "entity_type": "query", "entity_id": query_id,
            "spec": { "kind": "interval", "every_secs": 300 },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 422);
}

#[tokio::test]
async fn duplicate_schedule_for_same_entity_is_409() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;

    let body = serde_json::json!({
        "entity_type": "query", "entity_id": query_id,
        "spec": { "kind": "interval", "every_secs": 300 },
    });
    let first = client
        .post(format!("{}/api/schedules", app.address))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status().as_u16(), 201);

    let second = client
        .post(format!("{}/api/schedules", app.address))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status().as_u16(), 409);
}

#[tokio::test]
async fn patch_enabling_clears_auto_pause_and_recomputes_next_run() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;
    let org_id = default_org_id(&app.db).await;

    let now = chrono::Utc::now();
    let schedule_id = seed_schedule_row(
        &app.db,
        org_id,
        "query",
        query_id,
        false,
        9,
        Some(now),
        None,
    )
    .await;

    let updated = json_status(
        client
            .patch(format!("{}/api/schedules/{schedule_id}", app.address))
            .json(&serde_json::json!({ "enabled": true }))
            .send()
            .await
            .unwrap(),
        200,
        "patch enable",
    )
    .await;
    assert_eq!(updated["enabled"], true);
    assert!(updated["auto_paused_at"].is_null());
    assert_eq!(updated["consecutive_failures"], 0);
    let next = updated["next_run_at"].as_str().unwrap();
    let next_dt: chrono::DateTime<chrono::Utc> = next.parse().unwrap();
    assert!(
        next_dt > now && next_dt <= now + chrono::Duration::seconds(310),
        "next_run_at {next_dt} should be a near-future, non-backed-off deadline (now={now})"
    );
    assert!(
        latest_event(&app.db, org_id, "schedule.updated", schedule_id)
            .await
            .is_some(),
        "patching a schedule must record a schedule.updated event"
    );
}

#[tokio::test]
async fn patch_clearing_paused_until_clears_auto_pause_too() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;
    let org_id = default_org_id(&app.db).await;

    let now = chrono::Utc::now();
    let future = now + chrono::Duration::hours(1);
    let schedule_id = seed_schedule_row(
        &app.db,
        org_id,
        "query",
        query_id,
        true,
        3,
        Some(now),
        Some(future),
    )
    .await;

    let updated = json_status(
        client
            .patch(format!("{}/api/schedules/{schedule_id}", app.address))
            .json(&serde_json::json!({ "paused_until": null }))
            .send()
            .await
            .unwrap(),
        200,
        "patch clear paused_until",
    )
    .await;
    assert!(updated["paused_until"].is_null());
    assert!(updated["auto_paused_at"].is_null());
    assert_eq!(updated["consecutive_failures"], 0);
}

#[tokio::test]
async fn patch_spec_revalidates_and_400s_on_bad_spec() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;

    let created = json_status(
        client
            .post(format!("{}/api/schedules", app.address))
            .json(&serde_json::json!({
                "entity_type": "query", "entity_id": query_id,
                "spec": { "kind": "interval", "every_secs": 300 },
            }))
            .send()
            .await
            .unwrap(),
        201,
        "create schedule",
    )
    .await;
    let schedule_id = created["id"].as_i64().unwrap();

    let resp = client
        .patch(format!("{}/api/schedules/{schedule_id}", app.address))
        .json(&serde_json::json!({ "spec": { "kind": "interval", "every_secs": 10 } }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

/// PATCH `paused_until` distinguishes omitted (unchanged), explicit `null` (cleared) and a value
/// (set), through the real HTTP + DB round trip.
#[tokio::test]
async fn patch_paused_until_distinguishes_omitted_null_and_set() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;
    let org_id = default_org_id(&app.db).await;

    let schedule_id =
        seed_schedule_row(&app.db, org_id, "query", query_id, true, 0, None, None).await;

    let future = chrono::Utc::now() + chrono::Duration::hours(1);

    let set = json_status(
        client
            .patch(format!("{}/api/schedules/{schedule_id}", app.address))
            .json(&serde_json::json!({ "paused_until": future.to_rfc3339() }))
            .send()
            .await
            .unwrap(),
        200,
        "set paused_until",
    )
    .await;
    assert!(
        !set["paused_until"].is_null(),
        "explicit value must set paused_until"
    );

    let unchanged = json_status(
        client
            .patch(format!("{}/api/schedules/{schedule_id}", app.address))
            .json(&serde_json::json!({ "overlap_policy": "skip" }))
            .send()
            .await
            .unwrap(),
        200,
        "omit paused_until",
    )
    .await;
    assert!(
        !unchanged["paused_until"].is_null(),
        "omitting paused_until must leave the existing value unchanged"
    );

    let cleared = json_status(
        client
            .patch(format!("{}/api/schedules/{schedule_id}", app.address))
            .json(&serde_json::json!({ "paused_until": null }))
            .send()
            .await
            .unwrap(),
        200,
        "clear paused_until",
    )
    .await;
    assert!(
        cleared["paused_until"].is_null(),
        "explicit null must clear paused_until"
    );
}

#[tokio::test]
async fn delete_then_get_is_empty() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;

    let created = json_status(
        client
            .post(format!("{}/api/schedules", app.address))
            .json(&serde_json::json!({
                "entity_type": "query", "entity_id": query_id,
                "spec": { "kind": "interval", "every_secs": 300 },
            }))
            .send()
            .await
            .unwrap(),
        201,
        "create schedule",
    )
    .await;
    let schedule_id = created["id"].as_i64().unwrap();

    let del = client
        .delete(format!("{}/api/schedules/{schedule_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status().as_u16(), 204);

    let org_id = default_org_id(&app.db).await;
    assert!(
        latest_event(&app.db, org_id, "schedule.deleted", schedule_id)
            .await
            .is_some(),
        "deleting a schedule must record a schedule.deleted event"
    );

    let list = ok_json(
        client
            .get(format!(
                "{}/api/schedules?entity_type=query&entity_id={query_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "get after delete",
    )
    .await;
    assert!(list.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn every_verb_404s_on_a_nonexistent_schedule_or_entity() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    // Targeted GET/POST against a nonexistent id takes the same `.filter(org_id)` path a cross-org
    // id would.
    let resp = client
        .get(format!(
            "{}/api/schedules?entity_type=query&entity_id=999999999",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "GET targeted nonexistent entity"
    );

    let resp = client
        .post(format!("{}/api/schedules", app.address))
        .json(&serde_json::json!({
            "entity_type": "query", "entity_id": 999999999,
            "spec": { "kind": "interval", "every_secs": 300 },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "POST against nonexistent entity"
    );

    let resp = client
        .patch(format!("{}/api/schedules/999999999", app.address))
        .json(&serde_json::json!({ "enabled": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "PATCH nonexistent schedule id");

    let resp = client
        .delete(format!("{}/api/schedules/999999999", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "DELETE nonexistent schedule id"
    );
}

#[tokio::test]
async fn viewer_capability_caller_can_read_but_not_mutate() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id, true).await;

    let (other_id, other) =
        onboard_user(&app, &admin, "sched-viewer@test.local", "V13w3rP@ss").await;
    grant_query_view(&admin, &app.address, query_id, other_id).await;
    grant_capability(&app.db, other_id, &["view_query"]).await;

    let resp = other
        .get(format!(
            "{}/api/schedules?entity_type=query&entity_id={query_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "GET must succeed for a Viewer");

    let resp = other
        .post(format!("{}/api/schedules", app.address))
        .json(&serde_json::json!({
            "entity_type": "query", "entity_id": query_id,
            "spec": { "kind": "interval", "every_secs": 300 },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "POST must 404 without schedule_query"
    );

    let created = json_status(
        admin
            .post(format!("{}/api/schedules", app.address))
            .json(&serde_json::json!({
                "entity_type": "query", "entity_id": query_id,
                "spec": { "kind": "interval", "every_secs": 300 },
            }))
            .send()
            .await
            .unwrap(),
        201,
        "admin create schedule",
    )
    .await;
    let schedule_id = created["id"].as_i64().unwrap();

    let resp = other
        .patch(format!("{}/api/schedules/{schedule_id}", app.address))
        .json(&serde_json::json!({ "enabled": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "PATCH must 404 without schedule_query"
    );

    let resp = other
        .delete(format!("{}/api/schedules/{schedule_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "DELETE must 404 without schedule_query"
    );
}

#[tokio::test]
async fn run_now_query_is_idempotent_under_a_double_click() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;

    let first = json_status(
        client
            .post(format!("{}/api/queries/{query_id}/run", app.address))
            .send()
            .await
            .unwrap(),
        201,
        "first run-now",
    )
    .await;
    let second = json_status(
        client
            .post(format!("{}/api/queries/{query_id}/run", app.address))
            .send()
            .await
            .unwrap(),
        200,
        "second run-now (in flight)",
    )
    .await;
    assert_eq!(
        first["id"], second["id"],
        "must return the SAME in-flight run"
    );

    use arcodash_core::entities::run;
    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
    let inflight_count = run::Entity::find()
        .filter(run::Column::EntityType.eq("query"))
        .filter(run::Column::EntityId.eq(query_id))
        .filter(run::Column::Status.is_in(["pending", "running"]))
        .count(&app.db)
        .await
        .unwrap();
    assert_eq!(
        inflight_count, 1,
        "exactly one in-flight run for this query"
    );

    let row = run::Entity::find_by_id(first["id"].as_i64().unwrap())
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        row.enqueued_at.is_some(),
        "a genuinely queued manual run must have enqueued_at stamped, or the per-data-source \
         concurrency cap can't see it as in-flight"
    );
}

#[tokio::test]
async fn run_now_monitor_creates_a_manual_run() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;
    let monitor_id = create_monitor(&client, &app.address, query_id).await;

    let created = json_status(
        client
            .post(format!("{}/api/monitors/{monitor_id}/run", app.address))
            .send()
            .await
            .unwrap(),
        201,
        "monitor run-now",
    )
    .await;
    assert_eq!(created["triggered_by"], "manual");

    use arcodash_core::entities::run;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let row = run::Entity::find()
        .filter(run::Column::EntityType.eq("monitor"))
        .filter(run::Column::EntityId.eq(monitor_id))
        .filter(run::Column::TriggeredBy.eq("manual"))
        .one(&app.db)
        .await
        .unwrap();
    let row = row.expect("a manual monitor run row must exist");
    assert!(
        row.enqueued_at.is_some(),
        "a monitor run's enqueued_at must be stamped too, so the column means the same thing \
         regardless of which producer wrote it"
    );
}

/// Two manual runs on different queries against one `max_concurrent_runs = 1` data source must both
/// carry `enqueued_at`, or `count_running_on_ds` wouldn't count them and both would run
/// concurrently.
#[tokio::test]
async fn manual_runs_against_a_capped_data_source_both_carry_enqueued_at() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_a = create_query(&client, &app.address, ds_id, true).await;
    let query_b = create_query(&client, &app.address, ds_id, true).await;

    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    app.db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE data_sources SET max_concurrent_runs = $1 WHERE id = $2",
            [1i64.into(), ds_id.into()],
        ))
        .await
        .unwrap();

    let run_a = json_status(
        client
            .post(format!("{}/api/queries/{query_a}/run", app.address))
            .send()
            .await
            .unwrap(),
        201,
        "run-now on query A",
    )
    .await;
    let run_b = json_status(
        client
            .post(format!("{}/api/queries/{query_b}/run", app.address))
            .send()
            .await
            .unwrap(),
        201,
        "run-now on query B",
    )
    .await;

    use arcodash_core::entities::run;
    use sea_orm::EntityTrait;
    for run_id in [run_a["id"].as_i64().unwrap(), run_b["id"].as_i64().unwrap()] {
        let row = run::Entity::find_by_id(run_id)
            .one(&app.db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            row.enqueued_at.is_some(),
            "run {run_id} must carry enqueued_at so the data-source cap can see it as in-flight"
        );
    }
}

/// `POST /api/queries/:id/run` and `GET /api/runs/:id/result.csv` must enforce the same
/// data-source-access check as `POST /api/query_results`: a viewer with a query ACL grant but no
/// reach to its data source can't run it or download results. `seed_query_result`/`seed_run_row`
/// stand in for a worker-completed run.
#[tokio::test]
async fn run_and_result_csv_enforce_data_source_access() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id, true).await;
    let org_id = default_org_id(&app.db).await;

    let (viewer_id, viewer) =
        onboard_user(&app, &admin, "run-bypass-viewer@test.local", "V13w3rP@ss").await;
    grant_query_view(&admin, &app.address, query_id, viewer_id).await;
    grant_capability(&app.db, viewer_id, &["view_query", "execute_query"]).await;

    // Restrict the viewer's data-source reach via an allowlist (groups default unrestricted, which
    // would make this test vacuous).
    use arcodash_core::entities::{group, user};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    let viewer_row = user::Entity::find_by_id(viewer_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let cap_group_id = *viewer_row.groups.last().unwrap();
    let cap_group = group::Entity::find_by_id(cap_group_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: group::ActiveModel = cap_group.into();
    active.data_source_access_restricted = Set(true);
    active.update(&app.db).await.unwrap();

    // Baseline: the ad-hoc path already enforces this; `query_id` yields the friendly 403 remap
    // instead of an opaque 404.
    let baseline = viewer
        .post(format!("{}/api/query_results", app.address))
        .json(&serde_json::json!({ "data_source_id": ds_id, "query": "SELECT 1", "query_id": query_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        baseline.status().as_u16(),
        403,
        "baseline: /api/query_results must already deny a viewer with no data-source reach"
    );

    // /run must deny (403 data_source_access_required), not enqueue (201).
    let run_resp = viewer
        .post(format!("{}/api/queries/{query_id}/run", app.address))
        .send()
        .await
        .unwrap();
    let run_status = run_resp.status().as_u16();
    let run_body: serde_json::Value = run_resp.json().await.unwrap_or_default();
    assert_eq!(
        run_status, 403,
        "viewer with no data-source reach must be denied /run, not enqueued: {run_body:?}"
    );
    assert_eq!(
        run_body["error_code"], "data_source_access_required",
        "must surface the friendly error, not a bare 403: {run_body:?}"
    );

    // A finished run + result exists; the viewer must still be refused the download.
    let qr_id = seed_query_result(&app.db, org_id, ds_id).await;
    let run_id = seed_run_row(&app.db, org_id, "query", query_id, "success", Some(qr_id)).await;
    let csv_resp = viewer
        .get(format!("{}/api/runs/{run_id}/result.csv", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        csv_resp.status().as_u16(),
        404,
        "viewer with no data-source reach must not download real result data"
    );

    // A DS-reaching admin is unaffected: /run still enqueues and the CSV downloads.
    let admin_run = json_status(
        admin
            .post(format!("{}/api/queries/{query_id}/run", app.address))
            .send()
            .await
            .unwrap(),
        201,
        "admin run-now must still be allowed",
    )
    .await;
    assert!(admin_run["id"].as_i64().is_some());

    let admin_csv = admin
        .get(format!("{}/api/runs/{run_id}/result.csv", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        admin_csv.status().as_u16(),
        200,
        "admin with real data-source reach must still download the result"
    );
    let body = admin_csv.text().await.unwrap();
    assert!(body.contains("count") && body.contains('1'));
}

/// `schedule_service::authorize_entity` must apply the same data-source axis as
/// `query_service::get_for_user`: a 404 on `GET /api/queries/<id>` means no access to its run
/// history or schedule either.
#[tokio::test]
async fn authorize_entity_denies_run_history_and_schedule_list_without_data_source_reach() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id, true).await;

    let (viewer_id, viewer) =
        onboard_user(&app, &admin, "sched-bypass-viewer@test.local", "V13w3rP@ss").await;
    grant_capability(&app.db, viewer_id, &["view_query"]).await;
    // No explicit ACL grant on purpose: the data-source gate only applies under the org-open
    // default, and a grant would make the query visible and defeat the baseline.

    // Restrict the viewer's data-source reach via an allowlist (groups default unrestricted).
    use arcodash_core::entities::{group, user};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    let viewer_row = user::Entity::find_by_id(viewer_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let cap_group_id = *viewer_row.groups.last().unwrap();
    let cap_group = group::Entity::find_by_id(cap_group_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: group::ActiveModel = cap_group.into();
    active.data_source_access_restricted = Set(true);
    active.update(&app.db).await.unwrap();

    let query_get = viewer
        .get(format!("{}/api/queries/{query_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        query_get.status().as_u16(),
        404,
        "baseline: GET /api/queries/<id> must already 404 with no data-source reach"
    );
    let query_body: serde_json::Value = query_get.json().await.unwrap_or_default();

    // Run history and the schedule list must 404 identically, not leak data the query route
    // refuses.
    let runs_resp = viewer
        .get(format!(
            "{}/api/runs?entity_type=query&entity_id={query_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        runs_resp.status().as_u16(),
        404,
        "GET /api/runs must not be a side door around data-source access"
    );
    let runs_body: serde_json::Value = runs_resp.json().await.unwrap_or_default();
    assert_eq!(
        runs_body["message"], query_body["message"],
        "must be indistinguishable from the query route's own 404"
    );

    let schedules_resp = viewer
        .get(format!(
            "{}/api/schedules?entity_type=query&entity_id={query_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        schedules_resp.status().as_u16(),
        404,
        "GET /api/schedules must not be a side door around data-source access"
    );
    let schedules_body: serde_json::Value = schedules_resp.json().await.unwrap_or_default();
    assert_eq!(
        schedules_body["message"], query_body["message"],
        "must be indistinguishable from the query route's own 404"
    );

    let admin_runs = admin
        .get(format!(
            "{}/api/runs?entity_type=query&entity_id={query_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        admin_runs.status().as_u16(),
        200,
        "admin with real data-source reach must still read run history"
    );

    let admin_schedules = admin
        .get(format!(
            "{}/api/schedules?entity_type=query&entity_id={query_id}",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        admin_schedules.status().as_u16(),
        200,
        "admin with real data-source reach must still read the schedule list"
    );

    // `EntityAction::RunNow` shares the query-arm lookup but has no HTTP route for queries, so it
    // is exercised directly and must be denied for the same reason.
    use arcodash_backend::services::schedule_service::{authorize_entity, EntityAction};
    let run_now_denied = authorize_entity(
        &app.db,
        &viewer_row,
        "query",
        query_id,
        EntityAction::RunNow,
    )
    .await;
    assert!(
        run_now_denied.is_err(),
        "RunNow must also be denied without data-source reach"
    );
}

/// The untargeted branch must apply the same data-source gate as the targeted one
/// (`authorize_entity`/`can_view_object`), so a caller denied the query never sees its schedule or
/// `last_run.error`.
#[tokio::test]
async fn untargeted_schedule_list_hides_a_query_the_caller_cannot_see_on_the_data_source_axis() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id, true).await;
    let org_id = default_org_id(&app.db).await;

    let (viewer_id, viewer) = onboard_user(
        &app,
        &admin,
        "untargeted-sched-viewer@test.local",
        "V13w3rP@ss",
    )
    .await;
    grant_capability(&app.db, viewer_id, &["view_query"]).await;

    // Restrict the viewer's data-source reach via an allowlist (groups default unrestricted).
    use arcodash_core::entities::{group, user};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    let viewer_row = user::Entity::find_by_id(viewer_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let cap_group_id = *viewer_row.groups.last().unwrap();
    let cap_group = group::Entity::find_by_id(cap_group_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: group::ActiveModel = cap_group.into();
    active.data_source_access_restricted = Set(true);
    active.update(&app.db).await.unwrap();

    let query_get = viewer
        .get(format!("{}/api/queries/{query_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        query_get.status().as_u16(),
        404,
        "baseline: GET /api/queries/<id> must already 404 with no data-source reach"
    );

    seed_schedule_row(&app.db, org_id, "query", query_id, true, 0, None, None).await;

    let resp = viewer
        .get(format!("{}/api/schedules", app.address))
        .send()
        .await
        .unwrap();
    let body = json_status(resp, 200, "untargeted schedule list").await;
    let entity_ids: Vec<i64> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["entity_id"].as_i64().unwrap())
        .collect();
    assert!(
        !entity_ids.contains(&query_id),
        "a schedule for a query the caller cannot see on the data-source axis must not appear \
         in the untargeted list: {body:?}"
    );
}

/// Same guard for a monitor row: resolve the monitor's parent query, like `can_view_object`'s
/// monitor arm.
#[tokio::test]
async fn untargeted_schedule_list_hides_a_monitor_whose_query_is_unreachable() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id, true).await;
    let monitor_id = create_monitor(&admin, &app.address, query_id).await;

    let (viewer_id, viewer) = onboard_user(
        &app,
        &admin,
        "untargeted-sched-monitor-viewer@test.local",
        "V13w3rP@ss",
    )
    .await;
    grant_capability(&app.db, viewer_id, &["view_query", "view_monitor"]).await;

    use arcodash_core::entities::{group, user};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    let viewer_row = user::Entity::find_by_id(viewer_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let cap_group_id = *viewer_row.groups.last().unwrap();
    let cap_group = group::Entity::find_by_id(cap_group_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut active: group::ActiveModel = cap_group.into();
    active.data_source_access_restricted = Set(true);
    active.update(&app.db).await.unwrap();

    // `create_monitor` already inserts a default schedule (and `(entity_type, entity_id)` is
    // unique), so none is seeded here.

    let resp = viewer
        .get(format!("{}/api/schedules", app.address))
        .send()
        .await
        .unwrap();
    let body = json_status(resp, 200, "untargeted schedule list (monitor)").await;
    let entity_ids: Vec<i64> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["entity_id"].as_i64().unwrap())
        .collect();
    assert!(
        !entity_ids.contains(&monitor_id),
        "a schedule for a monitor whose parent query is unreachable must not appear in the \
         untargeted list: {body:?}"
    );
}

/// Positive control: the untargeted and targeted branches must not drift apart for a query the
/// caller can see.
#[tokio::test]
async fn untargeted_and_targeted_branches_agree_for_a_visible_query() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let ds_id = create_ds(&admin, &app.address).await;
    let query_id = create_query(&admin, &app.address, ds_id, true).await;
    let org_id = default_org_id(&app.db).await;

    let (viewer_id, viewer) =
        onboard_user(&app, &admin, "agree-sched-viewer@test.local", "V13w3rP@ss").await;
    grant_capability(&app.db, viewer_id, &["view_query"]).await;

    let schedule_id =
        seed_schedule_row(&app.db, org_id, "query", query_id, true, 0, None, None).await;

    let targeted = json_status(
        viewer
            .get(format!(
                "{}/api/schedules?entity_type=query&entity_id={query_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        200,
        "targeted schedule list",
    )
    .await;
    let targeted_ids: Vec<i64> = targeted
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert!(
        targeted_ids.contains(&schedule_id),
        "baseline: the targeted branch must see this schedule: {targeted:?}"
    );

    let untargeted = json_status(
        viewer
            .get(format!("{}/api/schedules", app.address))
            .send()
            .await
            .unwrap(),
        200,
        "untargeted schedule list",
    )
    .await;
    let untargeted_ids: Vec<i64> = untargeted
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert!(
        untargeted_ids.contains(&schedule_id),
        "a visible query's schedule must appear in both the targeted and untargeted branches: \
         {untargeted:?}"
    );
}

#[tokio::test]
async fn runs_list_shape_includes_notified_for_monitor_runs_only() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;
    let monitor_id = create_monitor(&client, &app.address, query_id).await;
    let org_id = default_org_id(&app.db).await;

    let qr_id = seed_query_result(&app.db, org_id, ds_id).await;
    let no_delivery_run = seed_run_row(
        &app.db,
        org_id,
        "monitor",
        monitor_id,
        "success",
        Some(qr_id),
    )
    .await;
    let with_delivery_run = seed_run_row(
        &app.db,
        org_id,
        "monitor",
        monitor_id,
        "success",
        Some(qr_id),
    )
    .await;

    let destination_id = ok_json(
        client
            .post(format!("{}/api/destinations", app.address))
            .json(&serde_json::json!({
                "name": "csv-dest", "type": "email", "options": { "to": "ops@test.local" }
            }))
            .send()
            .await
            .unwrap(),
        "create destination",
    )
    .await["id"]
        .as_i64()
        .unwrap();

    use arcodash_core::entities::notification_delivery;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};
    notification_delivery::ActiveModel {
        org_id: Set(org_id),
        monitor_id: Set(monitor_id),
        subscription_id: Set(None),
        destination_id: Set(Some(destination_id)),
        state: Set("sent".to_string()),
        status: Set("delivered".to_string()),
        error: Set(None),
        attempt_number: Set(1),
        run_id: Set(Some(with_delivery_run)),
        created_at: Set(chrono::Utc::now().into()),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();
    // A second, independently-failed delivery on the same run must not collapse into one
    // row/status.
    notification_delivery::ActiveModel {
        org_id: Set(org_id),
        monitor_id: Set(monitor_id),
        subscription_id: Set(None),
        destination_id: Set(Some(destination_id)),
        state: Set("triggered".to_string()),
        status: Set("failed".to_string()),
        error: Set(Some("connection refused".to_string())),
        attempt_number: Set(3),
        run_id: Set(Some(with_delivery_run)),
        created_at: Set(chrono::Utc::now().into()),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();

    let runs = ok_json(
        client
            .get(format!(
                "{}/api/runs?entity_type=monitor&entity_id={monitor_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list monitor runs",
    )
    .await;
    let rows = runs.as_array().unwrap();
    assert_eq!(rows.len(), 2, "both seeded runs must appear");
    assert_eq!(rows[0]["id"], with_delivery_run);
    assert_eq!(rows[1]["id"], no_delivery_run);
    let notified = rows[0]["notified"].as_array().unwrap();
    assert_eq!(
        notified.len(),
        2,
        "both independent deliveries must be returned"
    );
    let sent = notified
        .iter()
        .find(|n| n["status"] == "delivered")
        .expect("the sent delivery is present");
    assert!(sent["error"].is_null());
    let failed = notified
        .iter()
        .find(|n| n["status"] == "failed")
        .expect("the failed delivery is present");
    assert_eq!(failed["error"], "connection refused");
    assert!(rows[1]["notified"].as_array().unwrap().is_empty());

    // A query run's notified is always [].
    let query_run_id =
        seed_run_row(&app.db, org_id, "query", query_id, "success", Some(qr_id)).await;
    let query_runs = ok_json(
        client
            .get(format!(
                "{}/api/runs?entity_type=query&entity_id={query_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list query runs",
    )
    .await;
    let qrows = query_runs.as_array().unwrap();
    let this_run = qrows
        .iter()
        .find(|r| r["id"].as_i64() == Some(query_run_id))
        .unwrap();
    assert!(this_run["notified"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn runs_list_caps_at_ten_newest_first() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;
    let org_id = default_org_id(&app.db).await;

    let mut ids = Vec::new();
    for _ in 0..12 {
        ids.push(seed_run_row(&app.db, org_id, "query", query_id, "success", None).await);
    }

    let runs = ok_json(
        client
            .get(format!(
                "{}/api/runs?entity_type=query&entity_id={query_id}",
                app.address
            ))
            .send()
            .await
            .unwrap(),
        "list runs capped",
    )
    .await;
    let rows = runs.as_array().unwrap();
    assert_eq!(rows.len(), 10);
    let expected_newest_ten: Vec<i64> = ids.iter().rev().take(10).copied().collect();
    let actual_ids: Vec<i64> = rows.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert_eq!(actual_ids, expected_newest_ten);
}

#[tokio::test]
async fn csv_download_happy_path() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;
    let org_id = default_org_id(&app.db).await;

    let qr_id = seed_query_result(&app.db, org_id, ds_id).await;
    let run_id = seed_run_row(&app.db, org_id, "query", query_id, "success", Some(qr_id)).await;

    let resp = client
        .get(format!("{}/api/runs/{run_id}/result.csv", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let content_disposition = resp
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_disposition.contains("schedule-test-query"),
        "filename should be slugged from the query name: {content_disposition}"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("count"));
    assert!(body.contains('1'));
}

#[tokio::test]
async fn csv_download_404s_when_result_is_gone_or_never_existed() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;
    let org_id = default_org_id(&app.db).await;

    // A run whose result pointer is NULL (e.g. a failed run).
    let failed_run_id = seed_run_row(&app.db, org_id, "query", query_id, "failed", None).await;
    let resp = client
        .get(format!(
            "{}/api/runs/{failed_run_id}/result.csv",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "failed run has no result");

    // A run whose result row has since been deleted (simulates GC).
    let qr_id = seed_query_result(&app.db, org_id, ds_id).await;
    let run_id = seed_run_row(&app.db, org_id, "query", query_id, "success", Some(qr_id)).await;
    use arcodash_core::entities::query_result;
    use sea_orm::EntityTrait;
    query_result::Entity::delete_by_id(qr_id)
        .exec(&app.db)
        .await
        .unwrap();
    let resp = client
        .get(format!("{}/api/runs/{run_id}/result.csv", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "GC'd result must 404");
}

#[tokio::test]
async fn consumers_counts_tiles_dashboards_and_monitors() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;
    let ds_id = create_ds(&client, &app.address).await;
    let query_id = create_query(&client, &app.address, ds_id, true).await;

    let viz1 = ok_json(
        client
            .post(format!("{}/api/visualizations", app.address))
            .json(&serde_json::json!({ "query_id": query_id, "type": "TABLE", "name": "v1", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create viz1",
    )
    .await["id"]
        .as_i64()
        .unwrap();
    let viz2 = ok_json(
        client
            .post(format!("{}/api/visualizations", app.address))
            .json(&serde_json::json!({ "query_id": query_id, "type": "CHART", "name": "v2", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create viz2",
    )
    .await["id"]
        .as_i64()
        .unwrap();

    let dash1_id = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Dash 1" }))
            .send()
            .await
            .unwrap(),
        "create dash1",
    )
    .await["id"]
        .as_i64()
        .unwrap();
    let dash2_id = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Dash 2" }))
            .send()
            .await
            .unwrap(),
        "create dash2",
    )
    .await["id"]
        .as_i64()
        .unwrap();

    // 3 widgets: 2 on dashboard 1 (one per visualization), 1 on dashboard 2.
    for (dashboard_id, viz_id) in [(dash1_id, viz1), (dash1_id, viz2), (dash2_id, viz1)] {
        let resp = client
            .post(format!("{}/api/widgets", app.address))
            .json(&serde_json::json!({ "dashboard_id": dashboard_id, "visualization_id": viz_id }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            200,
            "create widget: {}",
            resp.text().await.unwrap_or_default()
        );
    }

    create_monitor(&client, &app.address, query_id).await;

    let consumers = ok_json(
        client
            .get(format!("{}/api/queries/{query_id}/consumers", app.address))
            .send()
            .await
            .unwrap(),
        "consumers",
    )
    .await;
    assert_eq!(consumers["tile_count"], 3);
    assert_eq!(consumers["dashboard_count"], 2);
    assert_eq!(consumers["monitor_count"], 1);
}

#[tokio::test]
async fn consumers_404s_on_a_nonexistent_query_even_as_admin() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let resp = client
        .get(format!("{}/api/queries/999999999/consumers", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}
