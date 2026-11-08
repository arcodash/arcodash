//! The handler contract a caller registers against a task type (`ConveyorBuilder::register`).
//! `TaskHandler` is the raw JSON-payload form the consumer dispatches through; `TypedTaskHandler`
//! is sugar for a handler whose payload deserializes to a concrete type, blanket-implemented onto
//! `TaskHandler` so both fit the same registry.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::ids::{TaskId, TaskType};

/// Everything a handler needs about the task it was invoked for, besides the payload itself.
pub struct TaskContext {
    pub task_id: TaskId,
    pub task_type: TaskType,
    pub attempt: u32,
    pub tenant_id: Option<i64>,
    pub entity_id: Option<i64>,
}

/// What a handler run produced. `WaitOn` parks the task rather than finalizing it — see
/// `lifecycle.rs`'s park/resume doc comment.
pub enum TaskOutcome {
    Success {
        result_ref: Option<serde_json::Value>,
    },
    Failed {
        error: String,
    },
    Cancelled,
    WaitOn {
        task_id: TaskId,
    },
}

pub type TaskResult = Result<TaskOutcome, anyhow::Error>;

/// The registry's storage shape: opaque JSON payload in, outcome out. A handler `Err`/panic is
/// caught by the consumer's spawn boundary and turned into `Failed`, not propagated.
#[async_trait]
pub trait TaskHandler: Send + Sync + 'static {
    async fn run(&self, ctx: TaskContext, payload: serde_json::Value) -> TaskResult;
}

/// Sugar for a handler whose payload is a concrete, deserializable type — most handlers should
/// implement this instead of `TaskHandler` directly.
#[async_trait]
pub trait TypedTaskHandler: Send + Sync + 'static {
    type Payload: serde::de::DeserializeOwned + Send;
    async fn run(&self, ctx: TaskContext, payload: Self::Payload) -> TaskResult;
}

#[async_trait]
impl<T: TypedTaskHandler> TaskHandler for T {
    async fn run(&self, ctx: TaskContext, payload: serde_json::Value) -> TaskResult {
        let typed = serde_json::from_value(payload)?;
        TypedTaskHandler::run(self, ctx, typed).await
    }
}

/// Worker heartbeat entry for `LiveSnapshot`.
#[derive(Serialize)]
pub struct WorkerHeartbeat {
    pub worker_id: String,
    pub last_seen: DateTime<Utc>,
}
