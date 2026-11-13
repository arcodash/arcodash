//! `SendMail` handler: a generic, durable one-off email send, sharing the `mail_deliveries`
//! claim/heartbeat/finalize lifecycle with `send_digest.rs`. The producer entry point is
//! `arcodash_core::mail_notify::enqueue_send_mail`; no in-app caller uses it yet.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;

use arcodash_conveyor::{TaskContext, TaskOutcome, TaskResult, TypedTaskHandler};
use arcodash_core::entities::mail_delivery;
use arcodash_core::mail_notify::{load_mail_body, SendMailPayload};
use arcodash_core::mailer::MailOutcome;
use arcodash_core::org_storage::StorageError;
use sea_orm::EntityTrait;

use crate::delivery_lifecycle::{self, MAIL_DELIVERIES};
use crate::handlers::execute_query::{send_email, ExecuteQueryContext};

/// Thin wrapper over [`deliver`] for the Redis-queue dispatch path — logs and swallows any error
/// `deliver` propagates (a genuine infra/claim failure; every other outcome, including a failed
/// send, is recorded on the delivery row itself and returns `Ok`).
pub async fn handle(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    to: String,
    subject: String,
    body_html: String,
    delivery_id: Option<i64>,
) {
    if let Err(e) = deliver(ctx, org_id, to, subject, body_html, delivery_id).await {
        tracing::error!(org_id, error = %e, "send_mail failed");
    }
}

/// Runs `send` for the pre-created `mail_deliveries` row `id` under a lease heartbeat, then
/// finalizes the row with its outcome. `Ok(None)` means the row wasn't claimable (already
/// running/terminal): a duplicate delivery of the same Redis message, so `send` never ran. `Err`
/// is only a claim failure; the send outcome lives on the row (and in the returned result).
pub async fn send_claimed(
    ctx: &ExecuteQueryContext,
    id: i64,
    send: impl Future<Output = anyhow::Result<MailOutcome>>,
) -> anyhow::Result<Option<anyhow::Result<MailOutcome>>> {
    if !delivery_lifecycle::claim(&ctx.db, MAIL_DELIVERIES, id).await? {
        return Ok(None);
    }
    let heartbeat = delivery_lifecycle::spawn_heartbeat(ctx.db.clone(), MAIL_DELIVERIES, id);
    let result = send.await;
    drop(heartbeat);

    let (status, error) = match &result {
        Ok(MailOutcome::Sent) => ("sent", None),
        Ok(MailOutcome::Skipped) => (
            "skipped",
            Some("SMTP_HOST is not configured; no email was sent".to_string()),
        ),
        Err(e) => ("failed", Some(e.to_string())),
    };
    if let Err(e) =
        delivery_lifecycle::finalize(&ctx.db, MAIL_DELIVERIES, id, status, error, None, true).await
    {
        tracing::error!(delivery_id = id, error = %e, "mail delivery: failed to record outcome");
    }
    Ok(Some(result))
}

/// `delivery_id` (when `Some`) is the pre-created `mail_deliveries` row's own id, finalized in
/// place via [`send_claimed`]. `None` is a pre-durability in-flight message: sends directly, no
/// claim/finalize. Returns `Err` only for a genuine claim/DB failure — a duplicate delivery and a
/// failed send are both `Ok(())`.
pub async fn deliver(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    to: String,
    subject: String,
    body_html: String,
    delivery_id: Option<i64>,
) -> anyhow::Result<()> {
    // Simplification: `send_email` sends `body_html` as plain text (no real HTML MIME path exists
    // yet) — acceptable until a producer needs a real HTML content-type.
    let send = send_email(&ctx.mailer, &ctx.mail_from, &to, &subject, &body_html);
    let result = match delivery_id {
        None => send.await,
        Some(id) => match send_claimed(ctx, id, send).await? {
            Some(result) => result,
            None => return Ok(()),
        },
    };
    if let Err(e) = result {
        tracing::error!(org_id, error = %e, "send_mail failed");
    }
    Ok(())
}

/// Adapts `deliver` onto the conveyor SDK — same shape as `send_digest::SendDigestHandler`.
pub struct SendMailHandler {
    pub ctx: Arc<ExecuteQueryContext>,
}

#[async_trait]
impl TypedTaskHandler for SendMailHandler {
    type Payload = SendMailPayload;

    async fn run(&self, task: TaskContext, p: SendMailPayload) -> TaskResult {
        let body_html = match p.body_html {
            Some(b) => b,
            None => match self.load_stored_body(&task, p.org_id).await {
                Ok(b) => b,
                Err(outcome) => return Ok(outcome),
            },
        };
        let result = deliver(
            &self.ctx,
            p.org_id,
            p.to,
            p.subject,
            body_html,
            task.entity_id,
        )
        .await;
        Ok(match result {
            Ok(()) => TaskOutcome::Success { result_ref: None },
            Err(e) => TaskOutcome::Failed {
                error: arcodash_core::scrub::scrub_error_values(&format!("{e:#}")),
            },
        })
    }
}

impl SendMailHandler {
    /// The body stored for the task's delivery row; `Err` carries the outcome to return instead.
    async fn load_stored_body(
        &self,
        task: &TaskContext,
        org_id: i64,
    ) -> Result<String, TaskOutcome> {
        let failed = |error: String| TaskOutcome::Failed { error };
        let Some(id) = task.entity_id else {
            return Err(failed("send_mail task has no delivery id".to_string()));
        };
        let ctx = &self.ctx;
        let row = mail_delivery::Entity::find_by_id(id)
            .one(&ctx.db)
            .await
            .map_err(|e| failed(format!("{e:#}")))?
            .ok_or_else(|| failed(format!("mail delivery {id} not found")))?;
        let storage = ctx
            .storage
            .resolve(&ctx.db, org_id)
            .await
            .map_err(|e| failed(e.to_string()))?;
        match load_mail_body(&storage, &row).await {
            Ok(b) => Ok(b),
            Err(StorageError::NotFound) => {
                mail_delivery::fail_body_missing(&ctx.db, id).await;
                Err(failed("mail body not found in storage".to_string()))
            }
            Err(e) => {
                if matches!(e, StorageError::Unreachable { .. }) {
                    ctx.storage.report_failure(&ctx.db, org_id, &e).await;
                }
                Err(failed(e.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Real Postgres + Redis only (skips cleanly without `TEST_DATABASE_URL`/`TEST_REDIS_URL`).
    //! Exercises the handler/table/sweep end to end through `enqueue_send_mail`.
    use super::*;
    use crate::scheduler::test_support::*;
    use arcodash_core::entities::mail_delivery;
    use arcodash_core::mail_notify::enqueue_send_mail;
    use arcodash_core::queue::ValkeyQueue;
    use redis::AsyncCommands;
    use sea_orm::ConnectionTrait;

    /// `enqueue_send_mail` inserts the durable row before the LPUSH, then a real dispatch through
    /// `consumer::dispatch` claims it. No mailer is configured in this test process, so
    /// `send_email` reports `Skipped`, not `Sent` — this proves the delivery row is finalized
    /// `"skipped"` (not `"sent"`) when there's genuinely nothing to send with.
    #[tokio::test]
    async fn enqueue_send_mail_then_real_dispatch_delivers_and_finalizes() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-send-mail-{suffix}")).await;
        let ctx = test_ctx(db.clone(), redis.clone());
        let queue = ValkeyQueue::new(redis.clone());

        let key = arcodash_core::queue::QueueName::Default.queue_key();
        let mut conn = redis.clone();
        while conn
            .lpop::<_, Option<String>>(&key, None)
            .await
            .unwrap()
            .is_some()
        {}

        let storage = ctx.storage.resolve(&db, org.id).await.unwrap();
        let delivery_id = enqueue_send_mail(
            &db,
            &storage,
            &queue,
            None,
            org.id,
            format!("send-mail-test-{suffix}@test.local"),
            "hi".to_string(),
            "hello".to_string(),
        )
        .await
        .unwrap();

        let row = mail_delivery::Entity::find_by_id(delivery_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "pending");
        assert_eq!(row.task_type, "send_mail");

        let payload: String = conn
            .lpop::<_, Option<String>>(&key, None)
            .await
            .unwrap()
            .expect("enqueue_send_mail must LPUSH a real SendMail job");
        let message: arcodash_core::queue::JobMessage = serde_json::from_str(&payload).unwrap();

        crate::consumer::dispatch(&ctx, message).await;

        let final_row = mail_delivery::Entity::find_by_id(delivery_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(final_row.status, "skipped");
        assert!(final_row.finished_at.is_some());
        assert!(final_row.lease_expires_at.is_none());
    }

    /// Real dispatch through a `Conveyor` (not `deliver` called directly): proves
    /// `SendMailHandler`'s wiring end to end, same shape as
    /// `send_digest`'s `conveyor_dispatch_delivers_and_finalizes_the_digest_row`.
    #[tokio::test]
    async fn conveyor_dispatch_delivers_and_finalizes_the_mail_row() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let redis_url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL required");
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-mail-conveyor-{suffix}")).await;
        let ctx = Arc::new(test_ctx(db.clone(), redis.clone()));

        let delivery_id =
            mail_delivery::insert_pending_mail(&db, org.id, "to@test.local", "subject")
                .await
                .unwrap();

        let conveyor = arcodash_conveyor::ConveyorBuilder::new(db.clone(), redis, redis_url)
            .register("send_mail", SendMailHandler { ctx: ctx.clone() })
            .tier("email", &["send_mail"], 4)
            .build();

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let run_handle = {
            let conveyor = conveyor.clone();
            tokio::spawn(async move { conveyor.run(shutdown_rx).await })
        };

        let payload = serde_json::to_value(SendMailPayload {
            org_id: org.id,
            to: "to@test.local".to_string(),
            subject: "subject".to_string(),
            body_html: Some("body".to_string()),
        })
        .unwrap();

        conveyor
            .enqueue(arcodash_conveyor::EnqueueRequest {
                task_type: "send_mail".into(),
                payload,
                tenant_id: Some(org.id),
                entity_id: Some(delivery_id),
                resource_key: None,
                resource_cap: None,
            })
            .await
            .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let row = loop {
            let row = mail_delivery::Entity::find_by_id(delivery_id)
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            if row.finished_at.is_some() {
                break row;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "conveyor never delivered the task within 5s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };

        let _ = shutdown_tx.send(true);
        run_handle.await.unwrap();

        assert_eq!(row.status, "skipped");
        assert!(row.finished_at.is_some());
        assert!(row.lease_expires_at.is_none());
    }
    fn conveyor_for(
        db: &sea_orm::DatabaseConnection,
        redis: &redis::aio::ConnectionManager,
        ctx: &Arc<ExecuteQueryContext>,
    ) -> Arc<arcodash_conveyor::Conveyor> {
        let redis_url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL required");
        arcodash_conveyor::ConveyorBuilder::new(db.clone(), redis.clone(), redis_url)
            .register("send_mail", SendMailHandler { ctx: ctx.clone() })
            .tier("email", &["send_mail"], 4)
            .build()
    }

    #[tokio::test]
    async fn conveyor_send_mail_payload_carries_no_body() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-mail-nobody-{suffix}")).await;
        let ctx = Arc::new(test_ctx(db.clone(), redis.clone()));
        let conveyor = conveyor_for(&db, &redis, &ctx);
        let storage = ctx.storage.resolve(&db, org.id).await.unwrap();
        let marker = format!("secret-body-{suffix}");

        std::env::set_var("CONVEYOR_SEND_MAIL_ENABLED", "true");
        let enqueued = enqueue_send_mail(
            &db,
            &storage,
            &ValkeyQueue::new(redis.clone()),
            Some(&*conveyor),
            org.id,
            "to@test.local".to_string(),
            "subject".to_string(),
            marker.clone(),
        )
        .await;
        std::env::remove_var("CONVEYOR_SEND_MAIL_ENABLED");
        let delivery_id = enqueued.unwrap();

        let rows = db
            .query_all_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "SELECT payload::text AS payload FROM conveyor_tasks \
                 WHERE task_type = 'send_mail' AND entity_id = $1",
                [delivery_id.into()],
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "the delivery must have one conveyor task");
        let text: String = rows[0].try_get("", "payload").unwrap();
        let payload: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(payload.get("body_html").is_none());
        assert!(!text.contains(&marker));
    }

    #[tokio::test]
    async fn conveyor_send_mail_handler_delivers_stored_body() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let mailpit =
            std::env::var("MAILPIT_API_BASE").unwrap_or_else(|_| "http://localhost:8026".into());
        let http = reqwest::Client::new();
        if http
            .get(format!("{mailpit}/api/v1/info"))
            .send()
            .await
            .is_err()
        {
            eprintln!("skipping: Mailpit not reachable at {mailpit}");
            return;
        }
        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-mail-handler-{suffix}")).await;
        let mut ctx = test_ctx(db.clone(), redis.clone());
        ctx.mailer = Some(Arc::new(
            arcodash_core::mailer::Mailer::builder_dangerous("localhost")
                .port(1026)
                .build(),
        ));
        let ctx = Arc::new(ctx);
        let storage = ctx.storage.resolve(&db, org.id).await.unwrap();
        let to = format!("stored-body-{suffix}@test.local");
        let marker = format!("stored-body-marker-{suffix}");
        let delivery_id = enqueue_send_mail(
            &db,
            &storage,
            &ValkeyQueue::new(redis.clone()),
            None,
            org.id,
            to.clone(),
            "stored".to_string(),
            marker.clone(),
        )
        .await
        .unwrap();

        let outcome = SendMailHandler { ctx: ctx.clone() }
            .run(
                arcodash_conveyor::TaskContext {
                    task_id: arcodash_conveyor::TaskId(0),
                    task_type: "send_mail".into(),
                    attempt: 1,
                    tenant_id: Some(org.id),
                    entity_id: Some(delivery_id),
                },
                SendMailPayload {
                    org_id: org.id,
                    to: to.clone(),
                    subject: "stored".to_string(),
                    body_html: None,
                },
            )
            .await
            .unwrap();
        assert!(matches!(outcome, TaskOutcome::Success { .. }));

        let row = mail_delivery::Entity::find_by_id(delivery_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "sent");

        let search: serde_json::Value = http
            .get(format!("{mailpit}/api/v1/search"))
            .query(&[("query", format!("to:{to}"))])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = search["messages"][0]["ID"]
            .as_str()
            .expect("the mail must reach Mailpit");
        let message: serde_json::Value = http
            .get(format!("{mailpit}/api/v1/message/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(message["Text"].as_str().unwrap().contains(&marker));
    }
}
