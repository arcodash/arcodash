//! Shared SMTP transport builder used by both `apps/backend` and `apps/worker` so `SMTP_TLS`
//! is honored identically everywhere mail is sent (backend invitation/password-reset emails,
//! worker monitor emails).
//!
//! `SMTP_TLS` picks the transport's TLS mode (`relay()` alone is implicit-TLS-on-connect, which
//! silently fails to even establish a connection against any plaintext-only relay, e.g. Mailpit
//! in the E2E stack, or many providers' STARTTLS-only port 587). `"wrapper"` (default) preserves
//! implicit TLS for providers that want it; `"starttls"` and `"none"` cover the other two
//! real-world cases. `Config::from_env` validates `smtp_tls` is one of these three at startup.

use std::sync::Arc;

use lettre::message::header::{self, ContentType};
use lettre::message::{Mailboxes, MessageBuilder, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::Config;

pub type Mailer = AsyncSmtpTransport<Tokio1Executor>;

/// What actually happened to a `send_html_email`/`send_email` call.
///
/// Distinguishes "no mailer configured, nothing was sent" from a real SMTP send — an unconfigured
/// `SMTP_HOST` is not a delivery, and a caller that records delivery status must be able to tell
/// the two apart instead of recording every unconfigured deployment as having successfully sent
/// every email.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailOutcome {
    Sent,
    /// `SMTP_HOST` is unset — not an error, just nothing to send with.
    Skipped,
}

/// Build an optional SMTP mailer from config.
///
/// Returns `None` when `SMTP_HOST` is unset, or when the transport itself fails to construct (e.g.
/// no root certificates available for TLS) — the latter is logged loudly here, at the one place
/// that can still say *why*, since every later send only ever sees `None` and would otherwise
/// report the misleading "not configured".
#[must_use]
pub fn build_mailer(cfg: &Config) -> Option<Arc<Mailer>> {
    let host = cfg.smtp_host.as_deref()?;
    let tls_mode = cfg.smtp_tls.as_str();
    let builder = match tls_mode {
        "none" => Ok(Mailer::builder_dangerous(host)),
        "starttls" => Mailer::starttls_relay(host),
        _ => Mailer::relay(host),
    };
    let mut builder = builder
        .inspect_err(
            |e| tracing::error!(host, tls_mode, error = %e, "failed to build SMTP transport"),
        )
        .ok()?
        .port(cfg.smtp_port);
    if let (Some(u), Some(p)) = (cfg.smtp_username.as_deref(), cfg.smtp_password.as_deref()) {
        builder = builder.credentials(Credentials::new(u.to_string(), p.to_string()));
    }
    Some(Arc::new(builder.build()))
}

/// `Mailbox` (used by `.to()`) parses exactly one RFC 5322 mailbox and errors on a comma;
/// `Mailboxes` parses the same grammar's comma-separated list, which alert destinations need.
fn to_header(to: &str) -> anyhow::Result<header::To> {
    Ok(to.parse::<Mailboxes>()?.into())
}

/// Addresses a message and sends it, or logs-and-returns `Skipped` when `mailer` is absent (SMTP
/// not configured). `body` attaches the content to the addressed builder.
async fn send(
    mailer: Option<&Arc<Mailer>>,
    from: &str,
    to: &str,
    subject: &str,
    body: impl FnOnce(MessageBuilder) -> Result<Message, lettre::error::Error>,
) -> anyhow::Result<MailOutcome> {
    let Some(m) = mailer else {
        tracing::info!(%to, %subject, "email send skipped: no mailer configured");
        return Ok(MailOutcome::Skipped);
    };
    let to_header = to_header(to)?;
    let msg = body(
        Message::builder()
            .from(from.parse()?)
            .mailbox(to_header)
            .subject(subject),
    )?;
    m.send(msg)
        .await
        .map_err(|e| anyhow::anyhow!("SMTP send failed: {e}"))?;
    Ok(MailOutcome::Sent)
}

/// Send an HTML email with a plain-text fallback part. Shared by the backend (invitations) and
/// the worker (monitor alerts) so this MIME-construction logic exists exactly once.
pub async fn send_html_email(
    mailer: &Option<Arc<Mailer>>,
    from: &str,
    to: &str,
    subject: &str,
    text: &str,
    html: &str,
) -> anyhow::Result<MailOutcome> {
    send(mailer.as_ref(), from, to, subject, |b| {
        b.multipart(MultiPart::alternative_plain_html(
            text.to_string(),
            html.to_string(),
        ))
    })
    .await
}

/// Send a plain-text email. Explicit `Content-Type` matters: without it, lettre emits no
/// charset, and any non-ASCII body byte gets mojibake'd by mail clients defaulting to Latin-1.
pub async fn send_email(
    mailer: &Option<Arc<Mailer>>,
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
) -> anyhow::Result<MailOutcome> {
    send(mailer.as_ref(), from, to, subject, |b| {
        b.header(ContentType::TEXT_PLAIN).body(body.to_string())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No `mailer` (SMTP unconfigured) must report `Skipped`, not `Sent` — an unconfigured
    /// deployment must never look like it successfully delivered mail.
    #[tokio::test]
    async fn send_email_reports_skipped_when_mailer_unconfigured() {
        let outcome = send_email(&None, "from@test.local", "to@test.local", "subj", "body")
            .await
            .unwrap();
        assert_eq!(outcome, MailOutcome::Skipped);
    }

    /// A comma-separated `to` (as alert destinations allow) must build a real multi-recipient
    /// `To` header — a `Mailbox`-only implementation errors on the comma instead.
    #[test]
    fn to_header_builds_multiple_addresses_for_comma_separated_recipients() {
        let header = to_header("sam@x.io, joe@x.io, ann@x.io").unwrap();
        let msg = Message::builder()
            .from("from@test.local".parse().unwrap())
            .mailbox(header)
            .subject("subj")
            .body("body".to_string())
            .unwrap();
        assert_eq!(msg.envelope().to().len(), 3);
    }

    /// The common single-recipient case must still produce exactly one address.
    #[test]
    fn to_header_builds_one_address_for_a_single_recipient() {
        let header = to_header("sam@x.io").unwrap();
        let msg = Message::builder()
            .from("from@test.local".parse().unwrap())
            .mailbox(header)
            .subject("subj")
            .body("body".to_string())
            .unwrap();
        assert_eq!(msg.envelope().to().len(), 1);
    }

    #[tokio::test]
    async fn send_html_email_reports_skipped_when_mailer_unconfigured() {
        let outcome = send_html_email(
            &None,
            "from@test.local",
            "to@test.local",
            "subj",
            "text",
            "<p>html</p>",
        )
        .await
        .unwrap();
        assert_eq!(outcome, MailOutcome::Skipped);
    }

    /// `to_header()`'s comma-separated parsing must not just build a `Message` in memory — it
    /// must reach a real SMTP server as a single message addressed to every recipient. Sends
    /// through the test stack's Mailpit and confirms via Mailpit's HTTP API that all three
    /// addresses landed on the one delivered message. Skips cleanly (doesn't fail) if Mailpit
    /// isn't reachable, matching `apps/backend/tests/integration_email_delivery.rs`'s
    /// `real_mailpit_send_records_no_failure_event`.
    #[tokio::test]
    async fn send_html_email_delivers_to_all_comma_separated_recipients_via_real_smtp() {
        let mailpit_api = std::env::var("MAILPIT_API_BASE")
            .unwrap_or_else(|_| "http://localhost:8026".to_string());
        let http = reqwest::Client::new();
        if http
            .get(format!("{mailpit_api}/api/v1/info"))
            .send()
            .await
            .is_err()
        {
            eprintln!("skipping: Mailpit not reachable at {mailpit_api}");
            return;
        }

        let smtp_host =
            std::env::var("MAILPIT_SMTP_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let smtp_port: u16 = std::env::var("MAILPIT_SMTP_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1026);
        let mailer: Option<Arc<Mailer>> = Some(Arc::new(
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&smtp_host)
                .port(smtp_port)
                .build(),
        ));

        let stamp = uuid::Uuid::new_v4().simple().to_string();
        let to = format!("a-{stamp}@test.local, b-{stamp}@test.local, c-{stamp}@test.local");
        let subject = format!("multi-recipient-{stamp}");

        let outcome = send_html_email(
            &mailer,
            "from@test.local",
            &to,
            &subject,
            "text",
            "<p>html</p>",
        )
        .await
        .expect("real send to Mailpit must succeed");
        assert_eq!(outcome, MailOutcome::Sent);

        let search = http
            .get(format!("{mailpit_api}/api/v1/search"))
            .query(&[("query", subject.as_str())])
            .send()
            .await
            .expect("Mailpit search request failed")
            .json::<serde_json::Value>()
            .await
            .expect("Mailpit search response was not JSON");

        let recipients: Vec<String> = search["messages"][0]["To"]
            .as_array()
            .expect("delivered message must have a To array")
            .iter()
            .map(|r| r["Address"].as_str().unwrap().to_string())
            .collect();

        for addr in ["a", "b", "c"] {
            let expected = format!("{addr}-{stamp}@test.local");
            assert!(
                recipients.contains(&expected),
                "expected {expected} among delivered recipients {recipients:?}"
            );
        }
        assert_eq!(
            recipients.len(),
            3,
            "must deliver to exactly the 3 addressed recipients, got {recipients:?}"
        );
    }

    // build_mailer's TLS-construction-failure branch (the `Err` arms of `relay()`/
    // `starttls_relay()`) is not forceable from a test: verified against lettre 0.11's rustls
    // backend that neither call validates the hostname string or touches the root cert store at
    // construction time (both are deferred to actual connect) — a syntactically invalid host
    // still builds successfully, and forcing "no root certificates available" would need a
    // fabricated TLS environment, which isn't worth building here.

    /// Single test (not split across fns) since `env::set_var` is process-global and
    /// `cargo test` runs tests in parallel threads by default — splitting these would race.
    #[test]
    fn smtp_tls_values_map_to_the_right_transport_and_reject_garbage() {
        unsafe {
            std::env::set_var("DATABASE_URL", "postgres://x");
            std::env::set_var("REDIS_URL", "redis://x");
            std::env::set_var("ENCRYPTION_KEY", "0".repeat(64));
            std::env::set_var("COOKIE_SECRET", "s");
            std::env::set_var("SMTP_HOST", "localhost");
        }

        for tls in ["wrapper", "starttls", "none"] {
            unsafe { std::env::set_var("SMTP_TLS", tls) };
            let cfg = Config::from_env().expect("valid SMTP_TLS should load");
            assert_eq!(cfg.smtp_tls, tls);
            assert!(
                build_mailer(&cfg).is_some(),
                "expected a mailer for SMTP_TLS={tls}"
            );
        }

        unsafe { std::env::set_var("SMTP_TLS", "yolo") };
        assert!(
            Config::from_env().is_err(),
            "invalid SMTP_TLS should fail Config::from_env"
        );

        unsafe {
            std::env::remove_var("SMTP_TLS");
            std::env::remove_var("SMTP_HOST");
        }
    }
}
