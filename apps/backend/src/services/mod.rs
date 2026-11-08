//! Business logic, no HTTP types. One module per resource; handlers in `api/handlers/` call these.

pub mod change_service;
pub mod dashboard_service;
pub mod data_source_service;
pub mod destination_service;
pub mod display_names;
pub mod embed_service;
pub mod job_service;
pub mod mailer;
pub mod monitor_service;
pub mod parameters;
pub mod permission_service;
pub mod query_service;
pub mod result_service;
pub mod run_service;
pub mod schedule_service;
pub mod snippet_service;
pub mod upload_service;
pub mod user_service;
pub mod visualization_service;
pub mod widget_service;
