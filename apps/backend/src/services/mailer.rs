//! SMTP mailer helper for the backend (invitation + password reset emails).
//!
//! Transport construction and the actual send functions live in `arcodash_core::mailer` so the
//! backend and worker never diverge on `SMTP_TLS` handling or MIME construction — re-exported
//! here rather than duplicated.

pub use arcodash_core::mailer::{build_mailer, send_email, send_html_email, Mailer};
