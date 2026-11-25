//! Queue abstraction and the Valkey job-status format, shared by the backend (enqueue, status
//! reads) and the worker (consume, status writes).
//!
//! Keys (frontend contract, keep stable): queue lists `arcodash:queue:<name>`; job status
//! `job:<job_id>` (JSON [`JobStatusRecord`] with a [`JOB_STATUS_TTL`] TTL; `org_id` rejects
//! cross-org access since Valkey has no RLS); cancel flag `job:<job_id>:cancel` = `"1"`. Status
//! codes: `PENDING=1 STARTED=2 SUCCESS=3 FAILURE=4 CANCELLED=5` ([`JobStatus`]).

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Valkey key prefix for queue lists: `arcodash:queue:<name>`.
const QUEUE_KEY_PREFIX: &str = "arcodash:queue";

/// TTL applied to every `job:<id>` / `job:<id>:cancel` key; bounds Valkey memory for abandoned
/// jobs.
pub const JOB_STATUS_TTL_SECS: u64 = 60 * 60;

/// Job status integer codes — do not renumber, the frontend depends on them. Encoded by hand as a
/// bare `u8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum JobStatus {
    Pending = 1,
    Started = 2,
    Success = 3,
    Failure = 4,
    Cancelled = 5,
}

impl Serialize for JobStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for JobStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u8::deserialize(deserializer)?;
        Self::from_u8(value)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid JobStatus code: {value}")))
    }
}

impl JobStatus {
    #[must_use]
    pub fn from_u8(value: u8) -> Option<Self> {
        [
            Self::Pending,
            Self::Started,
            Self::Success,
            Self::Failure,
            Self::Cancelled,
        ]
        .into_iter()
        .find(|s| *s as u8 == value)
    }

    /// `true` for statuses that will never progress further (success/failure/cancelled).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Success | Self::Failure | Self::Cancelled)
    }
}

/// The logical queues; each has its own list so interactive execution is never starved by bulk
/// scheduled work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueName {
    Queries,
    ScheduledQueries,
    Schemas,
    Default,
}

impl QueueName {
    pub const ALL: [Self; 4] = [
        Self::Queries,
        Self::ScheduledQueries,
        Self::Schemas,
        Self::Default,
    ];

    /// The queue's name as used in the Valkey key, e.g. `"queries"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queries => "queries",
            Self::ScheduledQueries => "scheduled_queries",
            Self::Schemas => "schemas",
            Self::Default => "default",
        }
    }

    #[must_use]
    pub fn queue_key(self) -> String {
        format!("{QUEUE_KEY_PREFIX}:{}", self.as_str())
    }

    /// Parse a queue name as it appears in `WORKER_QUEUES` (same spelling as [`Self::as_str`]).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.as_str() == name)
    }
}

/// Job payloads enqueued onto a [`QueueName`], one variant per worker handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum JobMessage {
    ExecuteQuery {
        job_id: String,
        org_id: i64,
        data_source_id: i64,
        query_text: String,
        parameters: serde_json::Value,
        user_id: i64,
        /// `Some(id)` when tied to a saved query (the worker updates
        /// `queries.latest_query_data_id`); `None` for ad-hoc executions.
        query_id: Option<i64>,
        scheduled: bool,
        /// `Some(id)` when driven by a `runs` row (the worker CASes it pending->running and
        /// finalizes it); `None` for interactive executes. `#[serde(default)]` keeps in-flight
        /// messages parseable.
        #[serde(default)]
        run_id: Option<i64>,
        /// `true` to repoint `queries.published_latest_query_data_id` instead of
        /// `latest_query_data_id`. `#[serde(default)]` for in-flight messages.
        #[serde(default)]
        repoint_published: bool,
        /// Per-execution row cap, clamped to the worker's `max_query_rows`; `None` = the worker's
        /// cap.
        #[serde(default)]
        max_rows: Option<u32>,
    },
    EvaluateMonitor {
        org_id: i64,
        monitor_id: i64,
        #[serde(default)]
        run_id: Option<i64>,
    },
    SendNotification {
        org_id: i64,
        monitor_id: i64,
        destination_id: i64,
        new_state: String,
        subscriber_id: i64,
        /// The `monitor_subscriptions` row this send is for; `None` for sends with no subscription
        /// (e.g. escalation steps).
        #[serde(default)]
        subscription_id: Option<i64>,
        /// 1-based delivery attempt for the bounded-retry loop; defaults to 1.
        #[serde(default = "default_attempt")]
        attempt: u32,
        /// The pre-created `notification_deliveries` row's id (the producer inserts a `pending` row
        /// first so the handler can CAS-claim and finalize it). `None` means an old in-flight
        /// message or an unresolvable id; the handler then inserts per attempt. Different from
        /// `notification_deliveries.run_id` (the monitor evaluation run); the wire field stays
        /// `run_id` for rolling-deploy compatibility.
        #[serde(default)]
        run_id: Option<i64>,
        /// The value that decided this transition, captured at evaluation time; `None` re-resolves
        /// the monitor's current result at send time.
        #[serde(default)]
        observed: Option<String>,
        #[serde(default)]
        explanation: Option<String>,
    },
    /// Send one user's daily digest email. Enqueued per digest-enabled user by the worker's digest
    /// cron, and by the e2e seam.
    SendDigest {
        org_id: i64,
        user_id: i64,
        /// The pre-created `mail_deliveries` row's id, as for `SendNotification.run_id`. `None`
        /// skips claim/finalize (e2e seam or old in-flight message).
        #[serde(default)]
        delivery_id: Option<i64>,
    },
    SendMail {
        org_id: i64,
        to: String,
        subject: String,
        body_html: String,
        #[serde(default)]
        delivery_id: Option<i64>,
    },
    /// Resumes a monitor run parked on an in-flight query run once that run is terminal.
    /// `query_run_status` carries the terminal status string so the handler can evaluate fresh data
    /// (success) or fail the monitor run.
    ResumeMonitorEvaluation {
        org_id: i64,
        run_id: i64,
        /// The query run this monitor run parked behind, to name it in the failure message.
        query_run_id: i64,
        query_run_status: String,
    },
    /// Sample the data source's tables and rebuild `data_sources.ai_profile`.
    ProfileDataSource {
        org_id: i64,
        data_source_id: i64,
        #[serde(default)]
        data: crate::models::ProfileData,
    },
    /// Fill missing column descriptions in `data_sources.ai_profile` with one LLM call.
    DescribeDataSource { org_id: i64, data_source_id: i64 },
}

/// Queue a profile job for the data source. `Ok(false)` when one is already pending (the pending
/// key dedupes and rate-limits).
pub async fn enqueue_profile(
    redis: &mut ConnectionManager,
    queue: &dyn Queue,
    org_id: i64,
    data_source_id: i64,
    data: crate::models::ProfileData,
) -> Result<bool> {
    let key = crate::models::ai_profile_pending_key(data_source_id);
    let set: Option<String> = redis::cmd("SET")
        .arg(&key)
        .arg("schema")
        .arg("NX")
        .arg("EX")
        .arg(crate::models::AI_PROFILE_PENDING_TTL_SECS)
        .query_async(redis)
        .await?;
    if set.is_none() {
        return Ok(false);
    }
    let job = JobMessage::ProfileDataSource {
        org_id,
        data_source_id,
        data,
    };
    if let Err(e) = queue.enqueue(QueueName::Schemas, job).await {
        let _: Result<(), _> = redis.del(&key).await;
        return Err(e);
    }
    Ok(true)
}

pub(crate) const fn default_attempt() -> u32 {
    1
}

/// Producer-side queue abstraction; the consumer side lives in the worker.
#[async_trait]
pub trait Queue: Send + Sync {
    async fn enqueue(&self, queue: QueueName, job: JobMessage) -> Result<()>;
}

/// Valkey-backed [`Queue`]: `RPUSH arcodash:queue:<name>` with the JSON [`JobMessage`]; the worker
/// `BLPOP`s the same key.
#[derive(Clone)]
pub struct ValkeyQueue {
    valkey: ConnectionManager,
}

impl ValkeyQueue {
    #[must_use]
    pub const fn new(valkey: ConnectionManager) -> Self {
        Self { valkey }
    }
}

#[async_trait]
impl Queue for ValkeyQueue {
    async fn enqueue(&self, queue: QueueName, job: JobMessage) -> Result<()> {
        let payload = serde_json::to_string(&job)?;
        let mut conn = self.valkey.clone();
        conn.rpush::<_, _, ()>(queue.queue_key(), payload).await?;
        Ok(())
    }
}

/// Build the job producer (Valkey lists). `config` is unused.
pub fn build_queue(_config: &Config, valkey: ConnectionManager) -> Result<Arc<dyn Queue>> {
    Ok(Arc::new(ValkeyQueue::new(valkey)))
}

fn job_status_key(job_id: &str) -> String {
    format!("job:{job_id}")
}

fn job_cancel_key(job_id: &str) -> String {
    format!("job:{job_id}:cancel")
}

/// The JSON shape stored at `job:<id>`: the job's status record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatusRecord {
    /// The org that submitted this job; lets `job_service::{get_status,cancel}` reject cross-org
    /// access (Valkey has no RLS). `default` (0) matches no org, so old records 404 (fail closed).
    #[serde(default)]
    pub org_id: i64,
    /// The submitting user; `None` for older records or writers without request context
    /// (`set_job_status` preserves it). Lets `job_service` reject same-org callers who are neither
    /// submitter nor admin.
    #[serde(default)]
    pub user_id: Option<i64>,
    pub status: JobStatus,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub query_result_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// Write `job:<job_id>`'s status record, refreshing its TTL.
///
/// Each call is a full snapshot, so pass `org_id` every time. `user_id: None` means "unknown", not
/// "clear": the previous record's `user_id` is carried forward.
#[allow(clippy::too_many_arguments)]
pub async fn set_job_status(
    valkey: &ConnectionManager,
    job_id: &str,
    org_id: i64,
    user_id: Option<i64>,
    status: JobStatus,
    query_result_id: Option<i64>,
    error: Option<String>,
) -> Result<()> {
    let user_id = match user_id {
        None => get_job_status(valkey, job_id)
            .await?
            .and_then(|r| r.user_id),
        known => known,
    };
    let record = JobStatusRecord {
        org_id,
        user_id,
        status,
        query_result_id,
        error: error.map(|e| crate::scrub::scrub_error_values(&e)),
        updated_at: Utc::now(),
    };
    let payload = serde_json::to_string(&record)?;

    let mut conn = valkey.clone();
    conn.set_ex::<_, _, ()>(job_status_key(job_id), payload, JOB_STATUS_TTL_SECS)
        .await?;
    Ok(())
}

/// Read `job:<job_id>`'s status record, if present (it may have expired or never existed).
pub async fn get_job_status(
    valkey: &ConnectionManager,
    job_id: &str,
) -> Result<Option<JobStatusRecord>> {
    let mut conn = valkey.clone();
    let raw: Option<String> = conn.get(job_status_key(job_id)).await?;
    Ok(raw.map(|raw| serde_json::from_str(&raw)).transpose()?)
}

/// Flip the cancellation flag for `job_id`. Best-effort: the worker only checks before starting (no
/// mid-query abort).
pub async fn request_cancellation(valkey: &ConnectionManager, job_id: &str) -> Result<()> {
    let mut conn = valkey.clone();
    conn.set_ex::<_, _, ()>(job_cancel_key(job_id), "1", JOB_STATUS_TTL_SECS)
        .await?;
    Ok(())
}

/// Atomically flip `job:<job_id>` to Cancelled iff non-terminal, in one Lua script so a worker's
/// terminal write can't be clobbered back to Cancelled. Keeps every field except
/// `status`/`updated_at` (drops `query_result_id`/`error`). `Ok(true)` iff this call flipped it.
const SET_CANCELLED_IF_NOT_TERMINAL_SCRIPT: &str = r"
local raw = redis.call('GET', KEYS[1])
if not raw then
    return 0
end
local rec = cjson.decode(raw)
if rec.status == 3 or rec.status == 4 or rec.status == 5 then
    return 0
end
rec.status = 5
rec.query_result_id = nil
rec.error = nil
rec.updated_at = ARGV[1]
redis.call('SET', KEYS[1], cjson.encode(rec), 'EX', ARGV[2])
return 1
";

pub async fn set_cancelled_if_not_terminal(
    valkey: &ConnectionManager,
    job_id: &str,
) -> Result<bool> {
    let mut conn = valkey.clone();
    let flipped: i64 = redis::Script::new(SET_CANCELLED_IF_NOT_TERMINAL_SCRIPT)
        .key(job_status_key(job_id))
        .arg(Utc::now().to_rfc3339())
        .arg(JOB_STATUS_TTL_SECS)
        .invoke_async(&mut conn)
        .await?;
    Ok(flipped == 1)
}

/// Whether cancellation has been requested for `job_id`.
pub async fn is_cancelled(valkey: &ConnectionManager, job_id: &str) -> Result<bool> {
    let mut conn = valkey.clone();
    Ok(conn.exists(job_cancel_key(job_id)).await?)
}

/// Renews `job:<job_id>`'s TTL without touching its contents, so a long-running query's record (and
/// cancellation) doesn't expire mid-flight. A no-op if the key is already gone.
#[allow(
    clippy::cast_possible_wrap,
    reason = "JOB_STATUS_TTL_SECS is a small constant that fits i64"
)]
pub async fn refresh_job_status_ttl(valkey: &ConnectionManager, job_id: &str) -> Result<()> {
    let mut conn = valkey.clone();
    conn.expire::<_, ()>(job_status_key(job_id), JOB_STATUS_TTL_SECS as i64)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    static SCHEMAS_QUEUE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Live round-trip against `TEST_REDIS_URL` (skips when unset) covering the enqueue, `LPOP`,
    /// status and cancel-flag command families.
    #[tokio::test]
    async fn live_server_round_trips_enqueue_and_job_status() {
        let Ok(url) = std::env::var("TEST_REDIS_URL") else {
            return;
        };
        let _guard = SCHEMAS_QUEUE_LOCK.lock().await;
        let conn = crate::valkey_client::connect(&url)
            .await
            .expect("connect to test server");

        let queue = ValkeyQueue::new(conn.clone());
        let key = QueueName::Schemas.queue_key();
        let mut raw = conn.clone();
        let _: () = raw.del(&key).await.unwrap();
        queue
            .enqueue(
                QueueName::Schemas,
                JobMessage::EvaluateMonitor {
                    org_id: 7,
                    monitor_id: 1,
                    run_id: None,
                },
            )
            .await
            .expect("enqueue");
        let popped: Option<String> = raw.lpop(&key, None).await.unwrap();
        let msg: JobMessage =
            serde_json::from_str(&popped.expect("a job on the queue")).expect("parse job");
        assert!(matches!(
            msg,
            JobMessage::EvaluateMonitor {
                org_id: 7,
                monitor_id: 1,
                run_id: None
            }
        ));

        let job_id = "test-valkey-round-trip";
        set_job_status(&conn, job_id, 7, None, JobStatus::Success, Some(99), None)
            .await
            .expect("set status");
        let got = get_job_status(&conn, job_id).await.expect("get status");
        let got = got.expect("status present");
        assert_eq!(got.org_id, 7);
        assert_eq!(got.status, JobStatus::Success);
        assert_eq!(got.query_result_id, Some(99));

        assert!(!is_cancelled(&conn, job_id).await.unwrap());
        request_cancellation(&conn, job_id).await.expect("cancel");
        assert!(is_cancelled(&conn, job_id).await.unwrap());

        let _: () = raw.del(job_status_key(job_id)).await.unwrap();
        let _: () = raw.del(job_cancel_key(job_id)).await.unwrap();
    }

    #[tokio::test]
    async fn enqueue_is_fifo() {
        let Ok(url) = std::env::var("TEST_REDIS_URL") else {
            return;
        };
        let _guard = SCHEMAS_QUEUE_LOCK.lock().await;
        let conn = crate::valkey_client::connect(&url)
            .await
            .expect("connect to test server");

        let queue = ValkeyQueue::new(conn.clone());
        let key = QueueName::Schemas.queue_key();
        let mut raw = conn.clone();
        let _: () = raw.del(&key).await.unwrap();

        for monitor_id in [1, 2, 3] {
            queue
                .enqueue(
                    QueueName::Schemas,
                    JobMessage::EvaluateMonitor {
                        org_id: 7,
                        monitor_id,
                        run_id: None,
                    },
                )
                .await
                .expect("enqueue");
        }

        let mut ours = Vec::new();
        while ours.len() < 3 {
            let popped: Option<String> = raw.lpop(&key, None).await.unwrap();
            let msg: JobMessage =
                serde_json::from_str(&popped.expect("a job on the queue")).expect("parse job");
            if let JobMessage::EvaluateMonitor {
                org_id: 7,
                monitor_id,
                ..
            } = msg
            {
                ours.push(monitor_id);
            }
        }
        assert_eq!(ours, vec![1, 2, 3]);

        let _: () = raw.del(&key).await.unwrap();
    }

    /// The atomic cancel flips a non-terminal record but must not clobber one that already went
    /// terminal.
    #[tokio::test]
    async fn set_cancelled_if_not_terminal_flips_pending_but_not_already_terminal() {
        let Ok(url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let conn = crate::valkey_client::connect(&url).await.unwrap();

        let pending_job = format!("test-cancel-pending-{}", uuid::Uuid::new_v4());
        set_job_status(&conn, &pending_job, 7, None, JobStatus::Started, None, None)
            .await
            .unwrap();
        assert!(
            set_cancelled_if_not_terminal(&conn, &pending_job)
                .await
                .unwrap(),
            "a non-terminal record must flip"
        );
        let flipped = get_job_status(&conn, &pending_job).await.unwrap().unwrap();
        assert_eq!(flipped.status, JobStatus::Cancelled);
        assert_eq!(flipped.org_id, 7, "org_id must be preserved");

        let success_job = format!("test-cancel-success-{}", uuid::Uuid::new_v4());
        set_job_status(
            &conn,
            &success_job,
            9,
            None,
            JobStatus::Success,
            Some(42),
            None,
        )
        .await
        .unwrap();
        assert!(
            !set_cancelled_if_not_terminal(&conn, &success_job)
                .await
                .unwrap(),
            "an already-terminal record must not flip"
        );
        let untouched = get_job_status(&conn, &success_job).await.unwrap().unwrap();
        assert_eq!(
            untouched.status,
            JobStatus::Success,
            "a genuine worker success must survive a racing cancel request, not get clobbered \
             back to Cancelled"
        );
        assert_eq!(untouched.query_result_id, Some(42));

        let missing_job = format!("test-cancel-missing-{}", uuid::Uuid::new_v4());
        assert!(
            !set_cancelled_if_not_terminal(&conn, &missing_job)
                .await
                .unwrap(),
            "a nonexistent record must not flip (and must not error)"
        );

        let mut raw = conn.clone();
        let _: () = raw.del(job_status_key(&pending_job)).await.unwrap();
        let _: () = raw.del(job_status_key(&success_job)).await.unwrap();
    }

    /// A writer with `user_id: None` must not erase a previously recorded submitter.
    #[tokio::test]
    async fn set_job_status_preserves_user_id_when_a_later_writer_passes_none() {
        let Ok(url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let conn = crate::valkey_client::connect(&url).await.unwrap();
        let job_id = format!("test-user-id-preserve-{}", uuid::Uuid::new_v4());

        set_job_status(&conn, &job_id, 7, Some(42), JobStatus::Pending, None, None)
            .await
            .unwrap();
        set_job_status(&conn, &job_id, 7, None, JobStatus::Success, Some(1), None)
            .await
            .unwrap();

        let got = get_job_status(&conn, &job_id).await.unwrap().unwrap();
        assert_eq!(
            got.user_id,
            Some(42),
            "user_id must survive a later None write"
        );
        assert_eq!(got.status, JobStatus::Success);

        let mut raw = conn.clone();
        let _: () = raw.del(job_status_key(&job_id)).await.unwrap();
    }

    #[tokio::test]
    async fn refresh_job_status_ttl_restores_the_full_ttl_and_is_a_no_op_for_a_missing_key() {
        let Ok(url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let conn = crate::valkey_client::connect(&url).await.unwrap();
        let mut raw = conn.clone();

        let missing_job = format!("test-ttl-refresh-missing-{}", uuid::Uuid::new_v4());
        refresh_job_status_ttl(&conn, &missing_job)
            .await
            .expect("refreshing a nonexistent key must not error");

        let job_id = format!("test-ttl-refresh-{}", uuid::Uuid::new_v4());
        set_job_status(&conn, &job_id, 7, None, JobStatus::Started, None, None)
            .await
            .unwrap();
        let key = job_status_key(&job_id);
        let _: () = raw.expire(&key, 5).await.unwrap();
        let lowered: i64 = raw.ttl(&key).await.unwrap();
        assert!(lowered <= 5, "the manual EXPIRE must have taken effect");

        refresh_job_status_ttl(&conn, &job_id).await.unwrap();
        let restored: i64 = raw.ttl(&key).await.unwrap();
        assert!(
            restored > 5,
            "TTL must be back up near JOB_STATUS_TTL_SECS after refresh, got {restored}"
        );

        let _: () = raw.del(&key).await.unwrap();
    }

    #[test]
    fn queue_name_queue_keys_match_doc_naming() {
        assert_eq!(QueueName::Queries.queue_key(), "arcodash:queue:queries");
        assert_eq!(
            QueueName::ScheduledQueries.queue_key(),
            "arcodash:queue:scheduled_queries"
        );
        assert_eq!(QueueName::Schemas.queue_key(), "arcodash:queue:schemas");
        assert_eq!(QueueName::Default.queue_key(), "arcodash:queue:default");
    }

    #[test]
    fn queue_name_parse_round_trips_with_as_str() {
        for queue in QueueName::ALL {
            assert_eq!(QueueName::parse(queue.as_str()), Some(queue));
        }
        assert_eq!(QueueName::parse("not-a-queue"), None);
    }

    #[test]
    fn job_status_codes_are_stable() {
        assert_eq!(JobStatus::Pending as u8, 1);
        assert_eq!(JobStatus::Started as u8, 2);
        assert_eq!(JobStatus::Success as u8, 3);
        assert_eq!(JobStatus::Failure as u8, 4);
        assert_eq!(JobStatus::Cancelled as u8, 5);
    }

    #[test]
    fn job_status_serializes_as_bare_integer() {
        assert_eq!(serde_json::to_string(&JobStatus::Pending).unwrap(), "1");
        assert_eq!(serde_json::to_string(&JobStatus::Cancelled).unwrap(), "5");
    }

    #[test]
    fn job_status_round_trips_through_json() {
        for status in [
            JobStatus::Pending,
            JobStatus::Started,
            JobStatus::Success,
            JobStatus::Failure,
            JobStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: JobStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn job_status_rejects_invalid_code() {
        let result: Result<JobStatus, _> = serde_json::from_str("9");
        assert!(result.is_err());
    }

    #[test]
    fn is_terminal_only_true_for_success_failure_cancelled() {
        assert!(!JobStatus::Pending.is_terminal());
        assert!(!JobStatus::Started.is_terminal());
        assert!(JobStatus::Success.is_terminal());
        assert!(JobStatus::Failure.is_terminal());
        assert!(JobStatus::Cancelled.is_terminal());
    }

    #[test]
    fn job_message_execute_query_round_trips_through_json_with_type_tag() {
        let msg = JobMessage::ExecuteQuery {
            job_id: "abc-123".to_string(),
            org_id: 1,
            data_source_id: 2,
            query_text: "SELECT 1".to_string(),
            parameters: serde_json::json!({}),
            user_id: 3,
            query_id: Some(42),
            scheduled: false,
            run_id: Some(7),
            repoint_published: true,
            max_rows: Some(20),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"ExecuteQuery\""));
        let parsed: JobMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            JobMessage::ExecuteQuery {
                job_id,
                query_id,
                scheduled,
                run_id,
                repoint_published,
                ..
            } => {
                assert_eq!(job_id, "abc-123");
                assert_eq!(query_id, Some(42));
                assert!(!scheduled);
                assert_eq!(run_id, Some(7));
                assert!(repoint_published);
            }
            other => panic!("expected ExecuteQuery, got {other:?}"),
        }
    }

    #[test]
    fn job_message_profile_data_source_round_trips_through_json() {
        let msg = JobMessage::ProfileDataSource {
            org_id: 1,
            data_source_id: 2,
            data: crate::models::ProfileData::All,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"ProfileDataSource\""));
        match serde_json::from_str::<JobMessage>(&json).unwrap() {
            JobMessage::ProfileDataSource {
                org_id,
                data_source_id,
                data,
            } => assert_eq!(
                (org_id, data_source_id, data),
                (1, 2, crate::models::ProfileData::All)
            ),
            other => panic!("expected ProfileDataSource, got {other:?}"),
        }
        let legacy = r#"{"type":"ProfileDataSource","org_id":1,"data_source_id":2}"#;
        assert!(matches!(
            serde_json::from_str::<JobMessage>(legacy).unwrap(),
            JobMessage::ProfileDataSource {
                data: crate::models::ProfileData::Changed,
                ..
            }
        ));
        let none = r#"{"type":"ProfileDataSource","org_id":1,"data_source_id":2,"data":"none"}"#;
        assert!(matches!(
            serde_json::from_str::<JobMessage>(none).unwrap(),
            JobMessage::ProfileDataSource {
                data: crate::models::ProfileData::None,
                ..
            }
        ));
    }

    #[test]
    fn job_message_describe_data_source_round_trips_through_json() {
        let msg = JobMessage::DescribeDataSource {
            org_id: 1,
            data_source_id: 2,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"DescribeDataSource","org_id":1,"data_source_id":2}"#
        );
        assert!(matches!(
            serde_json::from_str::<JobMessage>(&json).unwrap(),
            JobMessage::DescribeDataSource {
                org_id: 1,
                data_source_id: 2
            }
        ));
    }

    #[tokio::test]
    async fn enqueue_profile_dedupes_and_sets_schema_phase() {
        let Ok(url) = std::env::var("TEST_REDIS_URL") else {
            return;
        };
        let _guard = SCHEMAS_QUEUE_LOCK.lock().await;
        let conn = crate::valkey_client::connect(&url).await.unwrap();
        let queue = ValkeyQueue::new(conn.clone());
        let mut redis = conn.clone();
        let id = 987_654_321;
        let key = crate::models::ai_profile_pending_key(id);
        let _: () = redis.del(&key).await.unwrap();
        let _: () = redis.del(QueueName::Schemas.queue_key()).await.unwrap();
        assert!(
            enqueue_profile(&mut redis, &queue, 1, id, crate::models::ProfileData::All)
                .await
                .unwrap()
        );
        assert!(
            !enqueue_profile(&mut redis, &queue, 1, id, crate::models::ProfileData::All)
                .await
                .unwrap()
        );
        let v: String = redis.get(&key).await.unwrap();
        assert_eq!(v, "schema");
        let _: () = redis.del(&key).await.unwrap();
        let _: () = redis.del(QueueName::Schemas.queue_key()).await.unwrap();
    }

    /// An `ExecuteQuery` payload without `run_id` must still deserialize (defaults to `None`) so
    /// in-flight messages survive a rolling deploy.
    #[test]
    fn job_message_execute_query_without_run_id_key_defaults_to_none() {
        let json = serde_json::json!({
            "type": "ExecuteQuery",
            "job_id": "abc-123",
            "org_id": 1,
            "data_source_id": 2,
            "query_text": "SELECT 1",
            "parameters": {},
            "user_id": 3,
            "query_id": 42,
            "scheduled": false,
        });
        let parsed: JobMessage = serde_json::from_value(json).unwrap();
        match parsed {
            JobMessage::ExecuteQuery {
                run_id, max_rows, ..
            } => {
                assert_eq!(run_id, None);
                assert_eq!(max_rows, None);
            }
            other => panic!("expected ExecuteQuery, got {other:?}"),
        }
    }

    /// Same wire-compat guarantee for `repoint_published` (defaults to `false`).
    #[test]
    fn job_message_execute_query_without_repoint_published_key_defaults_to_false() {
        let json = serde_json::json!({
            "type": "ExecuteQuery",
            "job_id": "abc-123",
            "org_id": 1,
            "data_source_id": 2,
            "query_text": "SELECT 1",
            "parameters": {},
            "user_id": 3,
            "query_id": 42,
            "scheduled": false,
        });
        let parsed: JobMessage = serde_json::from_value(json).unwrap();
        match parsed {
            JobMessage::ExecuteQuery {
                repoint_published, ..
            } => assert!(!repoint_published),
            other => panic!("expected ExecuteQuery, got {other:?}"),
        }
    }

    #[test]
    fn job_status_record_skips_none_fields_when_serializing() {
        let record = JobStatusRecord {
            org_id: 1,
            user_id: None,
            status: JobStatus::Pending,
            query_result_id: None,
            error: None,
            updated_at: Utc::now(),
        };
        let json = serde_json::to_value(&record).unwrap();
        assert!(json.get("query_result_id").is_none());
        assert!(json.get("error").is_none());
    }

    /// Same wire-compat guarantee for `SendNotification.run_id`.
    #[test]
    fn job_message_send_notification_without_run_id_key_defaults_to_none() {
        let json = serde_json::json!({
            "type": "SendNotification",
            "org_id": 1,
            "monitor_id": 2,
            "destination_id": 3,
            "new_state": "triggered",
            "subscriber_id": 4,
        });
        let parsed: JobMessage = serde_json::from_value(json).unwrap();
        match parsed {
            JobMessage::SendNotification { run_id, .. } => assert_eq!(run_id, None),
            other => panic!("expected SendNotification, got {other:?}"),
        }
    }

    /// Same wire-compat guarantee for `SendDigest.delivery_id`.
    #[test]
    fn job_message_send_digest_without_delivery_id_key_defaults_to_none() {
        let json = serde_json::json!({
            "type": "SendDigest",
            "org_id": 1,
            "user_id": 2,
        });
        let parsed: JobMessage = serde_json::from_value(json).unwrap();
        match parsed {
            JobMessage::SendDigest { delivery_id, .. } => assert_eq!(delivery_id, None),
            other => panic!("expected SendDigest, got {other:?}"),
        }
    }

    #[test]
    fn job_message_send_digest_round_trips_with_delivery_id() {
        let msg = JobMessage::SendDigest {
            org_id: 1,
            user_id: 2,
            delivery_id: Some(42),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: JobMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            JobMessage::SendDigest { delivery_id, .. } => assert_eq!(delivery_id, Some(42)),
            other => panic!("expected SendDigest, got {other:?}"),
        }
    }

    #[test]
    fn job_message_send_mail_without_delivery_id_key_defaults_to_none() {
        let json = serde_json::json!({
            "type": "SendMail",
            "org_id": 1,
            "to": "a@example.com",
            "subject": "hi",
            "body_html": "hello",
        });
        let parsed: JobMessage = serde_json::from_value(json).unwrap();
        match parsed {
            JobMessage::SendMail { delivery_id, .. } => assert_eq!(delivery_id, None),
            other => panic!("expected SendMail, got {other:?}"),
        }
    }

    #[test]
    fn job_message_send_mail_round_trips_with_delivery_id() {
        let msg = JobMessage::SendMail {
            org_id: 1,
            to: "a@example.com".to_string(),
            subject: "hi".to_string(),
            body_html: "hello".to_string(),
            delivery_id: Some(7),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: JobMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            JobMessage::SendMail { delivery_id, .. } => assert_eq!(delivery_id, Some(7)),
            other => panic!("expected SendMail, got {other:?}"),
        }
    }

    #[test]
    fn job_message_resume_monitor_evaluation_round_trips() {
        let msg = JobMessage::ResumeMonitorEvaluation {
            org_id: 1,
            run_id: 42,
            query_run_id: 7,
            query_run_status: "success".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"ResumeMonitorEvaluation\""));
        let parsed: JobMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            JobMessage::ResumeMonitorEvaluation {
                org_id,
                run_id,
                query_run_id,
                query_run_status,
            } => {
                assert_eq!(org_id, 1);
                assert_eq!(run_id, 42);
                assert_eq!(query_run_id, 7);
                assert_eq!(query_run_status, "success");
            }
            other => panic!("expected ResumeMonitorEvaluation, got {other:?}"),
        }
    }
}
