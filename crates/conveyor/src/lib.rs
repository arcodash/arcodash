//! `arcodash-conveyor` — a domain-agnostic task queue/scheduler SDK.
//!
//! Register handlers per task type, group them into concurrency tiers, and get back a `Conveyor`
//! handle to enqueue/schedule work and read its status back. Owns its own Postgres tables
//! (`conveyor_tasks`/`conveyor_schedules`, see `schema.rs`) — a caller applies them via its own
//! migration tooling; this crate never runs its migrator outside its own tests.
//!
//! Entry point: [`ConveyorBuilder`].

mod builder;
mod config;
mod consumer;
mod conveyor;
mod entities;
mod envelope;
mod handler;
mod ids;
mod lease;
mod lifecycle;
mod queue;
mod reporting;
mod resource;
pub mod schedule_spec;
mod scheduler;
mod sweeper;
pub mod worker_identity;

pub mod schema;

#[cfg(test)]
mod test_support;

pub use builder::ConveyorBuilder;
pub use consumer::connect_with_retry;
pub use conveyor::{
    AlreadyInflight, Conveyor, EnqueueRequest, Enqueued, LiveSnapshot, OverlapPolicy,
    ScheduleRequest,
};
pub use handler::{
    TaskContext, TaskHandler, TaskOutcome, TaskResult, TypedTaskHandler, WorkerHeartbeat,
};
pub use ids::{ScheduleId, TaskId, TaskType};
pub use lease::{HeartbeatGuard, HEARTBEAT_LEASE_LOST};
pub use lifecycle::{FINALIZE_LOST_RACE, SCHEDULE_AUTO_PAUSED};
pub use queue::{blpop, get_job_status, set_job_status, JobStatusRecord};
pub use scheduler::{
    acquire_leader_lock, release_leader_lock, LeaderLockOutcome, LEADER_ACQUIRE_CONTENDED,
    LEADER_ACQUIRE_FAILED, LEADER_LOCK_TTL_MS,
};
pub use sweeper::{
    SWEEPER_RECLAIMED_ABANDONED_PENDING, SWEEPER_RECLAIMED_EXPIRED_LEASE,
    SWEEPER_RECLAIMED_STALE_PENDING, SWEEPER_RELEASED_ORPHANED_WAITER,
};
