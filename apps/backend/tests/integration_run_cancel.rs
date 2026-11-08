//! Integration tests: `DELETE /api/runs/<id>` (`runs::cancel`).

mod common;
use common::{api_key_client, ok_json};
use common::{make_org, spawn_app, user_with_role};

use arcodash_core::queue::JobStatus;

/// Create a data source and a published query under it; returns `(data_source_id, query_id)`.
async fn seed_published_query(
    client: &reqwest::Client,
    address: &str,
    ds_name: &str,
) -> (i64, i64) {
    let ds = ok_json(
        client
            .post(format!("{address}/api/data_sources"))
            .json(&serde_json::json!({ "name": ds_name, "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    let q = ok_json(
        client
            .post(format!("{address}/api/queries"))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "cancel-run-query",
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
        client
            .post(format!("{address}/api/queries/{query_id}/publish"))
            .json(&serde_json::json!({ "expected_version": 0 }))
            .send()
            .await
            .unwrap(),
        "publish query",
    )
    .await;

    (ds_id, query_id)
}

/// `POST /api/queries/<id>/run` → `DELETE /api/runs/<run_id>` → 204, and the underlying job's
/// status in Redis flips to Cancelled.
#[tokio::test]
async fn cancel_flips_the_underlying_job_to_cancelled() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let (_ds_id, query_id) = seed_published_query(&client, &app.address, "cancel-run-ds").await;

    let run_resp = client
        .post(format!("{}/api/queries/{query_id}/run", app.address))
        .send()
        .await
        .unwrap();
    let run_status = run_resp.status().as_u16();
    let run_body_text = run_resp.text().await.unwrap_or_default();
    assert!(
        run_status == 200 || run_status == 201,
        "run should start: {run_status} {run_body_text}"
    );
    let run_body: serde_json::Value = serde_json::from_str(&run_body_text).unwrap();
    let run_id = run_body["id"]
        .as_i64()
        .expect("run response must include id");

    let cancel_resp = client
        .delete(format!("{}/api/runs/{run_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        cancel_resp.status().as_u16(),
        204,
        "cancelling a fresh run must succeed"
    );

    // The run row's job_id is the Redis job status key.
    use arcodash_core::entities::run as run_entity;
    use sea_orm::EntityTrait;
    let run_row = run_entity::Entity::find_by_id(run_id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("run row must exist");
    let job_id = run_row.job_id.expect("run must have a job_id");

    let record = arcodash_core::queue::get_job_status(&app.redis, &job_id)
        .await
        .unwrap()
        .expect("job status must exist");
    assert_eq!(record.status, JobStatus::Cancelled);
}

/// A user with only Read reach on the query (no execute capability, so `RunNow` fails) must not
/// be able to cancel a run on it.
#[tokio::test]
async fn cancel_by_a_user_with_only_read_reach_is_denied() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let (_ds_id, query_id) =
        seed_published_query(&admin, &app.address, "cancel-read-only-ds").await;

    let run_resp = admin
        .post(format!("{}/api/queries/{query_id}/run", app.address))
        .send()
        .await
        .unwrap();
    let run_body: serde_json::Value = run_resp.json().await.unwrap();
    let run_id = run_body["id"].as_i64().unwrap();

    let org_id = admin_org_id(&app.db).await;
    // No capabilities at all -> fails both view_query and execute_query, so `RunNow` denies.
    let (stranger, _) = user_with_role(&app.db, org_id, &[]).await;
    let stranger_client = api_key_client(&common::test_api_key(&stranger.email));

    let resp = stranger_client
        .delete(format!("{}/api/runs/{run_id}", app.address))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert!(
        status == 403 || status == 404,
        "a caller without RunNow-tier access must be denied, got {status}"
    );
}

async fn admin_org_id(db: &sea_orm::DatabaseConnection) -> i64 {
    use arcodash_core::entities::organization;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    organization::Entity::find()
        .filter(organization::Column::Slug.eq("default"))
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .id
}

/// Cancelling a run belonging to a different org must read as not-found. The plain OSS test
/// harness (`spawn_app()`) resolves every API-key-authenticated request to the single seeded
/// `default` org, so a genuinely different org's *user* can never authenticate here (see
/// `integration_data_sources.rs`'s `HeaderOrgResolver` note) — instead, the run row itself is
/// seeded directly under a fresh org while the acting caller is the `default`-org admin,
/// proving the same org-scoping the handler's `run.filter(|r| r.org_id == user.org_id)` enforces.
#[tokio::test]
async fn cancel_of_another_orgs_run_is_not_found() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let foreign_org_id = make_org(&app.db).await;
    let (foreign_owner, _) =
        user_with_role(&app.db, foreign_org_id, &["edit_query", "execute_query"]).await;
    let ds = arcodash_core::test_support::new_data_source(foreign_org_id)
        .insert(&app.db)
        .await
        .unwrap();
    let query = arcodash_core::test_support::new_query(foreign_org_id, ds.id, foreign_owner.id)
        .insert(&app.db)
        .await
        .unwrap();

    use arcodash_core::entities::run as run_entity;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};
    let now = chrono::Utc::now();
    let foreign_run = run_entity::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        org_id: Set(foreign_org_id),
        entity_type: Set("query".to_string()),
        entity_id: Set(query.id),
        schedule_id: Set(None),
        data_source_id: Set(Some(ds.id)),
        scheduled_for: Set(None),
        triggered_by: Set("manual".to_string()),
        status: Set("pending".to_string()),
        waiting_on_run_id: Set(None),
        job_id: Set(Some(format!("cross-org-job-{}", uuid::Uuid::new_v4()))),
        worker_id: Set(None),
        lease_expires_at: Set(None),
        started_at: Set(None),
        finished_at: Set(None),
        duration_ms: Set(None),
        row_count: Set(None),
        error: Set(None),
        data_age_secs: Set(None),
        query_result_id: Set(None),
        condition_met: Set(None),
        created_at: Set(now.into()),
        enqueued_at: sea_orm::ActiveValue::NotSet,
    }
    .insert(&app.db)
    .await
    .unwrap();

    let resp = admin
        .delete(format!("{}/api/runs/{}", app.address, foreign_run.id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

/// A run with a NULL `job_id` (shouldn't normally happen for a manual run, but the handler must
/// still fail cleanly rather than panicking) → 404 "no cancellable job".
#[tokio::test]
async fn cancel_of_a_run_with_no_job_id_returns_404() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let (_ds_id, query_id) = seed_published_query(&admin, &app.address, "cancel-no-job-ds").await;

    use arcodash_core::entities::run as run_entity;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};

    let org_id = admin_org_id(&app.db).await;
    let now = chrono::Utc::now();
    let created = run_entity::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        org_id: Set(org_id),
        entity_type: Set("query".to_string()),
        entity_id: Set(query_id),
        schedule_id: Set(None),
        data_source_id: Set(None),
        scheduled_for: Set(None),
        triggered_by: Set("manual".to_string()),
        status: Set("pending".to_string()),
        waiting_on_run_id: Set(None),
        job_id: Set(None),
        worker_id: Set(None),
        lease_expires_at: Set(None),
        started_at: Set(None),
        finished_at: Set(None),
        duration_ms: Set(None),
        row_count: Set(None),
        error: Set(None),
        data_age_secs: Set(None),
        query_result_id: Set(None),
        condition_met: Set(None),
        created_at: Set(now.into()),
        enqueued_at: sea_orm::ActiveValue::NotSet,
    }
    .insert(&app.db)
    .await
    .unwrap();

    let resp = admin
        .delete(format!("{}/api/runs/{}", app.address, created.id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}
