//! Newtypes over the physical id/type representations conveyor persists (`bigint` ids, `text`
//! task type) — keeps a stray `i64`/`String` from being passed where a task id or type was meant.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub i64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScheduleId(pub i64);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskType(pub String);

impl From<&str> for TaskType {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for TaskType {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for TaskType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
