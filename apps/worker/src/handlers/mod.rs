//! Job handlers, one module per `JobMessage` variant. Queues for variants without a handler
//! are still consumed (see `consumer::valkey_consumer`); such messages are logged and dropped.

pub mod describe_data_source;
pub mod digest_fanout;
pub mod escalation_tick;
pub mod evaluate_monitor;
pub mod execute_query;
pub mod explain_alert;
pub mod profile_data_source;
pub mod resume_monitor;
pub mod schema_daily;
pub mod send_digest;
pub mod send_mail;
pub mod send_notification;
