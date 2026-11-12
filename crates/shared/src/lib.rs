pub mod ai;
pub mod ai_relationships;
pub mod alert_explanation;
pub mod authz;
pub mod baseline;
pub mod chat_body;
pub mod config;
pub mod connectors;
pub mod db;
pub mod digest_schedule;
pub mod email_template;
pub mod entities;
pub mod entitlements;
pub mod events;
pub mod hash;
pub mod mail_notify;
pub mod mailer;
pub mod metrics;
pub mod models;
pub mod monitor_eval;
pub mod net_guard;
pub mod notify;
pub mod org_seed;
pub mod org_settings;
pub mod org_storage;
pub mod permissions_catalog;
pub mod query_exec;
pub mod queue;
pub mod redis_util;
pub mod secrets;
pub mod shutdown;
pub mod storage;
pub mod user_details;
pub mod valkey_client;

pub use arcodash_conveyor::schedule_spec;
/// Re-exported for the same reason as `schedule_spec` above: `arcodash-conveyor` owns the one
/// implementation (it's upstream of this crate in the dependency graph, so it can't depend back
/// on `arcodash-core`), and every caller of `worker_id()` reaches it through here.
pub use arcodash_conveyor::worker_identity;
/// Re-exported so a caller that only depends on `arcodash-core` (e.g. the backend, which routes
/// to conveyor only through this crate's producer helpers) can name the type without adding a
/// direct `arcodash-conveyor` dependency.
pub use arcodash_conveyor::Conveyor;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use config::Config;
