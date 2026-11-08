//! Audit events: thin re-export of `arcodash_core::events::record_event` (shared with the worker,
//! which records its own job success/failure events).
//!
//! Wired on login success/failure and query execution outcomes.

pub use arcodash_core::events::record_event;
