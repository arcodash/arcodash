//! The JSON wire envelope RPUSH'd onto a task type's queue and BLPOP'd back by a consumer tier.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{TaskId, TaskType};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub task_id: TaskId,
    pub task_type: TaskType,
    pub payload: serde_json::Value,
    pub attempt: u32,
    pub enqueued_at: DateTime<Utc>,
    /// `true` for a resume delivery (`lifecycle.rs`'s fan-out) — the consumer skips the
    /// `pending->running` claim CAS for these, since the row is already `running` (parked).
    #[serde(default)]
    pub resume: bool,
}

/// The payload a resume envelope carries — built at fan-out time from the task that just went
/// terminal, never stored on the row itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumePayload {
    pub done_task_id: TaskId,
    pub done_task_status: String,
}
