//! `ConveyorBuilder` — register task-type handlers, group them into concurrency tiers, then
//! `build()` into a shared `Conveyor` handle. Duplicate registration and a tier naming an
//! unregistered task type are both deferred to `build()` (not caught at `register()`/`tier()`
//! time) so registration order never matters.

use std::collections::HashMap;
use std::sync::Arc;

use redis::aio::ConnectionManager;
use sea_orm::DatabaseConnection;

use crate::conveyor::{Conveyor, TierSpec};
use crate::handler::TaskHandler;
use crate::ids::TaskType;

pub struct ConveyorBuilder {
    db: DatabaseConnection,
    valkey: ConnectionManager,
    redis_url: String,
    registrations: Vec<(TaskType, Arc<dyn TaskHandler>)>,
    tiers: Vec<TierSpec>,
}

impl ConveyorBuilder {
    /// `redis_url` opens the dedicated connections the consumer's `BLPOP` and the scheduler's
    /// wake `SUBSCRIBE` need — `valkey` (a `ConnectionManager`) multiplexes many callers over one
    /// shared connection, so a blocking command on it would stall every other command queued
    /// behind it.
    #[must_use]
    pub fn new(db: DatabaseConnection, valkey: ConnectionManager, redis_url: String) -> Self {
        Self {
            db,
            valkey,
            redis_url,
            registrations: Vec::new(),
            tiers: Vec::new(),
        }
    }

    #[must_use]
    pub fn register(mut self, task_type: impl Into<TaskType>, handler: impl TaskHandler) -> Self {
        self.registrations
            .push((task_type.into(), Arc::new(handler)));
        self
    }

    /// Declares a tier with the crate's default drain grace
    /// ([`crate::consumer::DEFAULT_DRAIN_SECS`]) — fine for tiers whose task types are normally
    /// sub-second. A tier carrying genuinely long-running work should use
    /// [`Self::tier_with_drain`] instead so shutdown doesn't abandon it well before it can finish.
    #[must_use]
    pub fn tier(self, name: &str, task_types: &[&str], concurrency: usize) -> Self {
        self.tier_with_drain(
            name,
            task_types,
            concurrency,
            crate::consumer::DEFAULT_DRAIN_SECS,
        )
    }

    /// Declares a tier with an explicit shutdown drain grace, in seconds. A tier carrying
    /// multi-hour tasks needs a grace on the order of the work it carries, not the crate's short
    /// default.
    #[must_use]
    pub fn tier_with_drain(
        mut self,
        name: &str,
        task_types: &[&str],
        concurrency: usize,
        drain_secs: u64,
    ) -> Self {
        self.tiers.push(TierSpec {
            name: name.to_string(),
            task_types: task_types.iter().map(|s| TaskType::from(*s)).collect(),
            concurrency,
            drain_secs,
        });
        self
    }

    /// Panics if the same task type was registered more than once, or if a tier names a task
    /// type nothing registered a handler for.
    #[must_use]
    pub fn build(self) -> Arc<Conveyor> {
        let mut registry: HashMap<TaskType, Arc<dyn TaskHandler>> = HashMap::new();
        for (task_type, handler) in self.registrations {
            assert!(
                registry.insert(task_type.clone(), handler).is_none(),
                "arcodash-conveyor: duplicate registration for task type {task_type}"
            );
        }

        for tier in &self.tiers {
            for task_type in &tier.task_types {
                assert!(
                    registry.contains_key(task_type),
                    "arcodash-conveyor: tier {:?} references unregistered task type {task_type}",
                    tier.name
                );
            }
        }

        let mut seen_in_tier: HashMap<&TaskType, &str> = HashMap::new();
        for tier in &self.tiers {
            for task_type in &tier.task_types {
                if let Some(first_tier) = seen_in_tier.insert(task_type, &tier.name) {
                    panic!(
                        "arcodash-conveyor: task type {task_type} is registered in two tiers \
                         ({first_tier:?} and {:?}) — each type's concurrency budget must come \
                         from exactly one tier",
                        tier.name
                    );
                }
            }
        }

        for task_type in registry.keys() {
            assert!(
                seen_in_tier.contains_key(task_type),
                "arcodash-conveyor: task type {task_type} is registered but no tier ever \
                 dispatches it, so it can never run"
            );
        }

        Arc::new(Conveyor {
            db: self.db,
            valkey: self.valkey,
            redis_url: self.redis_url,
            registry: Arc::new(registry),
            tiers: self.tiers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::{TaskContext, TaskOutcome, TaskResult};
    use async_trait::async_trait;

    struct Noop;

    #[async_trait]
    impl TaskHandler for Noop {
        async fn run(&self, _ctx: TaskContext, _payload: serde_json::Value) -> TaskResult {
            Ok(TaskOutcome::Success { result_ref: None })
        }
    }

    fn builder() -> ConveyorBuilder {
        // Neither field is touched by `build()` itself (no I/O happens until `run()`/
        // `enqueue()`/etc.) — a disconnected db and a lazy (not-yet-connected) valkey manager are
        // safe here, since the whole point of this test is that `build()` panics before any of
        // that matters.
        let db = sea_orm::DatabaseConnection::default();
        let client = redis::Client::open("valkey://127.0.0.1:1").unwrap();
        let valkey = redis::aio::ConnectionManager::new_lazy_with_config(
            client,
            redis::aio::ConnectionManagerConfig::new(),
        )
        .unwrap();
        ConveyorBuilder::new(db, valkey, "valkey://127.0.0.1:1".to_string())
    }

    #[tokio::test]
    #[should_panic(expected = "duplicate registration")]
    async fn build_panics_on_duplicate_registration() {
        let _ = builder()
            .register("toy", Noop)
            .register("toy", Noop)
            .build();
    }

    #[tokio::test]
    #[should_panic(expected = "unregistered task type")]
    async fn build_panics_on_tier_naming_unregistered_task_type() {
        let _ = builder().tier("fast", &["toy"], 1).build();
    }

    #[tokio::test]
    #[should_panic(expected = "no tier ever dispatches it")]
    async fn build_panics_on_a_registered_task_type_with_no_tier() {
        let _ = builder().register("toy", Noop).build();
    }

    #[tokio::test]
    async fn build_succeeds_with_consistent_registrations_and_tiers() {
        let conveyor = builder()
            .register("toy", Noop)
            .tier("fast", &["toy"], 1)
            .build();
        assert!(conveyor.registry.contains_key(&TaskType::from("toy")));
    }

    #[tokio::test]
    #[should_panic(expected = "registered in two tiers")]
    async fn build_rejects_a_task_type_registered_in_two_tiers() {
        let _ = builder()
            .register("toy", Noop)
            .tier("fast", &["toy"], 1)
            .tier("slow", &["toy"], 1)
            .build();
    }

    #[tokio::test]
    async fn build_succeeds_when_each_task_type_is_in_exactly_one_tier() {
        let conveyor = builder()
            .register("toy_a", Noop)
            .register("toy_b", Noop)
            .tier("fast", &["toy_a"], 1)
            .tier("slow", &["toy_b"], 1)
            .build();
        assert!(conveyor.registry.contains_key(&TaskType::from("toy_a")));
        assert!(conveyor.registry.contains_key(&TaskType::from("toy_b")));
    }
}
