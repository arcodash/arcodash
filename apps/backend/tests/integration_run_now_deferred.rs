//! `run_service::run_now` must not dispatch a job to a worker/conveyor until the `runs` row it
//! references has actually committed — under `Db::Txn` (a per-request transaction),
//! the queue push is deferred via `Db::after_commit_fallible` rather than run inline. Verified
//! directly: nothing is sent to the queue before the transaction's post-commit jobs are run, and
//! exactly one message is sent once they are.

mod common;
use common::spawn_app;

use std::sync::{Arc, Mutex};

use arcodash_backend::db::{Db, PostCommitJobs};
use arcodash_core::entities::{organization, run};
use arcodash_core::queue::{JobMessage, Queue, QueueName};
use arcodash_core::test_support::{new_data_source, new_query, new_user};
use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, ActiveValue::NotSet, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter,
    TransactionTrait,
};

#[derive(Default)]
struct RecordingQueue {
    sent: Mutex<Vec<(QueueName, JobMessage)>>,
}

#[async_trait]
impl Queue for RecordingQueue {
    async fn enqueue(&self, queue: QueueName, job: JobMessage) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push((queue, job));
        Ok(())
    }
}

async fn seed_org(db: &sea_orm::DatabaseConnection, label: &str) -> i64 {
    organization::Entity::insert(organization::ActiveModel {
        id: NotSet,
        name: Set(format!("{label} org")),
        slug: Set(format!("{label}-{}", uuid::Uuid::new_v4().simple())),
        settings: Set(serde_json::json!({})),
        created_at: NotSet,
        updated_at: NotSet,
    })
    .exec(db)
    .await
    .expect("org insert failed")
    .last_insert_id
}

/// Deferred ordering: `run_now` under `Db::Txn` must return successfully with
/// the queue push still un-dispatched, and only actually dispatch once the transaction's deferred
/// post-commit jobs are run.
#[tokio::test]
async fn run_now_under_db_txn_defers_the_queue_push_until_post_commit_jobs_run() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
        eprintln!("skipping: TEST_REDIS_URL not set");
        return;
    };
    let redis = arcodash_core::valkey_client::connect(&redis_url)
        .await
        .unwrap();

    let org_id = seed_org(&app.db, "run-now-deferred").await;
    let ds = new_data_source(org_id).insert(&app.db).await.unwrap();
    let mut u = new_user(org_id);
    u.email = Set(format!(
        "run-now-deferred-{}@test.local",
        uuid::Uuid::new_v4()
    ));
    let user = u.insert(&app.db).await.unwrap();
    let query = new_query(org_id, ds.id, user.id)
        .insert(&app.db)
        .await
        .unwrap();

    let jobs = PostCommitJobs::default();
    let txn = app.db.begin().await.expect("begin failed");
    let db_txn = Db::Txn(Arc::new(txn), jobs.clone());

    let recording_queue = Arc::new(RecordingQueue::default());

    let result = arcodash_backend::services::run_service::run_now(
        &db_txn,
        recording_queue.clone(),
        None,
        redis.clone(),
        app.db.clone(),
        org_id,
        user.id,
        "query",
        query.id,
        Some(ds.id),
    )
    .await;
    assert!(
        result.is_ok(),
        "run_now under Db::Txn must return Ok before the deferred job has run: {result:?}"
    );

    // (b) Nothing dispatched yet — the enqueue is deferred, not inline.
    assert_eq!(
        recording_queue.sent.lock().unwrap().len(),
        0,
        "the queue push must not happen before post-commit jobs run"
    );

    // Commit the underlying transaction so the `runs` row this test just wrote is durable —
    // mirrors what the owning middleware does before running post-commit jobs for real.
    let Db::Txn(arc_txn, _) = db_txn else {
        unreachable!("db_txn was constructed as Db::Txn above")
    };
    let txn = Arc::try_unwrap(arc_txn).unwrap_or_else(|_| panic!("txn still shared"));
    txn.commit().await.expect("commit failed");

    // Now run the deferred jobs — mirrors `middleware::org::inject_current_org`'s post-commit step.
    jobs.run_all().await;

    // (c) Exactly one message reached the queue. Scoped so the guard drops before the `.await`
    // below — holding a std Mutex guard across an await point is a deadlock risk under a
    // multi-threaded runtime.
    {
        let sent = recording_queue.sent.lock().unwrap();
        assert_eq!(
            sent.len(),
            1,
            "exactly one message must be dispatched once post-commit jobs run: {sent:?}"
        );
        assert!(
            matches!(&sent[0].1, JobMessage::ExecuteQuery { query_id: Some(id), .. } if *id == query.id),
            "the dispatched message must be this query's ExecuteQuery: {:?}",
            sent[0]
        );
    }

    // The `runs` row itself (a pre-commit write) must be visible now that the transaction
    // committed, with `enqueued_at` stamped (also pre-commit, per this function's ordering rule).
    let saved_run = run::Entity::find()
        .filter(run::Column::OrgId.eq(org_id))
        .filter(run::Column::EntityType.eq("query"))
        .filter(run::Column::EntityId.eq(query.id))
        .one(&app.db)
        .await
        .unwrap()
        .expect("the runs row must have committed");
    assert!(
        saved_run.enqueued_at.is_some(),
        "enqueued_at must be stamped pre-commit, atomically with the runs row"
    );
}
