//! Producer-side helpers for the `send_digest`/`send_mail` jobs.
//!
//! Both share the `mail_deliveries` table (`entities::mail_delivery`): one payload type per task
//! type, a flag reader per task type, and a routing helper that picks the conveyor path or the
//! legacy default queue based on whether a `Conveyor` handle was passed in — same shape as
//! `notify.rs`'s `send_notification` helpers, kept in a separate module because these two task
//! types share a different delivery table.

use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde::{Deserialize, Serialize};

use arcodash_conveyor::{Conveyor, EnqueueRequest};

use crate::entities::mail_delivery;
use crate::org_storage::{OrgStorage, StorageArea, StorageError};
use crate::queue::{JobMessage, Queue, QueueName};

/// Terminalizes a just-inserted `mail_deliveries` row after its enqueue failed, so the sweeper's
/// stale-pending recovery never picks it back up and the caller's guard release (see
/// `digest_fanout.rs`) doesn't leave a `pending` row a later fan-out can collide with. Best-effort:
/// logs and returns without altering the original error — a failed cleanup must never mask why the
/// enqueue itself failed.
async fn mark_enqueue_failed(db: &impl ConnectionTrait, id: i64, error: &anyhow::Error) {
    let result = db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE mail_deliveries SET status = 'failed', finished_at = now(), error = $2 \
             WHERE id = $1 AND status = 'pending'",
            [
                id.into(),
                crate::scrub::scrub_error_values(&format!("{error:#}")).into(),
            ],
        ))
        .await;
    if let Err(e) = result {
        tracing::error!(delivery_id = id, error = %e, "mail_notify: failed to mark a mail_deliveries row failed after its enqueue failed");
    }
}

/// Object key (within the emails area) of delivery `delivery_id`'s body.
#[must_use]
pub fn mail_body_key(delivery_id: i64) -> String {
    format!("{delivery_id}.html")
}

/// The body of a `send_mail` row: its stored object, else the inline column of rows written before
/// bodies moved to storage.
///
/// # Errors
///
/// `NotFound` when the row has neither or the object is gone; otherwise the store's error.
pub async fn load_mail_body(
    storage: &OrgStorage,
    d: &mail_delivery::Model,
) -> Result<String, StorageError> {
    load_body(storage, d.body_location.as_deref(), d.body_html.as_deref()).await
}

/// [`load_mail_body`] over the two raw columns.
///
/// # Errors
///
/// As [`load_mail_body`].
pub async fn load_body(
    storage: &OrgStorage,
    location: Option<&str>,
    inline: Option<&str>,
) -> Result<String, StorageError> {
    if let Some(rel) = location {
        let bytes = storage.get(StorageArea::Emails, rel).await?;
        return String::from_utf8(bytes.to_vec()).map_err(|e| {
            StorageError::Other(anyhow::anyhow!("stored mail body is not UTF-8: {e}"))
        });
    }
    inline.map(str::to_owned).ok_or(StorageError::NotFound)
}

/// Enqueue the conveyor task for delivery row `id`; `Ok(false)` when conveyor reports it already
/// in flight rather than queued.
async fn enqueue_on_conveyor(
    c: &Conveyor,
    task_type: &str,
    payload: serde_json::Value,
    org_id: i64,
    id: i64,
) -> anyhow::Result<bool> {
    let res = c
        .enqueue(EnqueueRequest {
            task_type: task_type.into(),
            payload,
            tenant_id: Some(org_id),
            entity_id: Some(id),
            resource_key: None,
            resource_cap: None,
        })
        .await?;
    Ok(res.queued)
}

/// The handler payload for a `send_digest` conveyor task. Does NOT carry the delivery id — that
/// is the task's `entity_id`, read from `TaskContext.entity_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendDigestPayload {
    pub org_id: i64,
    pub user_id: i64,
}

/// The handler payload for a `send_mail` conveyor task. Does NOT carry the delivery id — same
/// convention as `SendDigestPayload`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMailPayload {
    pub org_id: i64,
    pub to: String,
    pub subject: String,
    /// Absent on tasks enqueued by `enqueue_send_mail`: the handler loads the stored body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_html: Option<String>,
}

/// Route `send_digest` enqueues through the conveyor SDK instead of the legacy default queue.
/// Off/unset keeps the legacy queue.
#[must_use]
pub fn conveyor_send_digest_enabled() -> bool {
    crate::config::conveyor_flag("CONVEYOR_SEND_DIGEST_ENABLED")
}

/// Route `send_mail` enqueues through the conveyor SDK instead of the legacy default queue.
/// Off/unset keeps the legacy queue.
#[must_use]
pub fn conveyor_send_mail_enabled() -> bool {
    crate::config::conveyor_flag("CONVEYOR_SEND_MAIL_ENABLED")
}

/// Insert the durable `mail_deliveries` row, then enqueue.
///
/// Routes through conveyor iff `conveyor` is `Some` AND [`conveyor_send_digest_enabled`] —
/// self-gated on its own flag so a `conveyor` handle present only because a *different* task type's
/// flag is on doesn't accidentally reroute this one. The legacy branch reproduces today's
/// `JobMessage::SendDigest` verbatim, so the flag-off wire output is unchanged.
pub async fn enqueue_send_digest(
    db: &impl sea_orm::ConnectionTrait,
    queue: &dyn Queue,
    conveyor: Option<&Conveyor>,
    org_id: i64,
    user_id: i64,
) -> anyhow::Result<i64> {
    let id = mail_delivery::insert_pending_digest(db, org_id, user_id).await?;
    let result = if let Some(c) = conveyor.filter(|_| conveyor_send_digest_enabled()) {
        let payload = serde_json::to_value(SendDigestPayload { org_id, user_id })?;
        enqueue_on_conveyor(c, "send_digest", payload, org_id, id)
            .await
            .map(|queued| {
                if !queued {
                    tracing::warn!(delivery_id = id, org_id, user_id, "enqueue_send_digest: conveyor reported the task already in flight, not actually queued");
                }
            })
    } else {
        let job = JobMessage::SendDigest {
            org_id,
            user_id,
            delivery_id: Some(id),
        };
        queue.enqueue(QueueName::Default, job).await
    };
    if let Err(e) = result {
        mark_enqueue_failed(db, id, &e).await;
        return Err(e);
    }
    Ok(id)
}

/// Insert the durable `mail_deliveries` row, store the body as an object, then enqueue.
///
/// Same routing shape as [`enqueue_send_digest`], gated on [`conveyor_send_mail_enabled`] instead.
/// The body never lands in Postgres: when the object write fails the row is failed and the error
/// returned.
///
/// # Errors
///
/// A database error, a storage error from writing the body, or the enqueue error.
#[allow(clippy::too_many_arguments)]
pub async fn enqueue_send_mail(
    db: &impl sea_orm::ConnectionTrait,
    storage: &OrgStorage,
    queue: &dyn Queue,
    conveyor: Option<&Conveyor>,
    org_id: i64,
    to: String,
    subject: String,
    body_html: String,
) -> anyhow::Result<i64> {
    let id = mail_delivery::insert_pending_mail(db, org_id, &to, &subject).await?;
    let rel = mail_body_key(id);
    let stored = storage
        .put(
            StorageArea::Emails,
            &rel,
            body_html.clone().into_bytes().into(),
            "text/html; charset=utf-8",
        )
        .await;
    if let Err(e) = stored {
        let e = anyhow::Error::new(e);
        mark_enqueue_failed(db, id, &e).await;
        return Err(e);
    }
    if let Err(e) = mail_delivery::set_body_location(db, id, &rel).await {
        let e = anyhow::Error::new(e);
        mark_enqueue_failed(db, id, &e).await;
        return Err(e);
    }
    let result = if let Some(c) = conveyor.filter(|_| conveyor_send_mail_enabled()) {
        let payload = serde_json::to_value(SendMailPayload {
            org_id,
            to,
            subject,
            body_html: None,
        })?;
        enqueue_on_conveyor(c, "send_mail", payload, org_id, id)
            .await
            .map(|queued| {
                if !queued {
                    tracing::warn!(delivery_id = id, org_id, "enqueue_send_mail: conveyor reported the task already in flight, not actually queued");
                }
            })
    } else {
        let job = JobMessage::SendMail {
            org_id,
            to,
            subject,
            body_html,
            delivery_id: Some(id),
        };
        queue.enqueue(QueueName::Default, job).await
    };
    if let Err(e) = result {
        mark_enqueue_failed(db, id, &e).await;
        return Err(e);
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::RecordingQueue;
    use async_trait::async_trait;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    /// Needs Postgres (`None` skips): a connection plus a fresh org slugged `{label}-<uuid>`.
    async fn db_with_org(label: &str) -> Option<(sea_orm::DatabaseConnection, i64)> {
        use sea_orm::ActiveModelTrait;
        use sea_orm::ActiveValue::Set;
        let db = crate::test_support::test_db().await?;
        let slug = format!("{label}-{}", uuid::Uuid::new_v4());
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
        let org_id = crate::entities::organization::ActiveModel {
            name: Set(slug.to_string()),
            slug: Set(slug.to_string()),
            settings: Set(serde_json::json!({})),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap()
        .id;
        Some((db, org_id))
    }

    fn mem_storage(
        org_id: i64,
    ) -> (
        OrgStorage,
        std::sync::Arc<crate::test_support::blob::MemoryBlobStore>,
    ) {
        let mem = std::sync::Arc::new(crate::test_support::blob::MemoryBlobStore::default());
        let storage = OrgStorage::new(
            org_id,
            crate::org_storage::StorageMode::ArcoDash,
            mem.clone(),
            format!("orgs/{org_id}/"),
        );
        (storage, mem)
    }

    async fn delivery(db: &sea_orm::DatabaseConnection, id: i64) -> mail_delivery::Model {
        mail_delivery::Entity::find_by_id(id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn mail_body_is_stored_outside_postgres() {
        let Some((db, org_id)) = db_with_org("org-mail-body-stored").await else {
            return;
        };
        let (storage, mem) = mem_storage(org_id);
        let id = enqueue_send_mail(
            &db,
            &storage,
            &RecordingQueue::default(),
            None,
            org_id,
            "to@test.local".to_string(),
            "subject".to_string(),
            "<p>secret body</p>".to_string(),
        )
        .await
        .unwrap();

        let row = delivery(&db, id).await;
        assert_eq!(row.body_html, None);
        assert_eq!(
            row.body_location.as_deref(),
            Some(format!("{id}.html").as_str())
        );
        assert_eq!(
            mem.content_type(&storage.key(StorageArea::Emails, &format!("{id}.html")))
                .as_deref(),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(
            load_mail_body(&storage, &row).await.unwrap(),
            "<p>secret body</p>"
        );
    }

    #[tokio::test]
    async fn mail_sender_reads_legacy_inline_body() {
        let Some((db, org_id)) = db_with_org("org-mail-body-legacy").await else {
            return;
        };
        let (storage, _) = mem_storage(org_id);
        let id = mail_delivery::insert_pending_mail(&db, org_id, "to@test.local", "s")
            .await
            .unwrap();
        let mut row = delivery(&db, id).await;
        assert!(matches!(
            load_mail_body(&storage, &row).await,
            Err(StorageError::NotFound)
        ));
        row.body_html = Some("<p>legacy</p>".to_string());
        assert_eq!(
            load_mail_body(&storage, &row).await.unwrap(),
            "<p>legacy</p>"
        );
    }

    #[tokio::test]
    async fn mail_put_failure_marks_delivery_failed() {
        let Some((db, org_id)) = db_with_org("org-mail-put-fails").await else {
            return;
        };
        let storage = OrgStorage::new(
            org_id,
            crate::org_storage::StorageMode::ArcoDash,
            std::sync::Arc::new(crate::test_support::blob::FailingBlobStore::default()),
            "orgs/x/",
        );
        let queue = RecordingQueue::default();
        let err = enqueue_send_mail(
            &db,
            &storage,
            &queue,
            None,
            org_id,
            "to@test.local".to_string(),
            "subject".to_string(),
            "<p>secret body</p>".to_string(),
        )
        .await
        .unwrap_err();
        assert!(err.downcast_ref::<StorageError>().is_some());
        assert!(queue.sent.lock().unwrap().is_empty());

        let row = mail_delivery::Entity::find()
            .filter(mail_delivery::Column::OrgId.eq(org_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "failed");
        assert_eq!(row.body_html, None);
        assert_eq!(row.body_location, None);
        assert!(!row.error.unwrap_or_default().contains("secret body"));
    }

    #[tokio::test]
    async fn mail_enqueue_failure_error_is_scrubbed() {
        let Some((db, org_id)) = db_with_org("org-mail-enqueue-scrub").await else {
            return;
        };
        let storage = OrgStorage::new(
            org_id,
            crate::org_storage::StorageMode::ArcoDash,
            std::sync::Arc::new(crate::test_support::blob::FailingBlobStore::new(
                "Key (email)=(bob@x.com) already exists.",
            )),
            "orgs/x/",
        );
        enqueue_send_mail(
            &db,
            &storage,
            &RecordingQueue::default(),
            None,
            org_id,
            "to@test.local".to_string(),
            "subject".to_string(),
            "<p>body</p>".to_string(),
        )
        .await
        .unwrap_err();
        let row = mail_delivery::Entity::find()
            .filter(mail_delivery::Column::OrgId.eq(org_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.error.as_deref(),
            Some("storage unreachable: Key (email)=(…) already exists.")
        );
    }

    #[tokio::test]
    async fn legacy_branch_reproduces_exact_send_digest_shape() {
        let Some((db, org_id)) = db_with_org("org-mail-notify-digest").await else {
            return;
        };

        let queue = RecordingQueue::default();
        let id = enqueue_send_digest(&db, &queue, None, org_id, 42)
            .await
            .unwrap();

        let sent = queue.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let (queue_name, job) = &sent[0];
        assert_eq!(*queue_name, QueueName::Default);
        match job {
            JobMessage::SendDigest {
                org_id: got_org,
                user_id,
                delivery_id,
            } => {
                assert_eq!(*got_org, org_id);
                assert_eq!(*user_id, 42);
                assert_eq!(*delivery_id, Some(id));
            }
            other => panic!("expected SendDigest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn legacy_branch_reproduces_exact_send_mail_shape() {
        let Some((db, org_id)) = db_with_org("org-mail-notify-mail").await else {
            return;
        };

        let queue = RecordingQueue::default();
        let (storage, _) = mem_storage(org_id);
        let id = enqueue_send_mail(
            &db,
            &storage,
            &queue,
            None,
            org_id,
            "to@test.local".to_string(),
            "subject".to_string(),
            "body".to_string(),
        )
        .await
        .unwrap();

        let sent = queue.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let (queue_name, job) = &sent[0];
        assert_eq!(*queue_name, QueueName::Default);
        match job {
            JobMessage::SendMail {
                org_id: got_org,
                to,
                subject,
                body_html,
                delivery_id,
            } => {
                assert_eq!(*got_org, org_id);
                assert_eq!(to, "to@test.local");
                assert_eq!(subject, "subject");
                assert_eq!(body_html, "body");
                assert_eq!(*delivery_id, Some(id));
            }
            other => panic!("expected SendMail, got {other:?}"),
        }
    }

    #[test]
    fn send_digest_payload_round_trips_through_json() {
        let payload = SendDigestPayload {
            org_id: 1,
            user_id: 2,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: SendDigestPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.org_id, payload.org_id);
        assert_eq!(parsed.user_id, payload.user_id);
    }

    #[test]
    fn send_mail_payload_round_trips_through_json() {
        let payload = SendMailPayload {
            org_id: 1,
            to: "to@test.local".to_string(),
            subject: "subject".to_string(),
            body_html: Some("body".to_string()),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: SendMailPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.org_id, payload.org_id);
        assert_eq!(parsed.to, payload.to);
        assert_eq!(parsed.subject, payload.subject);
        assert_eq!(parsed.body_html, payload.body_html);
    }

    // `conveyor_send_digest_enabled`/`conveyor_send_mail_enabled` are thin wrappers over
    // `config::parse_bool_env`, whose own test table covers every parse shape (case, whitespace,
    // garbage) — no need to duplicate that here per call site.

    /// Always fails, to drive the enqueue-error branch of
    /// `enqueue_send_digest`/`enqueue_send_mail`.
    struct FailingQueue;

    #[async_trait]
    impl Queue for FailingQueue {
        async fn enqueue(&self, _queue: QueueName, _job: JobMessage) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("queue unreachable"))
        }
    }

    /// Status of the org's only `mail_deliveries` row (the enqueue fns don't return the id on
    /// error; the fixture's org is fresh, so this row is the only one).
    async fn org_delivery_status(db: &sea_orm::DatabaseConnection, org_id: i64) -> String {
        mail_delivery::Entity::find()
            .filter(mail_delivery::Column::OrgId.eq(org_id))
            .one(db)
            .await
            .unwrap()
            .unwrap()
            .status
    }

    #[tokio::test]
    async fn a_failed_digest_enqueue_marks_its_row_failed_not_left_pending() {
        let Some((db, org_id)) = db_with_org("org-mail-notify-digest-fail").await else {
            return;
        };

        let err = enqueue_send_digest(&db, &FailingQueue, None, org_id, 42)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("queue unreachable"));

        assert_eq!(org_delivery_status(&db, org_id).await, "failed");
    }

    #[tokio::test]
    async fn a_failed_mail_enqueue_marks_its_row_failed_not_left_pending() {
        let Some((db, org_id)) = db_with_org("org-mail-notify-mail-fail").await else {
            return;
        };

        let (storage, _) = mem_storage(org_id);
        enqueue_send_mail(
            &db,
            &storage,
            &FailingQueue,
            None,
            org_id,
            "to@test.local".to_string(),
            "subject".to_string(),
            "body".to_string(),
        )
        .await
        .unwrap_err();

        assert_eq!(org_delivery_status(&db, org_id).await, "failed");
    }
}
