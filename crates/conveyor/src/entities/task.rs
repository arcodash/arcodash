use sea_orm::entity::prelude::*;

/// One row per enqueued/scheduled task. No CHECK on `task_type` (arbitrary caller-defined
/// strings), `entity_id` nullable (NULLs never collide in `uq_conveyor_tasks_inflight`, which is
/// what makes "fire freely" — `entity_id: None` — actually work), no org FK (conveyor is
/// tenant-agnostic; `tenant_id` is a plain nullable column a caller may or may not populate).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "conveyor_tasks")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub tenant_id: Option<i64>,
    pub task_type: String,
    pub entity_id: Option<i64>,
    pub schedule_id: Option<i64>,
    /// The handler payload, stored on the row so a `pending` task that never made it onto its
    /// queue (over the resource cap, or lost before enqueue) can be rebuilt and re-enqueued later
    /// — unlike a domain-specific table, this crate has nowhere else to re-derive a payload from
    /// at retry time.
    pub payload: Option<Json>,
    pub resource_key: Option<String>,
    pub resource_cap: Option<i64>,
    pub scheduled_for: Option<DateTimeWithTimeZone>,
    pub triggered_by: String,
    pub status: String,
    pub attempt: i32,
    /// The `WaitOn` park link (see `lifecycle.rs`'s park/resume doc comment).
    pub waiting_on_task_id: Option<i64>,
    pub worker_id: Option<String>,
    pub lease_expires_at: Option<DateTimeWithTimeZone>,
    pub enqueued_at: Option<DateTimeWithTimeZone>,
    pub started_at: Option<DateTimeWithTimeZone>,
    pub finished_at: Option<DateTimeWithTimeZone>,
    pub duration_ms: Option<i64>,
    pub error: Option<String>,
    pub result_ref: Option<Json>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
