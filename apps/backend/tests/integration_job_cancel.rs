//! Integration tests: `GET`/`DELETE /api/jobs/<id>` (`job_service::{get_status,cancel}`).

mod common;
use common::{api_key_client, default_org_id};
use common::{spawn_app, user_with_role};

use arcodash_core::queue::{self, JobStatus};

/// The most recent event row for `(org_id, action)`, if any.
async fn latest_event(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    action: &str,
) -> Option<arcodash_core::entities::event::Model> {
    use sea_orm::{ColumnTrait, EntityTrait, Order, QueryFilter, QueryOrder};
    arcodash_core::entities::event::Entity::find()
        .filter(arcodash_core::entities::event::Column::OrgId.eq(org_id))
        .filter(arcodash_core::entities::event::Column::Action.eq(action))
        .order_by(
            arcodash_core::entities::event::Column::CreatedAt,
            Order::Desc,
        )
        .one(db)
        .await
        .unwrap()
}

/// Cancelling an in-flight job flips its status to `Cancelled` so a caller polling
/// `GET /api/jobs/<id>` right after sees the cancellation reflected immediately.
#[tokio::test]
async fn cancel_marks_an_in_flight_job_cancelled() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let org_id = default_org_id(&app.db).await;
    let job_id = format!("cancel-test-{}", uuid::Uuid::new_v4());
    queue::set_job_status(
        &app.redis,
        &job_id,
        org_id,
        None,
        JobStatus::Started,
        None,
        None,
    )
    .await
    .unwrap();

    let resp = admin
        .delete(format!("{}/api/jobs/{job_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    let record = queue::get_job_status(&app.redis, &job_id)
        .await
        .unwrap()
        .expect("job status must still exist");
    assert_eq!(record.status, JobStatus::Cancelled);

    assert!(
        latest_event(&app.db, org_id, "job.cancelled")
            .await
            .is_some(),
        "cancelling a job must record a job.cancelled event"
    );
}

/// Cancelling a job that already reached a terminal state (e.g. the worker finished before the
/// cancel request landed) must NOT overwrite that terminal status — a completed job's result must
/// stay reachable, not get silently relabeled "cancelled" after the fact.
#[tokio::test]
async fn cancel_does_not_overwrite_an_already_terminal_job() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let job_id = format!("cancel-terminal-test-{}", uuid::Uuid::new_v4());
    queue::set_job_status(
        &app.redis,
        &job_id,
        1,
        None,
        JobStatus::Success,
        Some(42),
        None,
    )
    .await
    .unwrap();

    let resp = admin
        .delete(format!("{}/api/jobs/{job_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "cancel is still a no-op success, not an error, for an already-terminal job"
    );

    let record = queue::get_job_status(&app.redis, &job_id)
        .await
        .unwrap()
        .expect("job status must still exist");
    assert_eq!(
        record.status,
        JobStatus::Success,
        "an already-successful job must not be relabeled cancelled"
    );
    assert_eq!(
        record.query_result_id,
        Some(42),
        "the original result pointer must survive an ignored cancel"
    );
}

/// A job belonging to a different org must read as not-found, and cancel must not touch it —
/// same org-isolation contract `get_status` already enforces.
#[tokio::test]
async fn cancel_of_another_orgs_job_is_not_found_and_leaves_it_untouched() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;

    let job_id = format!("cancel-cross-org-test-{}", uuid::Uuid::new_v4());
    let foreign_org_id = 999_999;
    queue::set_job_status(
        &app.redis,
        &job_id,
        foreign_org_id,
        None,
        JobStatus::Started,
        None,
        None,
    )
    .await
    .unwrap();

    let resp = admin
        .delete(format!("{}/api/jobs/{job_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    let record = queue::get_job_status(&app.redis, &job_id)
        .await
        .unwrap()
        .expect("job status must still exist");
    assert_eq!(
        record.status,
        JobStatus::Started,
        "a cross-org cancel attempt must not touch the job's actual status"
    );
}

/// A same-org caller who is neither the job's submitter nor an admin must be unable to read or
/// cancel it — same not-found treatment as a cross-org caller. The submitter and an admin must
/// both still succeed.
#[tokio::test]
async fn same_org_non_submitter_non_admin_cannot_read_or_cancel_another_users_job() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;
    let admin = app.admin_client().await;

    let (submitter, _) = user_with_role(&app.db, org_id, &[]).await;
    let submitter_client = api_key_client(&common::test_api_key(&submitter.email));
    let (stranger, _) = user_with_role(&app.db, org_id, &[]).await;
    let stranger_client = api_key_client(&common::test_api_key(&stranger.email));

    let job_id = format!("cancel-ownership-test-{}", uuid::Uuid::new_v4());
    queue::set_job_status(
        &app.redis,
        &job_id,
        org_id,
        Some(submitter.id),
        JobStatus::Started,
        None,
        None,
    )
    .await
    .unwrap();

    let get_resp = stranger_client
        .get(format!("{}/api/jobs/{job_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        get_resp.status().as_u16(),
        404,
        "a same-org non-submitter, non-admin must not be able to read the job"
    );

    let cancel_resp = stranger_client
        .delete(format!("{}/api/jobs/{job_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        cancel_resp.status().as_u16(),
        404,
        "a same-org non-submitter, non-admin must not be able to cancel the job"
    );
    let record = queue::get_job_status(&app.redis, &job_id)
        .await
        .unwrap()
        .expect("job status must still exist");
    assert_eq!(
        record.status,
        JobStatus::Started,
        "the stranger's cancel attempt must not have touched the job"
    );

    let submitter_get = submitter_client
        .get(format!("{}/api/jobs/{job_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        submitter_get.status().as_u16(),
        200,
        "the submitter must be able to read their own job"
    );

    let admin_cancel = admin
        .delete(format!("{}/api/jobs/{job_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        admin_cancel.status().as_u16(),
        204,
        "an admin must be able to cancel a job submitted by someone else in the org"
    );
}

/// A job status record without a `user_id` (`None`) keeps org-only behavior — any same-org
/// caller can read/cancel it — so records already in flight don't lock anyone out.
#[tokio::test]
async fn a_pre_upgrade_record_with_no_user_id_stays_org_scoped_only() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let org_id = default_org_id(&app.db).await;

    let (someone, _) = user_with_role(&app.db, org_id, &[]).await;
    let someone_client = api_key_client(&common::test_api_key(&someone.email));

    let job_id = format!("cancel-legacy-record-test-{}", uuid::Uuid::new_v4());
    queue::set_job_status(
        &app.redis,
        &job_id,
        org_id,
        None,
        JobStatus::Started,
        None,
        None,
    )
    .await
    .unwrap();

    let get_resp = someone_client
        .get(format!("{}/api/jobs/{job_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        get_resp.status().as_u16(),
        200,
        "any same-org caller must still read a user_id-less legacy record"
    );

    let cancel_resp = someone_client
        .delete(format!("{}/api/jobs/{job_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(
        cancel_resp.status().as_u16(),
        204,
        "any same-org caller must still be able to cancel a user_id-less legacy record"
    );
}
