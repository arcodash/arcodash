use std::env;
use std::net::IpAddr;

use anyhow::{Context, Result};

use crate::queue::QueueName;

/// Parse `WORKER_QUEUES`. Unset → all four queues; set but naming none is an error.
pub fn parse_worker_queues(raw: Option<String>) -> Result<Vec<QueueName>> {
    let Some(raw) = raw else {
        return Ok(QueueName::ALL.to_vec());
    };
    let queues = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            QueueName::parse(s)
                .ok_or_else(|| anyhow::anyhow!("unknown queue name in WORKER_QUEUES: {s:?}"))
        })
        .collect::<Result<Vec<_>>>()?;
    if queues.is_empty() {
        anyhow::bail!("WORKER_QUEUES is set but names no queues; unset it to consume all four");
    }
    Ok(queues)
}

/// Application configuration loaded from environment variables (via `.env` + process env).
///
/// Required: `DATABASE_URL`, `VALKEY_URL` (legacy `REDIS_URL` accepted), `ENCRYPTION_KEY`,
/// `COOKIE_SECRET`; everything else has a default (see `from_env`).
#[derive(Clone)]
pub struct Config {
    pub app_host: IpAddr,
    pub app_port: u16,
    pub database_url: String,
    pub valkey_url: String,
    pub rust_log: String,
    pub base_url: String,
    /// Hex-encoded 32-byte key used by `crate::secrets` (AES-256-GCM) for data source options and
    /// destination secrets.
    pub encryption_key: String,
    /// Secret used to sign session JWTs (HS256). Required; there is no safe default.
    pub cookie_secret: String,
    /// Kill-switch for anonymous public/secret-token access to dashboards/visualizations; checked
    /// first by `embed_service::resolve_public_embed` and reported in `/api/session`'s
    /// `client_config`.
    pub disable_public_urls: bool,
    /// [`QueueName`]s this worker subscribes to (default all four). Splitting fast
    /// (`default,queries,schemas`) and long (`scheduled_queries`) tiers across deployments keeps
    /// long queries from contending with interactive jobs.
    pub worker_queues: Vec<QueueName>,
    /// Concurrency budget for the fast tier (`default`, `queries`, `schemas`), sized large since
    /// jobs are sub-second. Must be `>= 1`; to disable a tier drop its queues from `WORKER_QUEUES`.
    pub fast_concurrency: usize,
    /// Concurrency budget for the long tier (`scheduled_queries`), sized small. Must be `>= 1`.
    pub long_concurrency: usize,
    /// How long the long tier waits for in-flight jobs on shutdown before abandoning them to the
    /// sweeper. Separate from the fast tier's fixed 30s drain (`valkey_consumer::FAST_DRAIN_SECS`)
    /// because long queries need a longer grace.
    pub long_drain_secs: u64,
    /// Optional SMTP relay for monitor emails. When unset, email notifications are logged but not
    /// sent.
    pub smtp_host: Option<String>,
    pub smtp_port: u16,
    pub smtp_username: Option<String>,
    pub smtp_password: Option<String>,
    pub mail_from: String,
    /// TLS mode for the SMTP relay: `"wrapper"` (default, implicit TLS, port 465), `"starttls"`
    /// (port 587), or `"none"` (plaintext, for local relays like Mailpit).
    pub smtp_tls: String,
    /// Name of an HTTP header carrying the pre-authenticated username (email), for use behind a trusted SSO proxy.
    /// **NEVER expose this header from untrusted origins.**
    pub remote_user_header: Option<String>,
    /// Raw `IdP` metadata XML; its presence enables SAML login (`GET /api/saml/login`, `POST
    /// /api/saml/acs`) since it carries the SSO URL, entity ID and signing cert. Parsed lazily per
    /// request by `api::handlers::saml`.
    pub saml_idp_metadata_xml: Option<String>,
    /// This SP's own entity ID, sent in the `AuthnRequest` issuer and expected as the assertion's
    /// audience. Defaults to `{base_url}/api/saml/metadata` when unset.
    pub saml_sp_entity_id: Option<String>,
    /// LDAP/AD server URL (`ldap://` or `ldaps://host:port`); with `ldap_search_base_dn` it enables
    /// `POST /api/ldap/login`.
    pub ldap_url: Option<String>,
    /// DN of a service account to bind before searching; anonymous bind when unset.
    pub ldap_bind_dn: Option<String>,
    pub ldap_bind_password: Option<String>,
    /// Base DN to search under for the user entry.
    pub ldap_search_base_dn: Option<String>,
    /// Search filter template; `{username}` is replaced with the RFC 4515-escaped username.
    /// Defaults to a generic `uid` filter (e.g. `(sAMAccountName={username})` for AD).
    pub ldap_user_filter: String,
    pub ldap_email_attribute: String,
    pub ldap_first_name_attribute: String,
    pub ldap_last_name_attribute: String,
    /// Optional attribute (e.g. AD's `memberOf`) whose values map to arcodash group names; unset =
    /// no group sync.
    pub ldap_group_attribute: Option<String>,
    /// Fallback Gemini API key when no organization has configured its own; `None` leaves the
    /// feature unavailable until one is set.
    pub gemini_api_key: Option<String>,
    /// Fallback `OpenAI` API key, as `gemini_api_key`.
    pub openai_api_key: Option<String>,
    /// Fallback Anthropic API key, as `gemini_api_key`.
    pub anthropic_api_key: Option<String>,
    /// System-default AI SQL generation rate limit (requests per user per minute) unless an org
    /// overrides it.
    pub ai_rate_limit_per_min: u32,
    /// Gemini model used for text-to-SQL generation (`POST /api/queries/generate_sql`).
    pub gemini_model: String,
    /// `OpenAI` model used for text-to-SQL generation. Mirrors `gemini_model`.
    pub openai_model: String,
    /// Anthropic model used for text-to-SQL generation. Mirrors `gemini_model`.
    pub anthropic_model: String,
    /// Hard wall-clock cap (seconds) on a single `connector.run_query` before the job fails with a
    /// timeout. Some connectors also set a DB-side timeout, but this `tokio::time::timeout` is
    /// authoritative.
    pub query_timeout_secs: u64,
    /// Row cap auto-appended as a `LIMIT` to unbounded `SELECT`/`WITH` queries for connectors that
    /// support it (`QueryRunner::supports_auto_limit`).
    pub max_query_rows: u32,
    /// Days a stored query result may go unread and unrefreshed before the retention sweep
    /// deletes it. Floored at 1.
    pub result_retention_days: i64,
    /// Trusted reverse-proxy hops in front of this process. `0` (default) means the TCP peer is the
    /// real client and `X-Forwarded-For` is ignored; a positive value tells the per-IP rate limiter
    /// how many trailing entries proxies appended. Validated eagerly because a silently-ignored
    /// typo is a rate-limiting/security bug; `middleware::rate_limit::trusted_proxy_hops` re-reads
    /// the env var itself.
    pub trusted_proxy_hops: u32,
}

/// Hand-written `Debug` rather than derived: this struct holds the encryption key, cookie secret,
/// SMTP/LDAP passwords and AI keys, which a derive would leak to any `{config:?}`. Fields ending in
/// `_key`, `_secret`, `_password` or `_token` print as a redaction marker; `Option`s still show
/// `None` vs `Some(<redacted>)`.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("app_host", &self.app_host)
            .field("app_port", &self.app_port)
            .field("database_url", &redact_url_password(&self.database_url))
            .field("valkey_url", &redact_url_password(&self.valkey_url))
            .field("rust_log", &self.rust_log)
            .field("base_url", &self.base_url)
            .field("encryption_key", &"<redacted>")
            .field("cookie_secret", &"<redacted>")
            .field("disable_public_urls", &self.disable_public_urls)
            .field("worker_queues", &self.worker_queues)
            .field("fast_concurrency", &self.fast_concurrency)
            .field("long_concurrency", &self.long_concurrency)
            .field("long_drain_secs", &self.long_drain_secs)
            .field("smtp_host", &self.smtp_host)
            .field("smtp_port", &self.smtp_port)
            .field("smtp_username", &self.smtp_username)
            .field("smtp_password", &redact_opt(self.smtp_password.as_ref()))
            .field("mail_from", &self.mail_from)
            .field("smtp_tls", &self.smtp_tls)
            .field("remote_user_header", &self.remote_user_header)
            .field("saml_idp_metadata_xml", &self.saml_idp_metadata_xml)
            .field("saml_sp_entity_id", &self.saml_sp_entity_id)
            .field("ldap_url", &self.ldap_url)
            .field("ldap_bind_dn", &self.ldap_bind_dn)
            .field(
                "ldap_bind_password",
                &redact_opt(self.ldap_bind_password.as_ref()),
            )
            .field("ldap_search_base_dn", &self.ldap_search_base_dn)
            .field("ldap_user_filter", &self.ldap_user_filter)
            .field("ldap_email_attribute", &self.ldap_email_attribute)
            .field("ldap_first_name_attribute", &self.ldap_first_name_attribute)
            .field("ldap_last_name_attribute", &self.ldap_last_name_attribute)
            .field("ldap_group_attribute", &self.ldap_group_attribute)
            .field("gemini_api_key", &redact_opt(self.gemini_api_key.as_ref()))
            .field("openai_api_key", &redact_opt(self.openai_api_key.as_ref()))
            .field(
                "anthropic_api_key",
                &redact_opt(self.anthropic_api_key.as_ref()),
            )
            .field("ai_rate_limit_per_min", &self.ai_rate_limit_per_min)
            .field("gemini_model", &self.gemini_model)
            .field("openai_model", &self.openai_model)
            .field("anthropic_model", &self.anthropic_model)
            .field("query_timeout_secs", &self.query_timeout_secs)
            .field("max_query_rows", &self.max_query_rows)
            .field("result_retention_days", &self.result_retention_days)
            .field("trusted_proxy_hops", &self.trusted_proxy_hops)
            .finish()
    }
}

/// `Some(<redacted>)`/`None` in place of an `Option<String>` secret, so "is it configured?" stays
/// answerable.
fn redact_opt(field: Option<&String>) -> Option<&'static str> {
    field.as_ref().map(|_| "<redacted>")
}

/// Masks the password in a `scheme://user:password@host/...` URL, leaving the rest intact;
/// unchanged if there are no embedded credentials.
fn redact_url_password(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let rest = &url[authority_start..];
    // The authority ends at the first `/`, `?` or `#` (RFC 3986); an `@` in the path or query isn't
    // part of the credentials.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some(at) = rest[..authority_end].find('@') else {
        return url.to_string();
    };
    let at = authority_start + at;
    let authority = &url[authority_start..at];
    let Some(colon) = authority.find(':') else {
        return url.to_string();
    };
    let password_start = authority_start + colon + 1;
    format!("{}<redacted>{}", &url[..password_start], &url[at..])
}

impl Config {
    /// Load configuration from the environment. Callers wanting `.env` support must load it first
    /// (binary `main()`s do), so tests constructing a `Config` don't pick up the repo-root `.env`.
    pub fn from_env() -> Result<Self> {
        let app_host = env_or("APP_HOST", "0.0.0.0")
            .parse::<IpAddr>()
            .context("APP_HOST must be a valid IP address")?;

        let app_port = env_or("APP_PORT", "8080")
            .parse::<u16>()
            .context("APP_PORT must be a valid u16 port number")?;

        let database_url =
            env::var("DATABASE_URL").context("DATABASE_URL is required but was not set")?;

        // REDIS_URL is accepted as an alias for VALKEY_URL
        let valkey_url = env::var("VALKEY_URL")
            .or_else(|_| env::var("REDIS_URL"))
            .context("VALKEY_URL (or legacy REDIS_URL) is required but was not set")?;

        let encryption_key =
            env::var("ENCRYPTION_KEY").context("ENCRYPTION_KEY is required but was not set")?;
        if encryption_key.len() != 64 || !encryption_key.bytes().all(|b| b.is_ascii_hexdigit()) {
            anyhow::bail!("ENCRYPTION_KEY must be exactly 64 hex characters (32 bytes)");
        }

        let cookie_secret =
            env::var("COOKIE_SECRET").context("COOKIE_SECRET is required but was not set")?;
        if cookie_secret.is_empty() {
            anyhow::bail!("COOKIE_SECRET must not be empty");
        }

        let disable_public_urls = parse_bool_env("DISABLE_PUBLIC_URLS", false)?;
        let worker_queues = parse_worker_queues(env::var("WORKER_QUEUES").ok())?;
        let fast_concurrency = parse_positive_env::<usize>("FAST_CONCURRENCY", 64)?;
        let long_concurrency = parse_positive_env::<usize>("LONG_CONCURRENCY", 8)?;
        let long_drain_secs = parse_positive_env::<u64>("LONG_DRAIN_SECS", 180)?;

        let smtp_port = env_or("SMTP_PORT", "587")
            .parse::<u16>()
            .context("SMTP_PORT must be a valid u16 port number")?;
        let smtp_tls = env_or("SMTP_TLS", "wrapper");
        if !matches!(smtp_tls.as_str(), "wrapper" | "starttls" | "none") {
            anyhow::bail!(
                "SMTP_TLS must be one of \"wrapper\", \"starttls\", \"none\", got {smtp_tls:?}"
            );
        }

        let trusted_proxy_hops = parse_env("TRUSTED_PROXY_HOPS", 0, "non-negative integer")?;

        Ok(Self {
            app_host,
            app_port,
            database_url,
            valkey_url,
            rust_log: env_or("RUST_LOG", "info"),
            base_url: env_or("BASE_URL", "http://localhost:3000"),
            encryption_key,
            cookie_secret,
            disable_public_urls,
            worker_queues,
            fast_concurrency,
            long_concurrency,
            long_drain_secs,
            smtp_host: env::var("SMTP_HOST").ok(),
            smtp_port,
            smtp_username: env::var("SMTP_USERNAME").ok(),
            smtp_password: env::var("SMTP_PASSWORD").ok(),
            mail_from: env_or("SMTP_FROM", "arcodash@localhost"),
            smtp_tls,
            remote_user_header: env::var("REMOTE_USER_HEADER").ok(),
            saml_idp_metadata_xml: env::var("SAML_IDP_METADATA_XML").ok(),
            saml_sp_entity_id: env::var("SAML_SP_ENTITY_ID").ok(),
            ldap_url: env::var("LDAP_URL").ok(),
            ldap_bind_dn: env::var("LDAP_BIND_DN").ok(),
            ldap_bind_password: env::var("LDAP_BIND_PASSWORD").ok(),
            ldap_search_base_dn: env::var("LDAP_SEARCH_BASE_DN").ok(),
            ldap_user_filter: env_or("LDAP_USER_FILTER", "(uid={username})"),
            ldap_email_attribute: env_or("LDAP_EMAIL_ATTRIBUTE", "mail"),
            ldap_first_name_attribute: env_or("LDAP_FIRST_NAME_ATTRIBUTE", "givenName"),
            ldap_last_name_attribute: env_or("LDAP_LAST_NAME_ATTRIBUTE", "sn"),
            ldap_group_attribute: env::var("LDAP_GROUP_ATTRIBUTE").ok(),
            gemini_api_key: env::var("GEMINI_API_KEY").ok().filter(|k| !k.is_empty()),
            openai_api_key: env::var("OPENAI_API_KEY").ok().filter(|k| !k.is_empty()),
            anthropic_api_key: env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()),
            ai_rate_limit_per_min: env_parsed("AI_RATE_LIMIT_PER_MIN", 10),
            gemini_model: env_or("GEMINI_MODEL", "gemini-3.8-flash"),
            openai_model: env_or("OPENAI_MODEL", "gpt-5.6-terra"),
            anthropic_model: env_or("ANTHROPIC_MODEL", "claude-sonnet-5-5"),
            query_timeout_secs: env_parsed("QUERY_TIMEOUT_SECS", 300),
            max_query_rows: env_parsed("MAX_QUERY_ROWS", 10_000),
            result_retention_days: result_retention_days(),
            trusted_proxy_hops,
        })
    }
}

/// Parse a boolean env var (`true`/`false`, case-insensitive, also `1`/`0`), falling back to
/// `default` when unset.
///
/// Errors on a present-but-unparseable value so deploy-config typos surface. `pub`: also behind
/// every `CONVEYOR_*_ENABLED` flag reader, which runs outside `Config::from_env`, treats an `Err`
/// as `false` and logs an `error!`.
pub fn parse_bool_env(name: &str, default: bool) -> Result<bool> {
    env::var(name).map_or(Ok(default), |raw| {
        match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            other => Err(anyhow::anyhow!(
                "{name} must be a boolean (true/false/1/0), got {other:?}"
            )),
        }
    })
}

/// `name` unset → `default`.
fn env_or(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Parse an integer env var at startup, hard-erroring on a present-but-unparseable value; for
/// settings like `TRUSTED_PROXY_HOPS` where accepting garbage would be a security bug.
fn parse_env<T>(name: &str, default: T, what: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    env::var(name).map_or(Ok(default), |raw| {
        raw.trim()
            .parse::<T>()
            .with_context(|| format!("{name} must be a {what}, got {raw:?}"))
    })
}

/// Parse a knob that must be `>= 1`; a `0` is a misconfiguration (consumers floor it to 1), and
/// accepting it silently would run a tier the operator meant to disable.
fn parse_positive_env<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr + PartialEq + From<u8>,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let parsed = parse_env(name, default, "positive integer")?;
    if parsed == T::from(0) {
        anyhow::bail!("{name} must be a positive integer, got 0");
    }
    Ok(parsed)
}

/// Names already reported malformed, so a bad hot-path knob logs once, not per database connection.
static REPORTED_INVALID: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

/// Whether this is the first time `name` has been reported invalid in this process.
fn first_time_reporting_invalid(name: &str) -> bool {
    REPORTED_INVALID.lock().unwrap().insert(name.to_string())
}

/// `RESULT_RETENTION_DAYS` (default 7), floored at 1 so a zero or negative value cannot expire
/// results immediately.
pub fn result_retention_days() -> i64 {
    env_parsed("RESULT_RETENTION_DAYS", 7_i64).max(1)
}

/// Parse an operator knob read outside `Config::from_env` (a hot-path lookup). A malformed value
/// falls back to `default` but logs once per variable at `error!`, naming the variable and raw
/// value.
pub fn env_parsed<T: std::str::FromStr>(name: &str, default: T) -> T
where
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Err(_) => default,
        Ok(raw) => match raw.trim().parse() {
            Ok(v) => v,
            Err(e) => {
                if first_time_reporting_invalid(name) {
                    tracing::error!(name, raw, error = %e, "invalid env var value, using default");
                }
                default
            }
        },
    }
}

mod fingerprint;
pub use fingerprint::{
    cross_binary_fingerprint, publish_and_check_cross_binary_fingerprint,
    spawn_cross_binary_fingerprint_check,
};

/// Read a `CONVEYOR_*_ENABLED` routing flag.
///
/// Malformed values are treated as off: a task type must never reroute because someone typed `yes`.
/// Behind every `conveyor_*_enabled` wrapper; an `Err` from `parse_bool_env` becomes `false` plus
/// an `error!`.
#[must_use]
pub fn conveyor_flag(name: &str) -> bool {
    parse_bool_env(name, false).unwrap_or_else(|e| {
        tracing::error!("{e}");
        false
    })
}

/// Floor for `MONITOR_TICK_SECS`, guarding only against a 0/negative value becoming a busy loop.
///
/// Deliberately not the conveyor's 60s `EVERY_SECS_FLOOR` (a schedule-cadence constraint);
/// escalation ticks need to go as low as a few seconds for e2e tests.
pub const MONITOR_TICK_SECS_MIN: u64 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    fn set(name: &str, value: &str) {
        unsafe { env::set_var(name, value) }
    }

    fn unset(name: &str) {
        unsafe { env::remove_var(name) }
    }

    fn parse_u32_env(name: &str, default: u32) -> Result<u32> {
        parse_env(name, default, "non-negative integer")
    }

    #[test]
    fn parse_bool_env_defaults_when_unset() {
        assert!(env::var("Q_TEST_BOOL_UNSET").is_err());
        assert!(!parse_bool_env("Q_TEST_BOOL_UNSET", false).unwrap());
        assert!(parse_bool_env("Q_TEST_BOOL_UNSET", true).unwrap());
    }

    #[test]
    fn parse_bool_env_parses_common_truthy_falsy_values() {
        for (raw, expected) in [
            ("true", true),
            ("TRUE", true),
            ("1", true),
            ("false", false),
            ("FALSE", false),
            ("0", false),
        ] {
            set("Q_TEST_BOOL_PARSE", raw);
            assert_eq!(
                parse_bool_env("Q_TEST_BOOL_PARSE", false).unwrap(),
                expected
            );
        }
        unset("Q_TEST_BOOL_PARSE");
    }

    #[test]
    fn parse_bool_env_rejects_garbage() {
        set("Q_TEST_BOOL_GARBAGE", "maybe");
        assert!(parse_bool_env("Q_TEST_BOOL_GARBAGE", false).is_err());
        unset("Q_TEST_BOOL_GARBAGE");
    }

    /// Table covering every shape `parse_bool_env` must handle: upper-case, whitespace, mixed-case
    /// falsy, numeric falsy alias, garbage.
    #[test]
    fn parse_bool_env_handles_case_whitespace_and_garbage() {
        for (raw, expected) in [("TRUE", true), (" true ", true), ("False", false)] {
            set("Q_TEST_BOOL_TABLE", raw);
            assert_eq!(
                parse_bool_env("Q_TEST_BOOL_TABLE", !expected).unwrap(),
                expected
            );
        }
        set("Q_TEST_BOOL_TABLE", "0");
        assert!(!parse_bool_env("Q_TEST_BOOL_TABLE", true).unwrap());
        set("Q_TEST_BOOL_TABLE", "garbage");
        assert!(parse_bool_env("Q_TEST_BOOL_TABLE", false).is_err());
        unset("Q_TEST_BOOL_TABLE");
    }

    #[test]
    fn parse_worker_queues_defaults_rejects_empty_and_unknown() {
        assert_eq!(parse_worker_queues(None).unwrap(), QueueName::ALL.to_vec());
        assert_eq!(
            parse_worker_queues(Some(" scheduled_queries ,".into())).unwrap(),
            vec![QueueName::parse("scheduled_queries").unwrap()]
        );
        assert!(parse_worker_queues(Some("".into())).is_err());
        assert!(parse_worker_queues(Some(" , ".into())).is_err());
        assert!(parse_worker_queues(Some("nope".into())).is_err());
    }

    #[test]
    fn result_retention_days_defaults_overrides_and_floors() {
        unset("RESULT_RETENTION_DAYS");
        assert_eq!(result_retention_days(), 7);
        set("RESULT_RETENTION_DAYS", "30");
        assert_eq!(result_retention_days(), 30);
        set("RESULT_RETENTION_DAYS", "0");
        assert_eq!(result_retention_days(), 1);
        set("RESULT_RETENTION_DAYS", "-5");
        assert_eq!(result_retention_days(), 1);
        unset("RESULT_RETENTION_DAYS");
    }

    #[test]
    fn env_parsed_returns_default_when_unset() {
        unset("Q_TEST_ENV_PARSED_UNSET");
        assert_eq!(env_parsed::<u32>("Q_TEST_ENV_PARSED_UNSET", 7), 7);
    }

    #[test]
    fn env_parsed_parses_a_valid_value() {
        set("Q_TEST_ENV_PARSED_OK", "42");
        assert_eq!(env_parsed::<u32>("Q_TEST_ENV_PARSED_OK", 0), 42);
        unset("Q_TEST_ENV_PARSED_OK");
    }

    #[test]
    fn env_parsed_falls_back_to_default_on_garbage_instead_of_panicking() {
        set("Q_TEST_ENV_PARSED_GARBAGE", "not-a-number");
        assert_eq!(env_parsed::<u32>("Q_TEST_ENV_PARSED_GARBAGE", 9), 9);
        unset("Q_TEST_ENV_PARSED_GARBAGE");
    }

    /// Pins `conveyor_flag`'s own truthy/falsy/garbage handling. The real `CONVEYOR_*_ENABLED`
    /// names are not reused here: each owning module pins its own wrapper and reuse would race
    /// under parallel `cargo test`.
    #[test]
    fn conveyor_flag_matches_parse_bool_env_semantics() {
        for (raw, expected) in [("true", true), ("1", true), ("false", false), ("0", false)] {
            set("Q_TEST_CONVEYOR_FLAG", raw);
            assert_eq!(conveyor_flag("Q_TEST_CONVEYOR_FLAG"), expected);
        }
        unset("Q_TEST_CONVEYOR_FLAG");
        assert!(!conveyor_flag("Q_TEST_CONVEYOR_FLAG_UNSET"));

        set("Q_TEST_CONVEYOR_FLAG_GARBAGE", "maybe");
        assert!(
            !conveyor_flag("Q_TEST_CONVEYOR_FLAG_GARBAGE"),
            "a malformed value must fall back to off, not propagate the parse error"
        );
        unset("Q_TEST_CONVEYOR_FLAG_GARBAGE");
    }

    #[test]
    fn first_time_reporting_invalid_is_true_once_then_false_for_the_same_name() {
        let name = "Q_TEST_REPORT_DEDUPE_SAME_NAME";
        assert!(first_time_reporting_invalid(name));
        assert!(!first_time_reporting_invalid(name));
        assert!(!first_time_reporting_invalid(name));
    }

    #[test]
    fn first_time_reporting_invalid_tracks_each_name_independently() {
        assert!(first_time_reporting_invalid("Q_TEST_REPORT_DEDUPE_A"));
        assert!(first_time_reporting_invalid("Q_TEST_REPORT_DEDUPE_B"));
        assert!(!first_time_reporting_invalid("Q_TEST_REPORT_DEDUPE_A"));
    }

    #[test]
    fn parse_u32_env_defaults_when_unset() {
        assert!(env::var("Q_TEST_U32_UNSET").is_err());
        assert_eq!(parse_u32_env("Q_TEST_U32_UNSET", 0).unwrap(), 0);
        assert_eq!(parse_u32_env("Q_TEST_U32_UNSET", 3).unwrap(), 3);
    }

    #[test]
    fn parse_u32_env_parses_zero_and_a_positive_integer() {
        set("Q_TEST_U32_PARSE", "0");
        assert_eq!(parse_u32_env("Q_TEST_U32_PARSE", 9).unwrap(), 0);

        set("Q_TEST_U32_PARSE", "2");
        assert_eq!(parse_u32_env("Q_TEST_U32_PARSE", 9).unwrap(), 2);

        unset("Q_TEST_U32_PARSE");
    }

    #[test]
    fn parse_u32_env_rejects_garbage_and_negative_values() {
        for raw in ["not-a-number", "-1", "1.5"] {
            set("Q_TEST_U32_GARBAGE", raw);
            assert!(
                parse_u32_env("Q_TEST_U32_GARBAGE", 0).is_err(),
                "expected {raw:?} to be rejected"
            );
        }
        unset("Q_TEST_U32_GARBAGE");
    }

    #[test]
    fn parse_positive_usize_env_defaults_when_unset() {
        assert!(env::var("Q_TEST_POS_USIZE_UNSET").is_err());
        assert_eq!(
            parse_positive_env::<usize>("Q_TEST_POS_USIZE_UNSET", 64).unwrap(),
            64
        );
    }

    #[test]
    fn parse_positive_usize_env_accepts_one() {
        set("Q_TEST_POS_USIZE_ONE", "1");
        assert_eq!(
            parse_positive_env::<usize>("Q_TEST_POS_USIZE_ONE", 64).unwrap(),
            1
        );
        unset("Q_TEST_POS_USIZE_ONE");
    }

    #[test]
    fn parse_positive_usize_env_rejects_zero() {
        set("Q_TEST_POS_USIZE_ZERO", "0");
        assert!(parse_positive_env::<usize>("Q_TEST_POS_USIZE_ZERO", 64).is_err());
        unset("Q_TEST_POS_USIZE_ZERO");
    }

    #[test]
    fn parse_positive_usize_env_rejects_garbage_and_negatives() {
        for raw in ["not-a-number", "-1", "1.5"] {
            set("Q_TEST_POS_USIZE_GARBAGE", raw);
            assert!(
                parse_positive_env::<usize>("Q_TEST_POS_USIZE_GARBAGE", 64).is_err(),
                "expected {raw:?} to be rejected"
            );
        }
        unset("Q_TEST_POS_USIZE_GARBAGE");
    }

    #[test]
    fn parse_positive_u64_env_rejects_zero_and_defaults_when_unset() {
        assert_eq!(
            parse_positive_env::<u64>("Q_TEST_POS_U64_UNSET", 180).unwrap(),
            180
        );
        set("Q_TEST_POS_U64_ZERO", "0");
        assert!(parse_positive_env::<u64>("Q_TEST_POS_U64_ZERO", 180).is_err());
        unset("Q_TEST_POS_U64_ZERO");
    }

    /// Pins the exact error text for `0` or an unparseable value, byte-for-byte.
    #[test]
    fn parse_positive_env_error_messages_are_pinned() {
        set("Q_TEST_POS_USIZE_MSG_ZERO", "0");
        assert_eq!(
            parse_positive_env::<usize>("Q_TEST_POS_USIZE_MSG_ZERO", 64)
                .unwrap_err()
                .to_string(),
            "Q_TEST_POS_USIZE_MSG_ZERO must be a positive integer, got 0"
        );
        set("Q_TEST_POS_USIZE_MSG_GARBAGE", "abc");
        assert_eq!(
            parse_positive_env::<usize>("Q_TEST_POS_USIZE_MSG_GARBAGE", 64)
                .unwrap_err()
                .to_string(),
            "Q_TEST_POS_USIZE_MSG_GARBAGE must be a positive integer, got \"abc\""
        );
        set("Q_TEST_POS_U64_MSG_ZERO", "0");
        assert_eq!(
            parse_positive_env::<u64>("Q_TEST_POS_U64_MSG_ZERO", 180)
                .unwrap_err()
                .to_string(),
            "Q_TEST_POS_U64_MSG_ZERO must be a positive integer, got 0"
        );
        set("Q_TEST_POS_U64_MSG_GARBAGE", "abc");
        assert_eq!(
            parse_positive_env::<u64>("Q_TEST_POS_U64_MSG_GARBAGE", 180)
                .unwrap_err()
                .to_string(),
            "Q_TEST_POS_U64_MSG_GARBAGE must be a positive integer, got \"abc\""
        );
        for name in [
            "Q_TEST_POS_USIZE_MSG_ZERO",
            "Q_TEST_POS_USIZE_MSG_GARBAGE",
            "Q_TEST_POS_U64_MSG_ZERO",
            "Q_TEST_POS_U64_MSG_GARBAGE",
        ] {
            unset(name);
        }
    }

    fn config_with_known_secrets() -> Config {
        Config {
            app_host: "0.0.0.0".parse().unwrap(),
            app_port: 8080,
            database_url: "postgres://appuser:db-secret-pw@dbhost/appdb".to_string(),
            valkey_url: "redis://valkeyhost:6379".to_string(),
            rust_log: "info".to_string(),
            base_url: "http://localhost:3000".to_string(),
            encryption_key: "encryption-key-secret".to_string(),
            cookie_secret: "cookie-secret-value".to_string(),
            disable_public_urls: false,
            worker_queues: QueueName::ALL.to_vec(),
            fast_concurrency: 64,
            long_concurrency: 8,
            long_drain_secs: 180,
            smtp_host: None,
            smtp_port: 587,
            smtp_username: None,
            smtp_password: Some("smtp-secret-pw".to_string()),
            mail_from: "arcodash@localhost".to_string(),
            smtp_tls: "wrapper".to_string(),
            remote_user_header: None,
            saml_idp_metadata_xml: None,
            saml_sp_entity_id: None,
            ldap_url: None,
            ldap_bind_dn: None,
            ldap_bind_password: Some("ldap-secret-pw".to_string()),
            ldap_search_base_dn: None,
            ldap_user_filter: "(uid={username})".to_string(),
            ldap_email_attribute: "mail".to_string(),
            ldap_first_name_attribute: "givenName".to_string(),
            ldap_last_name_attribute: "sn".to_string(),
            ldap_group_attribute: None,
            gemini_api_key: Some("gemini-secret-key".to_string()),
            openai_api_key: Some("openai-secret-key".to_string()),
            anthropic_api_key: Some("anthropic-secret-key".to_string()),
            ai_rate_limit_per_min: 10,
            gemini_model: "gemini-3.8-flash".to_string(),
            openai_model: "gpt-5.6-terra".to_string(),
            anthropic_model: "claude-sonnet-5-5".to_string(),
            query_timeout_secs: 300,
            max_query_rows: 10_000,
            result_retention_days: 7,
            trusted_proxy_hops: 0,
        }
    }

    #[test]
    fn debug_output_redacts_every_secret_field() {
        let config = config_with_known_secrets();
        let out = format!("{config:?}");
        for secret in [
            "db-secret-pw",
            "encryption-key-secret",
            "cookie-secret-value",
            "smtp-secret-pw",
            "ldap-secret-pw",
            "gemini-secret-key",
            "openai-secret-key",
            "anthropic-secret-key",
        ] {
            assert!(
                !out.contains(secret),
                "debug output leaked secret value {secret:?}: {out}"
            );
        }
    }

    #[test]
    fn debug_output_still_shows_non_secret_fields() {
        let config = config_with_known_secrets();
        let out = format!("{config:?}");
        assert!(out.contains("8080"), "app_port must not be redacted: {out}");
        assert!(
            out.contains("appuser"),
            "database_url's non-password parts must survive: {out}"
        );
    }

    #[test]
    fn redact_url_password_masks_only_the_password() {
        assert_eq!(
            redact_url_password("postgres://u:pw@h/db"),
            "postgres://u:<redacted>@h/db"
        );
    }

    #[test]
    fn redact_url_password_leaves_credential_free_url_unchanged() {
        assert_eq!(
            redact_url_password("redis://valkeyhost:6379"),
            "redis://valkeyhost:6379"
        );
    }

    #[test]
    fn redact_url_password_leaves_a_query_string_at_sign_alone() {
        assert_eq!(
            redact_url_password("redis://valkeyhost:6379/0?sentinel=a:b@c"),
            "redis://valkeyhost:6379/0?sentinel=a:b@c"
        );
    }

    #[test]
    fn redact_url_password_still_masks_a_password_before_a_path() {
        assert_eq!(
            redact_url_password("postgres://u:pw@h/db?x=a@b"),
            "postgres://u:<redacted>@h/db?x=a@b"
        );
    }
}
