use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use arcodash_conveyor::{TaskContext, TaskOutcome, TaskResult, TypedTaskHandler};
use arcodash_core::chat_body::{
    chat_alert_body, chat_body, default_heading, default_intro, default_subject, state_label,
    ChatAlert,
};
use arcodash_core::email_template::{render, EmailLayout};
use arcodash_core::entities::{
    event, monitor, notification_delivery, notification_destination, query, query_result, user,
};
use arcodash_core::events::record_event;
use arcodash_core::models::MonitorOptions;
use arcodash_core::net_guard;
use arcodash_core::notify::{
    enqueue_send_notification, SendNotificationParams, SendNotificationPayload,
};
use arcodash_core::secrets;

use crate::delivery_lifecycle;
use crate::handlers::evaluate_monitor::resolve_monitor_result;
use crate::handlers::execute_query::{send_html_email, ExecuteQueryContext};

/// Bounded retry: a transient failure with `attempt < 3` is retried once after a fixed backoff;
/// attempt 3 or any non-transient error is terminal (`attempt` is 1-based).
const MAX_ATTEMPTS: u32 = 3;

/// Backoff before re-enqueuing attempt N+1 (1→30s, 2→120s). `NOTIFICATION_RETRY_BACKOFF_SECS_BASE`
/// overrides the attempt-1 backoff (attempt 2 scales 4x) so e2e tests needn't sleep; unset in
/// production.
fn retry_backoff_secs(attempt: u32) -> u64 {
    let base = arcodash_core::config::env_parsed("NOTIFICATION_RETRY_BACKOFF_SECS_BASE", 30);
    match attempt {
        1 => base,
        _ => base * 4,
    }
}

/// SSRF guard for user-supplied webhook destination URLs. Respects
/// `ARCODASH_ALLOW_PRIVATE_CONNECTORS` (self-hosted receivers on private hosts are legitimate).
/// Returns the guarded host and its resolved addresses so [`webhook_client`] can pin the
/// connection, closing the DNS-rebind gap before connect.
async fn guard_webhook_url(raw_url: &str) -> anyhow::Result<(String, Vec<std::net::SocketAddr>)> {
    let parsed = reqwest::Url::parse(raw_url).map_err(|e| anyhow::anyhow!("invalid URL: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("webhook URL has no host"))?
        .to_string();
    let addrs = net_guard::guard_host(&host).await?;
    Ok((host, addrs))
}

/// Connect/total timeout bounds shared by every outbound destination client. `sweeper.rs`'s
/// `STALE_RETRYING_SECS` cites the 30s request timeout; keep them in sync.
const SEND_CONNECT_TIMEOUT_SECS: u64 = 10;
const SEND_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Bounded timeouts make a blackholed destination fail fast. Used as-is for PagerDuty/Datadog,
/// whose API URLs are fixed.
fn api_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(SEND_CONNECT_TIMEOUT_SECS))
        .timeout(std::time::Duration::from_secs(SEND_REQUEST_TIMEOUT_SECS))
}

/// HTTP client for outbound webhook POSTs: redirects disabled so a 30x can't bounce the request
/// into an address `guard_webhook_url` didn't approve. Pinned to `addrs` (empty when the guard is
/// bypassed) so reqwest can't re-resolve `host` at connect time.
fn webhook_client(host: &str, addrs: &[std::net::SocketAddr]) -> reqwest::Result<reqwest::Client> {
    let builder = api_client_builder().redirect(reqwest::redirect::Policy::none());
    if addrs.is_empty() {
        builder.build()
    } else {
        builder.resolve_to_addrs(host, addrs).build()
    }
}

/// Redaction (security-critical): a `reqwest::Error`'s `Display` embeds the request URL, which for
/// Slack/Discord/Mattermost/Teams webhooks can itself be the secret. `.without_url()` strips it
/// before the text is stored in the user-visible `notification_deliveries.error`; applied to every
/// reqwest error in this file.
fn redact_reqwest_error(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

#[allow(clippy::too_many_arguments)]
pub async fn handle(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    monitor_id: i64,
    destination_id: i64,
    new_state: String,
    subscriber_id: i64,
    subscription_id: Option<i64>,
    attempt: u32,
    delivery_id: Option<i64>,
    observed: Option<String>,
    explanation: Option<String>,
) {
    let job = SendNotificationParams {
        org_id,
        monitor_id,
        destination_id,
        new_state,
        subscriber_id,
        subscription_id,
        attempt,
        delivery_id,
        observed,
        explanation,
    };
    if let Err(e) = handle_inner(ctx, job).await {
        tracing::error!(monitor_id, destination_id, error = %e, "send_notification failed");
    }
}

/// One send attempt's outcome, written to `notification_deliveries` for every attempt that reaches
/// a destination lookup (the "monitor not found" early return writes no row). `retryable` marks a
/// transient network/connection failure (eligible for the bounded retry in `handle_inner`) versus a
/// config failure (missing key, SSRF-blocked URL, decrypt failure, unknown type); meaningless when
/// `status == "sent"`.
struct Outcome {
    status: &'static str,
    error: Option<String>,
    retryable: bool,
}

impl Outcome {
    const fn sent() -> Self {
        Self {
            status: "sent",
            error: None,
            retryable: false,
        }
    }

    /// A network/connection-level failure — worth retrying.
    fn failed_transient(error: impl Into<String>) -> Self {
        Self {
            status: "failed",
            error: Some(error.into()),
            retryable: true,
        }
    }

    /// A config/validation failure; retrying won't help, so always terminal.
    fn failed_permanent(error: impl Into<String>) -> Self {
        Self {
            status: "failed",
            error: Some(error.into()),
            retryable: false,
        }
    }

    /// `SMTP_HOST` is unset: nothing was sent, but it isn't a retryable failure either; same
    /// terminal shape as the org-blocked skip.
    fn skipped(error: impl Into<String>) -> Self {
        Self {
            status: "skipped",
            error: Some(error.into()),
            retryable: false,
        }
    }
}

/// Audit-log signal for a delivery's terminal failure. Best-effort: errors are logged, never
/// propagated, so they can't turn a recorded outcome into an internal error.
#[allow(clippy::too_many_arguments)]
async fn record_delivery_failed_event(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    monitor_id: i64,
    destination_id: i64,
    destination_name: &str,
    state: &str,
    error: Option<&str>,
    attempt: i32,
) {
    if let Err(e) = record_event(
        db,
        org_id,
        None,
        "notification_delivery_failed",
        Some("monitor"),
        Some(monitor_id),
        serde_json::json!({
            "destination_id": (destination_id != -1).then_some(destination_id),
            "destination_name": destination_name,
            "state": state,
            "error": error,
            "attempt": attempt,
        }),
    )
    .await
    {
        tracing::error!(monitor_id, error = %e, "send_notification: failed to record delivery-failed audit event");
    }
}

/// Finalizes a claimed delivery row that ends before any send is attempted (and stops its
/// heartbeat first); a no-op on the unclaimed fallback path.
#[allow(
    clippy::cast_possible_wrap,
    reason = "attempt numbers are small retry counts"
)]
async fn finalize_unsent(
    ctx: &ExecuteQueryContext,
    claim: Option<(i64, arcodash_conveyor::HeartbeatGuard)>,
    status: &str,
    error: &str,
    attempt: u32,
) -> anyhow::Result<()> {
    let Some((id, heartbeat)) = claim else {
        return Ok(());
    };
    drop(heartbeat);
    delivery_lifecycle::finalize(
        &ctx.db,
        delivery_lifecycle::NOTIFICATION_DELIVERIES,
        id,
        status,
        Some(error.to_string()),
        Some(attempt as i32),
        true,
    )
    .await
}

/// Returns whether the notify path was reached: `false` when it returned before attempting a send
/// (org blocked, monitor not found, subscriber lost access). The delivery outcome itself is the
/// `notification_deliveries` row.
#[allow(
    clippy::cast_possible_wrap,
    reason = "attempt numbers are small retry counts"
)]
async fn handle_inner(
    ctx: &ExecuteQueryContext,
    job: SendNotificationParams,
) -> anyhow::Result<bool> {
    let SendNotificationParams {
        org_id,
        monitor_id,
        destination_id,
        new_state,
        subscriber_id,
        subscription_id,
        attempt,
        delivery_id,
        observed,
        explanation,
    } = job;

    // Durability: `delivery_id` (when `Some`) is the pre-created `notification_deliveries` row's
    // id, not the evaluation run id. Claim it so the outcome finalizes that same row, closing the
    // "LPUSHed but never popped" gap.
    let mut claim = None;
    if let Some(id) = delivery_id {
        if delivery_lifecycle::claim(&ctx.db, delivery_lifecycle::NOTIFICATION_DELIVERIES, id)
            .await?
        {
            let heartbeat = delivery_lifecycle::spawn_heartbeat(
                ctx.db.clone(),
                delivery_lifecycle::NOTIFICATION_DELIVERIES,
                id,
            );
            claim = Some((id, heartbeat));
        } else if delivery_lifecycle::row_exists(
            &ctx.db,
            delivery_lifecycle::NOTIFICATION_DELIVERIES,
            id,
        )
        .await?
        {
            // Row exists but isn't claimable: a duplicate delivery of the same message. Drop it, no
            // double-notify.
            return Ok(true);
        }
        // Otherwise the id isn't a delivery row at all (e.g. a `runs` id): fall through to the
        // fallback path (`claim` stays `None`).
    }
    // `delivery_id == None` also falls through to the insert-per-attempt fallback rather than
    // dropping the notification.
    let claimed_id = claim.as_ref().map(|(id, _)| *id);

    // An org blocked by its entitlements sends no outbound notifications. Checked here, not only at
    // the scheduling paths, because escalations enqueue a send directly. A claimed row is finalized
    // as skipped rather than left pending.
    if let arcodash_core::entitlements::WorkGate::Blocked { reason } =
        ctx.entitlements.scheduled_work(&ctx.db, org_id).await
    {
        tracing::info!(org_id, reason, "send_notification: skipping send");
        finalize_unsent(ctx, claim, "skipped", reason, attempt).await?;
        return Ok(false);
    }

    // Defense-in-depth: the worker connects as an RLS-exempt role, so re-check every id against the
    // job's `org_id`; a mismatch is treated as not found.
    let Some(monitor_model) = monitor::Entity::find_by_id(monitor_id)
        .filter(monitor::Column::OrgId.eq(org_id))
        .one(&ctx.db)
        .await?
    else {
        tracing::warn!(monitor_id, "send_notification: monitor not found");
        // A claimed row must not stay `running` forever because the monitor vanished.
        finalize_unsent(ctx, claim, "failed", "monitor not found", attempt).await?;
        crate::health::record_notification_outcome("failed");
        return Ok(false);
    };
    // A malformed `options` JSON blob must not block a send: fall back to default copy.
    let monitor_options =
        arcodash_core::models::parse_monitor_options(monitor_id, &monitor_model.options);

    // Loaded once for the -1 (email-to-subscriber) branch, whose own outcome handles a missing row.
    let subscriber = user::Entity::find_by_id(subscriber_id)
        .filter(user::Column::OrgId.eq(org_id))
        .one(&ctx.db)
        .await?;

    // A subscription that outlived the subscriber's access to the monitor must stop delivering
    // (nothing prunes `monitor_subscriptions` when grants change). Escalation sends
    // (`subscription_id: None`) are exempt. A missing or cross-org subscriber means no access,
    // except for `-1`, which reports "subscriber not found" itself.
    if subscription_id.is_some() {
        let denied = match &subscriber {
            Some(s) => !arcodash_core::authz::can_view_monitor(&ctx.db, s, monitor_id).await?,
            None => destination_id != -1,
        };
        if denied {
            tracing::info!(
                monitor_id,
                subscriber_id,
                "send_notification: subscriber no longer has access to this monitor, skipping"
            );
            let error = "subscriber no longer has access to this monitor";
            finalize_unsent(ctx, claim, "skipped", error, attempt).await?;
            return Ok(false);
        }
    }

    // Captured for the terminal-failure audit event; `None` falls back to "(deleted destination)".
    let mut destination_name: Option<String> = None;

    let outcome = if destination_id == -1 {
        // Subscription with no destination_id → email the subscriber directly.
        match &subscriber {
            Some(u) => {
                send_monitor_email(
                    ctx,
                    &u.email,
                    org_id,
                    &monitor_model,
                    &new_state,
                    &monitor_options,
                    observed.as_deref(),
                    explanation.as_deref(),
                )
                .await
            }
            None => Outcome::failed_permanent("subscriber not found"),
        }
    } else {
        match notification_destination::Entity::find_by_id(destination_id)
            .filter(notification_destination::Column::OrgId.eq(org_id))
            .one(&ctx.db)
            .await?
        {
            None => {
                tracing::warn!(destination_id, "send_notification: destination not found");
                Outcome::failed_permanent("destination not found")
            }
            Some(dest) => {
                destination_name = Some(dest.name.clone());
                // A decrypt/deserialize failure must not propagate via `?` before the delivery-row
                // insert (leaving no history for a real attempt); match instead so it becomes
                // Outcome::failed with a row. The error is redacted to a fixed safe message, since
                // it can embed ciphertext detail.
                match decrypt_options(&ctx.encryption_key, dest.options.as_deref()) {
                    Err(e) => {
                        tracing::warn!(
                            destination_id,
                            error = %e,
                            "send_notification: failed to decrypt destination options"
                        );
                        Outcome::failed_permanent("destination configuration error")
                    }
                    Ok(opts) if dest.r#type == "email" => {
                        // Email needs `ctx` (the real mailer), so it stays here rather than in the
                        // ctx-free `send_to_destination`.
                        let to = opts.get("to").and_then(|v| v.as_str()).map(str::trim);
                        if let Some(to) = to.filter(|to| !to.is_empty()) {
                            send_monitor_email(
                                ctx,
                                to,
                                org_id,
                                &monitor_model,
                                &new_state,
                                &monitor_options,
                                observed.as_deref(),
                                explanation.as_deref(),
                            )
                            .await
                        } else {
                            tracing::warn!(
                                "send_notification: missing 'to' address for email destination"
                            );
                            Outcome::failed_permanent("email destination is missing a 'to' address")
                        }
                    }
                    Ok(opts) => {
                        let is_chat = !matches!(dest.r#type.as_str(), "pagerduty" | "datadog");
                        let shows_observed = is_chat && dest.r#type != "webhook";
                        let chat_observed = match &observed {
                            None if shows_observed && new_state != "unevaluable" => {
                                Some(observed_value(ctx, &monitor_model, org_id).await)
                            }
                            other => other.clone(),
                        };
                        let alert = chat_alert(
                            &ctx.base_url,
                            &monitor_model,
                            &new_state,
                            &monitor_options,
                            chat_observed,
                            explanation.clone(),
                        );
                        send_to_destination(
                            monitor_id,
                            &monitor_model.name,
                            &new_state,
                            &dest.r#type,
                            &opts,
                            explanation.as_deref(),
                            &alert,
                        )
                        .await
                    }
                }
            }
        }
    };

    // Bounded retry: a transient failure with attempts remaining is recorded as "retrying" and
    // retried once after a backoff; anything else is terminal.
    let will_retry = outcome.status == "failed" && outcome.retryable && attempt < MAX_ATTEMPTS;
    let recorded_status = if will_retry {
        "retrying"
    } else {
        outcome.status
    };
    crate::health::record_notification_outcome(recorded_status);

    // `destination_id == -1` and failed-to-load destinations use the API's "Email (you)" /
    // "(deleted destination)" labels, so the audit event and run history agree.
    let destination_display_name = if destination_id == -1 {
        "Email (you)".to_string()
    } else {
        destination_name.unwrap_or_else(|| "(deleted destination)".to_string())
    };
    // Cloned before `outcome.error` moves into the delivery row.
    let event_error = outcome.error.clone();

    if let Some((id, heartbeat)) = claim {
        drop(heartbeat);
        if let Err(e) = delivery_lifecycle::finalize(
            &ctx.db,
            delivery_lifecycle::NOTIFICATION_DELIVERIES,
            id,
            recorded_status,
            outcome.error,
            Some(attempt as i32),
            !will_retry,
        )
        .await
        {
            // Failing to record the outcome is a worker-internal problem, not a delivery
            // failure: log it, don't error the send.
            tracing::error!(monitor_id, error = %e, "send_notification: failed to record delivery");
        }
    } else {
        let delivery = notification_delivery::ActiveModel {
            org_id: Set(org_id),
            monitor_id: Set(monitor_id),
            subscription_id: Set(subscription_id),
            destination_id: Set((destination_id != -1).then_some(destination_id)),
            state: Set(new_state.clone()),
            status: Set(recorded_status.to_string()),
            error: Set(outcome.error),
            attempt_number: Set(attempt as i32),
            // No evaluation run id is available on the fallback path.
            run_id: Set(None),
            ..Default::default()
        };
        if let Err(e) = delivery.insert(&ctx.db).await {
            tracing::error!(monitor_id, error = %e, "send_notification: failed to record delivery");
        }
    }

    // `failed` only happens when `will_retry` is false, so this never fires on an intermediate
    // "retrying" attempt.
    if recorded_status == "failed" {
        record_delivery_failed_event(
            &ctx.db,
            org_id,
            monitor_id,
            destination_id,
            &destination_display_name,
            &new_state,
            event_error.as_deref(),
            attempt as i32,
        )
        .await;
    }

    if will_retry {
        // Best-effort in-process delayed re-enqueue (tokio::spawn + sleep): a worker restart
        // mid-sleep loses the retry; upgrade to a Redis ZSET delay queue.
        let backoff_secs = retry_backoff_secs(attempt);
        let queue = ctx.queue.clone();
        let conveyor = ctx.conveyor.get().cloned();
        let db = ctx.db.clone();
        tokio::spawn(async move {
            const ERROR: &str = "failed to schedule retry";
            tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
            // Pass through the same delivery_id so the chain stays consistent (claim-in-place if
            // Some, fallback-insert if None).
            let retry = SendNotificationParams {
                org_id,
                monitor_id,
                destination_id,
                new_state: new_state.clone(),
                subscriber_id,
                subscription_id,
                attempt: attempt + 1,
                delivery_id,
                observed,
                explanation,
            };
            let Err(e) =
                enqueue_send_notification(queue.as_ref(), conveyor.as_deref(), retry).await
            else {
                return;
            };
            tracing::error!(monitor_id, error = %e, "send_notification: retry re-enqueue failed");
            // The queue is down, so no future attempt will run: record the terminal outcome
            // ourselves rather than leave the row at "retrying".
            let next_attempt = attempt + 1;
            let recorded = if let Some(id) = claimed_id {
                match delivery_lifecycle::fail_retrying(
                    &db,
                    delivery_lifecycle::NOTIFICATION_DELIVERIES,
                    id,
                    ERROR.to_string(),
                    next_attempt as i32,
                )
                .await
                {
                    Ok(true) => true,
                    Ok(false) => {
                        // Another party (the next attempt's claim or the stale-retrying sweep)
                        // already moved this row out of `retrying` and owns the outcome; a second
                        // terminal event would be wrong.
                        tracing::info!(
                        monitor_id,
                        delivery_id = id,
                        "send_notification: retry-scheduling failure raced a row already claimed elsewhere, not recording"
                    );
                        false
                    }
                    Err(e) => {
                        tracing::error!(monitor_id, error = %e, "send_notification: failed to record retry-scheduling failure");
                        false
                    }
                }
            } else {
                let failed = notification_delivery::ActiveModel {
                    org_id: Set(org_id),
                    monitor_id: Set(monitor_id),
                    subscription_id: Set(subscription_id),
                    destination_id: Set((destination_id != -1).then_some(destination_id)),
                    state: Set(new_state.clone()),
                    status: Set("failed".to_string()),
                    error: Set(Some(ERROR.to_string())),
                    attempt_number: Set(next_attempt as i32),
                    run_id: Set(None),
                    ..Default::default()
                };
                if let Err(e) = failed.insert(&db).await {
                    tracing::error!(monitor_id, error = %e, "send_notification: failed to record retry-scheduling failure");
                }
                true
            };
            if recorded {
                record_delivery_failed_event(
                    &db,
                    org_id,
                    monitor_id,
                    destination_id,
                    &destination_display_name,
                    &new_state,
                    Some(ERROR),
                    next_attempt as i32,
                )
                .await;
            }
        });
    }

    Ok(true)
}

/// Adapts `handle_inner` onto the conveyor SDK; the delivery-row id is the task's `entity_id`.
/// Reports `Success` for any run without an internal error: the task row tracks "did the worker
/// process this message", not whether the notification went out.
pub struct SendNotificationHandler {
    pub ctx: Arc<ExecuteQueryContext>,
}

#[async_trait]
impl TypedTaskHandler for SendNotificationHandler {
    type Payload = SendNotificationPayload;

    async fn run(&self, task: TaskContext, p: SendNotificationPayload) -> TaskResult {
        let job = SendNotificationParams {
            org_id: p.org_id,
            monitor_id: p.monitor_id,
            destination_id: p.destination_id,
            new_state: p.new_state,
            subscriber_id: p.subscriber_id,
            subscription_id: p.subscription_id,
            attempt: p.attempt,
            delivery_id: task.entity_id,
            observed: p.observed,
            explanation: p.explanation,
        };
        let result = handle_inner(&self.ctx, job).await;
        Ok(match result {
            Ok(_) => TaskOutcome::Success { result_ref: None },
            Err(e) => TaskOutcome::Failed {
                error: format!("{e:#}"),
            },
        })
    }
}

/// Classifies an HTTP response by status: 2xx is success; 408/429/5xx are transient and feed the
/// retry path; everything else is permanent. The message never includes the body or URL, either of
/// which can carry secrets.
fn classify_response(dest_type: &str, status: reqwest::StatusCode) -> Outcome {
    let code = status.as_u16();
    if status.is_success() {
        Outcome::sent()
    } else if code == 408 || code == 429 || status.is_server_error() {
        tracing::warn!(
            dest_type,
            status = code,
            "send_notification: destination responded with a transient-failure status"
        );
        Outcome::failed_transient(format!("destination responded with status {code}"))
    } else {
        tracing::warn!(
            dest_type,
            status = code,
            "send_notification: destination responded with a non-retryable failure status"
        );
        Outcome::failed_permanent(format!("destination responded with status {code}"))
    }
}

/// Sends `request` and classifies the response. A request-level failure (refused/timeout/DNS) is
/// transient; its error is redacted even where the secret travels in the body or a header rather
/// than the URL (`PagerDuty`'s key, Datadog's `DD-API-KEY`), for uniform redaction.
async fn post(dest_type: &str, request: reqwest::RequestBuilder) -> Outcome {
    match request.send().await {
        Ok(resp) => classify_response(dest_type, resp.status()),
        Err(e) => {
            let redacted = redact_reqwest_error(e);
            tracing::warn!(dest_type, error = %redacted, "send_notification: destination POST failed");
            Outcome::failed_transient(redacted)
        }
    }
}

/// Dispatches to the right destination-type branch; every branch yields `Outcome::sent()`,
/// `failed_transient(...)` (retried by `handle_inner`) or `failed_permanent(...)`, never a silent
/// success. Not used for `"email"`, which needs `ctx.mailer` and is handled in `handle_inner`.
async fn send_to_destination(
    monitor_id: i64,
    monitor_name: &str,
    new_state: &str,
    dest_type: &str,
    opts: &serde_json::Value,
    explanation: Option<&str>,
    alert: &ChatAlert,
) -> Outcome {
    match dest_type {
        t @ ("slack"
        | "mattermost"
        | "microsoft_teams_webhook"
        | "google_chat"
        | "discord"
        | "webhook") => {
            let Some(url) = opts.get("url").and_then(|v| v.as_str()) else {
                tracing::warn!(dest_type = t, "send_notification: missing url in options");
                return Outcome::failed_permanent("missing url in options");
            };
            let (host, addrs) = match guard_webhook_url(url).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(dest_type = t, error = %e, "send_notification: webhook URL blocked");
                    // `e` is our own net_guard message, never the destination's secret path.
                    return Outcome::failed_permanent(format!("webhook URL blocked: {e}"));
                }
            };
            let Ok(client) = webhook_client(&host, &addrs) else {
                return Outcome::failed_permanent("failed to build HTTP client");
            };
            let body = build_chat_body(t, url, opts, alert, explanation);
            post(t, client.post(url).json(&body)).await
        }
        "pagerduty" => {
            let Some(key) = opts.get("integration_key").and_then(|v| v.as_str()) else {
                tracing::warn!("send_notification: missing integration_key for pagerduty");
                return Outcome::failed_permanent("missing integration_key");
            };
            let event_action = if new_state == "triggered" {
                "trigger"
            } else {
                "resolve"
            };
            let body = pagerduty_body(
                key,
                event_action,
                monitor_id,
                monitor_name,
                new_state,
                explanation,
            );
            let Ok(client) = api_client_builder().build() else {
                return Outcome::failed_permanent("failed to build HTTP client");
            };
            let request = client
                .post("https://events.pagerduty.com/v2/enqueue")
                .json(&body);
            post("pagerduty", request).await
        }
        "datadog" => {
            let Some(api_key) = opts.get("api_key").and_then(|v| v.as_str()) else {
                tracing::warn!("send_notification: missing api_key for datadog");
                return Outcome::failed_permanent("missing api_key");
            };
            // NOTE: "site" holds an optional hostname hint (e.g. "datadoghq.eu"); default US.
            let site = opts.get("site").and_then(|v| v.as_str()).unwrap_or("");
            let api_url = if site.contains("eu") {
                "https://api.datadoghq.eu/api/v1/events"
            } else {
                "https://api.datadoghq.com/api/v1/events"
            };
            // NOTE: "alert_type" is Datadog's own Events API field (error/warning/info/success), an
            // external contract.
            let dd_alert_type = match new_state {
                "triggered" => "error",
                "ok" => "success",
                _ => "warning",
            };
            let body = datadog_body(
                dd_alert_type,
                monitor_id,
                monitor_name,
                new_state,
                explanation,
            );
            let Ok(client) = api_client_builder().build() else {
                return Outcome::failed_permanent("failed to build HTTP client");
            };
            let request = client
                .post(api_url)
                .header("DD-API-KEY", api_key)
                .json(&body);
            post("datadog", request).await
        }
        other => {
            tracing::warn!(
                dest_type = other,
                "send_notification: unknown destination type"
            );
            Outcome::failed_permanent(format!("unknown destination type: {other}"))
        }
    }
}

/// Fills a `ChatAlert` from the monitor row and payload; custom copy uses the same
/// `{{name}}`/`{{state}}` substitution as email.
fn chat_alert(
    base_url: &str,
    monitor: &monitor::Model,
    new_state: &str,
    options: &MonitorOptions,
    observed: Option<String>,
    explanation: Option<String>,
) -> ChatAlert {
    let state_word = state_label(new_state);
    ChatAlert {
        monitor_name: monitor.name.clone(),
        monitor_url: Some(format!("{base_url}/monitors/{}", monitor.id)),
        state: new_state.to_string(),
        observed,
        condition: describe_condition(options),
        reason: monitor.unevaluable_reason.clone(),
        explanation,
        custom_subject: render_custom_copy(
            options.custom_subject.as_deref(),
            &monitor.name,
            state_word,
        ),
        custom_body: render_custom_copy(options.custom_body.as_deref(), &monitor.name, state_word),
        timestamp: chrono::Utc::now(),
    }
}

/// Request body per chat destination. The generic webhook keeps its flat text payload;
/// Mattermost additionally honors the destination's `icon_emoji`/`username` overrides.
fn build_chat_body(
    dest_type: &str,
    url: &str,
    opts: &serde_json::Value,
    alert: &ChatAlert,
    explanation: Option<&str>,
) -> serde_json::Value {
    if dest_type == "webhook" {
        return chat_body(
            dest_type,
            &alert.monitor_name,
            &alert.state,
            explanation,
            url,
        );
    }
    let mut body = chat_alert_body(dest_type, url, alert);
    if dest_type == "mattermost" {
        if let Some(obj) = body.as_object_mut() {
            for key in ["icon_emoji", "username"] {
                if let Some(v) = opts.get(key).and_then(|v| v.as_str()) {
                    obj.insert(key.to_string(), serde_json::json!(v));
                }
            }
        }
    }
    body
}

fn pagerduty_body(
    key: &str,
    event_action: &str,
    monitor_id: i64,
    monitor_name: &str,
    new_state: &str,
    explanation: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "routing_key": key,
        "event_action": event_action,
        "dedup_key": format!("arcodash-monitor-{monitor_id}"),
        "payload": {
            "summary": format!("Monitor '{}' is {}", monitor_name, new_state),
            "severity": "error",
            "source": "arcodash"
        }
    });
    if let Some(e) = explanation {
        body["payload"]["custom_details"] = serde_json::json!({ "explanation": e });
    }
    body
}

fn datadog_body(
    alert_type: &str,
    monitor_id: i64,
    monitor_name: &str,
    new_state: &str,
    explanation: Option<&str>,
) -> serde_json::Value {
    let mut text = format!("arcodash monitor '{monitor_name}' transitioned to state: {new_state}.");
    if let Some(e) = explanation {
        text.push_str("\n\n");
        text.push_str(e);
    }
    serde_json::json!({
        "title": format!("Monitor '{}' is {}", monitor_name, new_state),
        "text": text,
        "alert_type": alert_type,
        "aggregation_key": format!("arcodash-monitor-{monitor_id}"),
        "tags": ["source:arcodash", format!("monitor_id:{monitor_id}")]
    })
}

/// Literal token substitution for `custom_subject`/`custom_body` (no templating crate); `None` when
/// `template` is unset so the caller uses the default.
fn render_custom_copy(
    template: Option<&str>,
    monitor_name: &str,
    new_state: &str,
) -> Option<String> {
    let template = template?;
    Some(
        template
            .replace("{{name}}", monitor_name)
            .replace("{{state}}", new_state),
    )
}

fn op_phrase(op: &str) -> &'static str {
    match op {
        ">" => "is greater than",
        ">=" => "is at least",
        "<" => "is less than",
        "<=" => "is at most",
        "==" => "equals",
        "!=" => "does not equal",
        _ => "compares to",
    }
}

/// Groups the integer part with commas (`12480.55` -> `12,480.55`), the email's equivalent of the
/// frontend's `formatNumeric`.
fn format_grouped(raw: &str) -> String {
    let raw = raw.trim();
    let negative = raw.starts_with('-');
    let unsigned = raw.strip_prefix('-').unwrap_or(raw);
    let (int_part, frac_part) = unsigned
        .split_once('.')
        .map_or((unsigned, None), |(i, f)| (i, Some(f)));
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return raw.to_string();
    }
    let mut out = String::from(if negative { "-" } else { "" });
    // ASCII digits only, so byte length == char count.
    for (i, c) in int_part.chars().enumerate() {
        if i > 0 && (int_part.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if let Some(f) = frac_part {
        out.push('.');
        out.push_str(f);
    }
    out
}

/// A JSON cell's display text: strings verbatim (NUMERIC/UInt64 arrive as strings for precision),
/// numbers via `Display`, anything else as JSON.
fn json_cell_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// `"{column} {phrase} {threshold}"`, e.g. `net_revenue is less than 20,000`, matching the
/// frontend's condition preview.
fn describe_condition(options: &MonitorOptions) -> String {
    match (
        options.column.as_deref(),
        options.op.as_deref(),
        options.value.as_ref(),
    ) {
        (Some(col), Some(op), Some(val)) => {
            format!(
                "{col} {} {}",
                op_phrase(op),
                format_grouped(&json_cell_to_string(val))
            )
        }
        _ => "No condition configured.".to_string(),
    }
}

/// A number rounded to 2 places and comma-grouped, for display.
pub fn format_number(n: f64) -> String {
    let s = format!("{n:.2}");
    format_grouped(s.trim_end_matches('0').trim_end_matches('.'))
}

/// A compared cell's display string, shared by `evaluate_state` and the send-time fallback so the
/// two can't format differently.
pub fn format_observed_cell(cell: &serde_json::Value) -> String {
    format!("{} (first row)", format_grouped(&json_cell_to_string(cell)))
}

/// Fallback only: re-resolves the monitor's current result at send time, for payloads that predate
/// the `observed` field (live producers capture the value at evaluation time, since re-resolving
/// can drift from what fired the alert). Returns `"no value"` when the result is absent and
/// `"(unavailable)"` on a DB error; never `null`/blank.
async fn observed_value(
    ctx: &ExecuteQueryContext,
    monitor: &monitor::Model,
    org_id: i64,
) -> String {
    let Some(column) = monitor.options.get("column").and_then(|v| v.as_str()) else {
        return "no value".to_string();
    };
    let qr_id = match resolve_monitor_result(&ctx.db, monitor, org_id).await {
        Ok(Some((qr_id, _))) => qr_id,
        Ok(None) => return "no value".to_string(),
        Err(e) => {
            tracing::warn!(
                monitor_id = monitor.id, error = %e,
                "send_notification: failed to resolve the monitor's result for the email body"
            );
            return "(unavailable)".to_string();
        }
    };
    let qr = match query_result::Entity::find_by_id(qr_id).one(&ctx.db).await {
        Ok(Some(qr)) => qr,
        Ok(None) => return "no value".to_string(),
        Err(e) => {
            tracing::warn!(
                monitor_id = monitor.id, error = %e,
                "send_notification: failed to load the monitor's query result for the email body"
            );
            return "(unavailable)".to_string();
        }
    };
    let cell = qr
        .data
        .as_ref()
        .and_then(|d| d.get("rows"))
        .and_then(|r| r.as_array())
        .and_then(|rows| rows.first())
        .and_then(|row| row.get(column));
    match cell {
        Some(v) if !v.is_null() => format_observed_cell(v),
        _ => "no value".to_string(),
    }
}

/// The state before the transition that produced this notification, read from the latest matching
/// `monitor_state_changed` audit event (no dedicated column or wire plumbing). A flap back to
/// `new_state` within this job's runtime could pick the wrong one, an accepted edge for best-effort
/// email context.
async fn previous_state_label(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    monitor_id: i64,
    new_state: &str,
) -> &'static str {
    let recent = event::Entity::find()
        .filter(event::Column::OrgId.eq(org_id))
        .filter(event::Column::ObjectType.eq("monitor"))
        .filter(event::Column::ObjectId.eq(monitor_id))
        .filter(event::Column::Action.eq(arcodash_core::events::MONITOR_STATE_CHANGED))
        .order_by_desc(event::Column::Id)
        .limit(10)
        .all(db)
        .await
        .unwrap_or_default();

    let old = recent.into_iter().find_map(|ev| {
        let is_this_transition = ev
            .additional_properties
            .get("new_state")
            .and_then(|v| v.as_str())
            == Some(new_state);
        is_this_transition.then(|| {
            ev.additional_properties
                .get("old_state")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string()
        })
    });
    state_label(old.as_deref().unwrap_or("unknown"))
}

/// Sends the monitor-state email and returns the outcome, so a real SMTP failure (e.g. TLS
/// handshake) is never recorded as sent; the caller uses it for `status`/`error`.
#[allow(clippy::too_many_arguments)]
async fn send_monitor_email(
    ctx: &ExecuteQueryContext,
    to: &str,
    org_id: i64,
    monitor: &monitor::Model,
    new_state: &str,
    options: &MonitorOptions,
    observed: Option<&str>,
    explanation: Option<&str>,
) -> Outcome {
    let monitor_name = &monitor.name;
    let state_word = state_label(new_state);

    let subject = render_custom_copy(options.custom_subject.as_deref(), monitor_name, state_word)
        .unwrap_or_else(|| default_subject(new_state, monitor_name));

    // A custom body replaces the heading+intro slot; the facts table, CTA and footer are never
    // user-configurable.
    let custom_heading =
        render_custom_copy(options.custom_body.as_deref(), monitor_name, state_word);
    let (heading, intro) = custom_heading.map_or_else(
        || {
            (
                default_heading(new_state, monitor_name),
                default_intro(new_state),
            )
        },
        |custom| (custom, ""),
    );

    // A DB error and a deleted query render as the same neutral fact: the email can't tell which
    // happened and a distinct string would leak failure detail into a customer-facing alert. The
    // log carries the distinction.
    let query_row = match query::Entity::find_by_id(monitor.query_id)
        .filter(query::Column::OrgId.eq(org_id))
        .one(&ctx.db)
        .await
    {
        Ok(row) => row,
        Err(e) => {
            tracing::warn!(
                monitor_id = monitor.id, error = %e,
                "send_notification: failed to load the monitor's query for the email body"
            );
            None
        }
    };
    let query_name = query_row
        .as_ref()
        .map_or_else(|| "(unavailable)".to_string(), |q| q.name.clone());

    let condition = describe_condition(options);
    let checked = monitor
        .updated_at
        .with_timezone(&chrono::Utc)
        .format("%b %-d, %Y, %-I:%M %p UTC")
        .to_string();
    let previous_state = previous_state_label(&ctx.db, org_id, monitor.id, new_state).await;

    let mut facts: Vec<(&str, String)> = vec![
        ("Monitor", monitor_name.clone()),
        ("Query", query_name),
        ("Condition", condition),
    ];
    if new_state == "unevaluable" {
        facts.push((
            "Reason",
            monitor
                .unevaluable_reason
                .clone()
                .unwrap_or_else(|| "No reason recorded.".to_string()),
        ));
    } else {
        // `observed` is the value captured at evaluation time; a payload without it falls back to
        // re-resolving the current result, which can drift.
        let observed = match observed {
            Some(v) => v.to_string(),
            None => observed_value(ctx, monitor, org_id).await,
        };
        facts.push(("Observed", observed));
    }
    facts.push(("Checked", checked));
    facts.push(("Previous state", previous_state.to_string()));

    let monitor_url = format!("{}/monitors/{}", ctx.base_url, monitor.id);
    let mut secondary: Vec<(&str, String)> = query_row
        .map(|q| {
            (
                "Open the query",
                format!("{}/queries/{}", ctx.base_url, q.id),
            )
        })
        .into_iter()
        .collect();
    secondary.push((
        "Manage notifications",
        format!("{}/settings/notifications", ctx.base_url),
    ));
    let footer = "You're receiving this because you subscribe to this monitor in arcodash.";

    let explanation_rows = explanation.map(|e| [(e.to_string(), String::new())]);
    let sections: Vec<(&str, &[(String, String)])> = explanation_rows
        .iter()
        .map(|r| ("What changed", r.as_slice()))
        .collect();
    let layout = EmailLayout {
        preheader: default_intro(new_state),
        heading: &heading,
        intro,
        facts: &facts,
        sections: &sections,
        cta: Some(("View monitor", &monitor_url)),
        secondary: &secondary,
        footer,
    };
    let (text, html) = render(&layout);

    match send_html_email(&ctx.mailer, &ctx.mail_from, to, &subject, &text, &html).await {
        Ok(arcodash_core::mailer::MailOutcome::Sent) => Outcome::sent(),
        Ok(arcodash_core::mailer::MailOutcome::Skipped) => {
            Outcome::skipped("SMTP_HOST is not configured; no email was sent")
        }
        Err(e) => {
            // `send_html_email`'s error is an SMTP transport/protocol error that doesn't embed the
            // password. Treated as transient and retried.
            let msg = e.to_string();
            tracing::warn!(%to, error = %msg, "send_notification: email delivery failed");
            Outcome::failed_transient(msg)
        }
    }
}

fn decrypt_options(key: &[u8; 32], encrypted: Option<&[u8]>) -> anyhow::Result<serde_json::Value> {
    let Some(bytes) = encrypted else {
        return Ok(serde_json::Value::Object(serde_json::Map::default()));
    };
    let plaintext = secrets::decrypt(key, bytes)?;
    Ok(serde_json::from_slice(&plaintext)?)
}

#[cfg(test)]
mod redaction_tests {
    //! Redaction rule: a stored `notification_deliveries.error` must never contain a destination
    //! URL (the Slack/Discord/Mattermost/Teams secret token) or an API key. Exercises
    //! `redact_reqwest_error` against a real `reqwest::Error` from a closed local port.
    use super::*;

    #[tokio::test]
    async fn webhook_error_redaction_strips_the_secret_url() {
        let secret_url = "http://127.0.0.1:1/services/T00000000/B00000000/supersecrettoken";
        let err = reqwest::Client::new()
            .post(secret_url)
            .send()
            .await
            .expect_err("connecting to a closed local port must fail");

        let raw = err.to_string();
        assert!(
            raw.contains("supersecrettoken"),
            "sanity check: an un-redacted reqwest error embeds the request URL, so this test \
             would be vacuous if it didn't — got: {raw}"
        );

        let redacted = redact_reqwest_error(err);
        assert!(
            !redacted.contains("supersecrettoken"),
            "redacted error must never leak the secret token embedded in the webhook URL — got: \
             {redacted}"
        );
        assert!(
            !redacted.contains("127.0.0.1"),
            "redacted error must not leak the destination host either — got: {redacted}"
        );
    }
}

#[cfg(test)]
mod custom_copy_tests {
    //! `custom_subject`/`custom_body` substitution is literal `{{name}}`/`{{state}}` replacement.
    use super::*;

    #[test]
    fn format_number_rounds_and_groups() {
        assert_eq!(format_number(0.1 + 0.2), "0.3");
        assert_eq!(format_number(12480.0), "12,480");
    }

    #[test]
    fn pagerduty_body_custom_details() {
        let plain = pagerduty_body("k", "trigger", 7, "CPU", "triggered", None);
        assert!(plain["payload"].get("custom_details").is_none());
        let with = pagerduty_body("k", "trigger", 7, "CPU", "triggered", Some("why"));
        assert_eq!(with["payload"]["custom_details"]["explanation"], "why");
        assert_eq!(with["dedup_key"], "arcodash-monitor-7");
    }

    #[test]
    fn datadog_body_text_appends() {
        let base = "arcodash monitor 'CPU' transitioned to state: triggered.";
        assert_eq!(
            datadog_body("error", 7, "CPU", "triggered", None)["text"],
            base
        );
        assert_eq!(
            datadog_body("error", 7, "CPU", "triggered", Some("why"))["text"],
            format!("{base}\n\nwhy")
        );
    }

    #[test]
    fn substitutes_both_tokens() {
        let template = Some("{{name}} is now {{state}}!".to_string());
        let rendered = render_custom_copy(template.as_deref(), "CPU usage", "triggered");
        assert_eq!(rendered, Some("CPU usage is now triggered!".to_string()));
    }

    #[test]
    fn no_tokens_present_is_used_verbatim() {
        let template = Some("Heads up, check the dashboard.".to_string());
        let rendered = render_custom_copy(template.as_deref(), "CPU usage", "triggered");
        assert_eq!(rendered, Some("Heads up, check the dashboard.".to_string()));
    }

    #[test]
    fn unset_falls_back_to_none() {
        assert_eq!(render_custom_copy(None, "CPU usage", "triggered"), None);
    }

    #[test]
    fn custom_subject_and_body_when_set() {
        let options = MonitorOptions {
            custom_subject: Some("[{{state}}] {{name}}".to_string()),
            custom_body: Some("{{name}} flipped to {{state}}.".to_string()),
            ..Default::default()
        };
        let subject =
            render_custom_copy(options.custom_subject.as_deref(), "CPU usage", "triggered")
                .unwrap_or_else(|| "unexpected fallback".to_string());
        let body = render_custom_copy(options.custom_body.as_deref(), "CPU usage", "triggered")
            .unwrap_or_else(|| "unexpected fallback".to_string());
        assert_eq!(subject, "[triggered] CPU usage");
        assert_eq!(body, "CPU usage flipped to triggered.");
    }

    #[test]
    fn falls_back_to_default_subject_and_body_when_unset() {
        let options = MonitorOptions::default();
        let subject =
            render_custom_copy(options.custom_subject.as_deref(), "CPU usage", "triggered")
                .unwrap_or_else(|| format!("Monitor '{}' is {}", "CPU usage", "triggered"));
        let body = render_custom_copy(options.custom_body.as_deref(), "CPU usage", "triggered")
            .unwrap_or_else(|| {
                format!(
                    "arcodash monitor '{}' transitioned to state: {}.",
                    "CPU usage", "triggered"
                )
            });
        assert_eq!(subject, "Monitor 'CPU usage' is triggered");
        assert_eq!(
            body,
            "arcodash monitor 'CPU usage' transitioned to state: triggered."
        );
    }
}

#[cfg(test)]
mod alert_copy_tests {
    //! The alert email's pure context-building helpers, exercised directly.
    use super::*;

    #[test]
    fn state_label_matches_the_one_vocabulary_table() {
        assert_eq!(state_label("triggered"), "Alerting");
        assert_eq!(state_label("ok"), "OK");
        assert_eq!(state_label("unevaluable"), "Can't evaluate");
        assert_eq!(state_label("unknown"), "Not checked yet");
        assert_eq!(state_label("anything-else"), "Not checked yet");
    }

    #[test]
    fn format_grouped_matches_the_frontend_convention() {
        assert_eq!(format_grouped("12480.55"), "12,480.55");
        assert_eq!(format_grouped("20000"), "20,000");
        assert_eq!(format_grouped("999"), "999");
        assert_eq!(format_grouped("-1234.5"), "-1,234.5");
        assert_eq!(format_grouped("0"), "0");
        // Non-numeric input degrades to "unchanged" rather than panicking or truncating.
        assert_eq!(format_grouped("not-a-number"), "not-a-number");
    }

    #[test]
    fn describe_condition_uses_plain_language_phrasing() {
        let options = MonitorOptions {
            column: Some("net_revenue".to_string()),
            op: Some("<".to_string()),
            value: Some(serde_json::json!(20000)),
            ..Default::default()
        };
        assert_eq!(
            describe_condition(&options),
            "net_revenue is less than 20,000"
        );
    }

    #[test]
    fn describe_condition_falls_back_when_incomplete() {
        assert_eq!(
            describe_condition(&MonitorOptions::default()),
            "No condition configured."
        );
    }

    #[test]
    fn default_subject_and_heading_per_state() {
        assert_eq!(
            default_subject("triggered", "Revenue floor breach"),
            "Alerting: Revenue floor breach"
        );
        assert_eq!(
            default_subject("ok", "Revenue floor breach"),
            "Recovered: Revenue floor breach"
        );
        assert_eq!(
            default_subject("unevaluable", "Revenue floor breach"),
            "Can't evaluate: Revenue floor breach"
        );
        assert_eq!(
            default_heading("triggered", "Revenue floor breach"),
            "Revenue floor breach is alerting"
        );
        assert_eq!(
            default_heading("ok", "Revenue floor breach"),
            "Revenue floor breach is back to OK"
        );
    }

    #[test]
    fn json_cell_to_string_prefers_the_original_string_encoding() {
        // NUMERIC/UInt64 arrive as JSON strings for precision and must survive verbatim, never
        // re-parsed through a float.
        assert_eq!(
            json_cell_to_string(&serde_json::json!("10813421.70")),
            "10813421.70"
        );
        assert_eq!(json_cell_to_string(&serde_json::json!(42)), "42");
    }
}

#[cfg(test)]
mod response_classification_tests {
    //! A non-2xx status must not be recorded as a successful send (`reqwest`'s `Ok` only means a
    //! response arrived).
    use super::*;

    fn status(code: u16) -> reqwest::StatusCode {
        reqwest::StatusCode::from_u16(code).unwrap()
    }

    #[test]
    fn two_xx_is_sent() {
        for code in [200, 201, 202, 204, 299] {
            let outcome = classify_response("test", status(code));
            assert_eq!(outcome.status, "sent", "status {code} must be sent");
        }
    }

    #[test]
    fn timeout_and_rate_limit_and_5xx_are_transient() {
        for code in [408, 429, 500, 502, 503, 504, 599] {
            let outcome = classify_response("test", status(code));
            assert_eq!(outcome.status, "failed", "status {code} must be failed");
            assert!(
                outcome.retryable,
                "status {code} must be retryable (transient)"
            );
        }
    }

    #[test]
    fn other_4xx_is_permanent() {
        for code in [400, 401, 403, 404, 410, 422] {
            let outcome = classify_response("test", status(code));
            assert_eq!(outcome.status, "failed", "status {code} must be failed");
            assert!(
                !outcome.retryable,
                "status {code} must NOT be retryable (permanent config/auth problem)"
            );
        }
    }

    #[test]
    fn error_message_never_leaks_anything_beyond_the_bare_status_code() {
        let outcome = classify_response("test", status(500));
        let msg = outcome.error.unwrap();
        assert_eq!(msg, "destination responded with status 500");
    }
}

/// Shared fixtures for the real-Postgres/Redis test modules below.
#[cfg(test)]
mod fixtures {
    use super::*;
    pub(super) use crate::scheduler::test_support::{
        seed_data_source, seed_org, seed_published_query, seed_query, seed_user, setup, test_ctx,
    };
    use arcodash_core::entities::monitor_subscription;
    use lettre::{AsyncSmtpTransport, Tokio1Executor};

    pub(super) fn now() -> sea_orm::prelude::DateTimeWithTimeZone {
        chrono::Utc::now().into()
    }

    /// A first-attempt `"triggered"` send with no claimed row, subscription or captured context.
    pub(super) fn job(
        org_id: i64,
        monitor_id: i64,
        destination_id: i64,
        subscriber_id: i64,
    ) -> SendNotificationParams {
        SendNotificationParams {
            org_id,
            monitor_id,
            destination_id,
            new_state: "triggered".to_string(),
            subscriber_id,
            subscription_id: None,
            attempt: 1,
            delivery_id: None,
            observed: None,
            explanation: None,
        }
    }

    /// `test_ctx` with a real SMTP transport.
    pub(super) fn ctx_with_mailer(
        db: &sea_orm::DatabaseConnection,
        redis: redis::aio::ConnectionManager,
        mailer: AsyncSmtpTransport<Tokio1Executor>,
    ) -> ExecuteQueryContext {
        let mut ctx = test_ctx(db.clone(), redis);
        ctx.mailer = Some(Arc::new(mailer));
        ctx
    }

    /// A real SMTP transport on a closed local port (`builder_dangerous`: no TLS, so it fails fast
    /// at TCP): any send attempt fails loudly.
    pub(super) fn closed_port_mailer() -> AsyncSmtpTransport<Tokio1Executor> {
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("127.0.0.1")
            .port(1)
            .build()
    }

    /// Seed one org's own user + data source + query + monitor as real rows. The user is in the
    /// builtin `default` group: owning the monitor clears `can_view_monitor`'s reach half, but the
    /// capability half needs a group.
    pub(super) async fn seed_org_with_monitor(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
    ) -> (monitor::Model, i64) {
        let now = now();
        let suffix = uuid::Uuid::new_v4();
        let groups = arcodash_core::org_seed::seed_builtin_groups(db, org_id)
            .await
            .unwrap();

        let u = seed_user(db, org_id, vec![groups.default_group_id]).await;

        let ds = seed_data_source(db, org_id).await;
        let q = query::ActiveModel {
            org_id: Set(org_id),
            data_source_id: Set(ds.id),
            name: Set(format!("send-notif-test-query-{suffix}")),
            description: Set(None),
            query: Set("SELECT 1".to_string()),
            query_hash: Set(format!("hash-{suffix}")),
            api_key: Set(suffix.to_string()),
            user_id: Set(u.id),
            last_modified_by_id: Set(None),
            latest_query_data_id: Set(None),
            is_archived: Set(false),
            options: Set(serde_json::json!({})),
            tags: Set(None),
            last_execution_status: Set(String::new()),
            last_execution_at: Set(None),
            row_count: Set(None),
            last_execution_error: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();

        let a = monitor::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("send-notif-test-monitor-{suffix}")),
            query_id: Set(q.id),
            user_id: Set(u.id),
            options: Set(serde_json::json!({})),
            state: Set("unknown".to_string()),
            last_triggered_at: Set(None),
            rearm: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();

        (a, u.id)
    }

    /// A destination whose stored `options` are `opts` encrypted under the test key.
    pub(super) async fn seed_dest(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        r#type: &str,
        opts: serde_json::Value,
    ) -> notification_destination::Model {
        let encrypted = secrets::encrypt(&[0u8; 32], &serde_json::to_vec(&opts).unwrap()).unwrap();
        seed_raw_dest(db, org_id, r#type, encrypted).await
    }

    pub(super) async fn seed_raw_dest(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        r#type: &str,
        options: Vec<u8>,
    ) -> notification_destination::Model {
        notification_destination::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("{}-dest-{}", r#type, uuid::Uuid::new_v4())),
            r#type: Set(r#type.to_string()),
            options: Set(Some(options)),
            user_id: Set(None),
            created_at: Set(now()),
            updated_at: Set(now()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    pub(super) async fn subscribe(
        db: &sea_orm::DatabaseConnection,
        monitor_id: i64,
        user_id: i64,
        destination_id: Option<i64>,
    ) -> monitor_subscription::Model {
        monitor_subscription::ActiveModel {
            monitor_id: Set(monitor_id),
            user_id: Set(user_id),
            destination_id: Set(destination_id),
            created_at: Set(now()),
            updated_at: Set(now()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    pub(super) async fn insert_pending(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        monitor_id: i64,
        subscription_id: Option<i64>,
        destination_id: i64,
    ) -> i64 {
        notification_delivery::insert_pending(
            db,
            org_id,
            monitor_id,
            subscription_id,
            destination_id,
            "triggered",
            None,
        )
        .await
        .unwrap()
    }

    pub(super) async fn delivery_rows(
        db: &sea_orm::DatabaseConnection,
        monitor_id: i64,
    ) -> Vec<notification_delivery::Model> {
        notification_delivery::Entity::find()
            .filter(notification_delivery::Column::MonitorId.eq(monitor_id))
            .all(db)
            .await
            .unwrap()
    }

    pub(super) async fn delivery_row(
        db: &sea_orm::DatabaseConnection,
        id: i64,
    ) -> notification_delivery::Model {
        notification_delivery::Entity::find_by_id(id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    /// A real Mailpit (SMTP `localhost:1026`, override `MAILPIT_SMTP_PORT`; HTTP API
    /// `MAILPIT_API_BASE`, default `http://localhost:8026`).
    pub(super) struct Mailpit {
        api: String,
        http: reqwest::Client,
    }

    impl Mailpit {
        /// `None` (the test skips) when Mailpit isn't reachable.
        pub(super) async fn connect() -> Option<Self> {
            let api = std::env::var("MAILPIT_API_BASE")
                .unwrap_or_else(|_| "http://localhost:8026".to_string());
            let http = reqwest::Client::new();
            if http.get(format!("{api}/api/v1/info")).send().await.is_err() {
                eprintln!("skipping: Mailpit not reachable at {api}");
                return None;
            }
            Some(Self { api, http })
        }

        pub(super) fn mailer() -> AsyncSmtpTransport<Tokio1Executor> {
            let port: u16 = std::env::var("MAILPIT_SMTP_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1026);
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("localhost")
                .port(port)
                .build()
        }

        /// The full message sent to `to`, polled for up to ~9s.
        pub(super) async fn message_to(&self, to: &str) -> serde_json::Value {
            let mut message_id = None;
            for _ in 0..30 {
                let search: serde_json::Value = self
                    .http
                    .get(format!("{}/api/v1/search", self.api))
                    .query(&[("query", format!("to:{to}"))])
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if let Some(m) = search["messages"].as_array().and_then(|a| a.first()) {
                    message_id = m["ID"].as_str().map(str::to_string);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
            let message_id = message_id.expect("alert email must land in Mailpit within ~9s");
            self.http
                .get(format!("{}/api/v1/message/{message_id}", self.api))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap()
        }
    }
}

#[cfg(test)]
mod org_scoping_tests {
    //! `handle_inner` loads the monitor (and destination/user) with an org filter, since the worker
    //! is RLS-exempt and can't rely on producers pairing ids from one org. A job whose claimed
    //! `org_id` differs from the monitor's must not find it (and so never notify), observed via
    //! `handle_inner`'s return (`Ok(true)` = notify path reached, `Ok(false)` = not found). Real
    //! Postgres + Redis only.
    use super::fixtures::*;
    use super::*;
    use crate::scheduler::test_support::exec_sql;

    #[tokio::test]
    async fn cross_org_job_org_id_does_not_reach_or_notify_foreign_monitor() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org_attacker = seed_org(&db, &format!("org-attacker-sendnotif-{suffix}")).await;
        let org_victim = seed_org(&db, &format!("org-victim-sendnotif-{suffix}")).await;
        let (victim_monitor, victim_user_id) = seed_org_with_monitor(&db, org_victim.id).await;
        let ctx = test_ctx(db.clone(), redis);

        // Attack: a job claiming `org_attacker.id` but naming the victim's monitor_id/subscriber_id
        // must not find the monitor and so must not notify.
        let attacker_job = job(org_attacker.id, victim_monitor.id, -1, victim_user_id);
        let attacker_reached_notify = handle_inner(&ctx, attacker_job).await.unwrap();
        assert!(
            !attacker_reached_notify,
            "a job whose org_id doesn't match the monitor's real org must never reach the notify \
             path for that monitor"
        );

        // Control: the same monitor/subscriber, but with the CORRECT org_id -- must proceed.
        let victim_job = job(org_victim.id, victim_monitor.id, -1, victim_user_id);
        let victim_reached_notify = handle_inner(&ctx, victim_job).await.unwrap();
        assert!(
            victim_reached_notify,
            "same-org job must reach the notify path"
        );
    }

    /// Guards the `Ok(None)`/`Err` split in `observed_value`: a monitor with no published result
    /// reads "no value", not the DB-error fallback.
    #[tokio::test]
    async fn observed_value_reports_no_value_when_no_result_is_resolvable_yet() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-observed-novalue-{suffix}")).await;
        let (monitor_model, _) = seed_org_with_monitor(&db, org.id).await;
        exec_sql(
            &db,
            "UPDATE monitors SET options = $1 WHERE id = $2",
            [
                serde_json::json!({"column": "net_revenue"}).into(),
                monitor_model.id.into(),
            ],
        )
        .await;
        let reloaded = monitor::Entity::find_by_id(monitor_model.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();

        let ctx = test_ctx(db.clone(), redis);
        assert_eq!(observed_value(&ctx, &reloaded, org.id).await, "no value");
    }

    /// A forced delivery failure (a webhook to an address nothing listens on, since an unconfigured
    /// mailer makes email succeed trivially) must record `status = "failed"`, never `"sent"`, with
    /// a redacted non-empty `error`. Called with `attempt = MAX_ATTEMPTS` to exercise the terminal
    /// path; the retry path is covered by `retry_tests`.
    #[tokio::test]
    async fn forced_delivery_failure_records_failed_never_sent() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-delivery-fail-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;

        // A secret-bearing webhook path on a closed local port: the send fails fast and the failure
        // text must not contain the secret path or host.
        let secret_path = format!("supersecrettoken-{suffix}");
        let opts = serde_json::json!({ "url": format!("http://127.0.0.1:1/hooks/{secret_path}") });
        let dest = seed_dest(&db, org.id, "webhook", opts).await;
        let sub = subscribe(&db, monitor_model.id, user_id, Some(dest.id)).await;
        let ctx = test_ctx(db.clone(), redis);

        // Real send attempt: nothing listens on 127.0.0.1:1, so this is a genuine `reqwest`
        // connection-refused failure. `ARCODASH_ALLOW_PRIVATE_CONNECTORS=1` bypasses the SSRF guard
        // so the attempt reaches `reqwest`. Not parallel-safe (process-global env var); restored
        // right after the call.
        std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
        let result = handle_inner(
            &ctx,
            SendNotificationParams {
                subscription_id: Some(sub.id),
                attempt: MAX_ATTEMPTS,
                ..job(org.id, monitor_model.id, dest.id, user_id)
            },
        )
        .await;
        std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
        result.unwrap();

        let rows = delivery_rows(&db, monitor_model.id).await;
        assert_eq!(rows.len(), 1, "exactly one delivery attempt was made");
        let row = &rows[0];
        assert_eq!(
            row.status, "failed",
            "a real send failure at the final attempt must never be recorded as sent"
        );
        assert_eq!(row.subscription_id, Some(sub.id));
        assert_eq!(row.destination_id, Some(dest.id));
        assert_eq!(row.state, "triggered");
        assert_eq!(row.attempt_number, MAX_ATTEMPTS as i32);
        let error = row.error.as_deref().unwrap_or("");
        assert!(!error.is_empty(), "a failed delivery must record an error");
        assert!(
            !error.contains(&secret_path),
            "stored error must not leak the webhook's secret URL path — got: {error}"
        );
        assert!(
            !error.contains("127.0.0.1"),
            "stored error must not leak the destination host — got: {error}"
        );
    }

    /// A decryption failure must not propagate out of `handle_inner` before the delivery insert: an
    /// undecryptable destination still writes exactly one `failed` row with a redacted error, and
    /// `handle_inner` returns `Ok(true)`.
    #[tokio::test]
    async fn corrupt_destination_options_still_records_a_failed_row() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-corrupt-dest-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;

        // Garbage bytes: long enough to pass the nonce-length check, never valid AES-GCM.
        let dest = seed_raw_dest(&db, org.id, "email", vec![0xAB; 32]).await;
        let sub = subscribe(&db, monitor_model.id, user_id, Some(dest.id)).await;
        let ctx = test_ctx(db.clone(), redis);

        let reached_notify = handle_inner(
            &ctx,
            SendNotificationParams {
                subscription_id: Some(sub.id),
                ..job(org.id, monitor_model.id, dest.id, user_id)
            },
        )
        .await
        .unwrap();
        assert!(
            reached_notify,
            "a decrypt failure must not propagate out of handle_inner as an Err — it must be \
             turned into a recorded Outcome::failed"
        );

        let rows = delivery_rows(&db, monitor_model.id).await;
        assert_eq!(
            rows.len(),
            1,
            "a corrupt/undecryptable destination must still write exactly one delivery row, not \
             zero"
        );
        let row = &rows[0];
        assert_eq!(
            row.status, "failed",
            "a decrypt failure is a failed delivery, never sent"
        );
        assert_eq!(row.destination_id, Some(dest.id));
        assert_eq!(row.subscription_id, Some(sub.id));
        let error = row.error.as_deref().unwrap_or("");
        assert_eq!(
            error, "destination configuration error",
            "the stored error must be the fixed redacted message, never raw decryption/ciphertext \
             internals"
        );
    }
}

#[cfg(test)]
mod smtp_failure_tests {
    //! A real SMTP failure must record `status = "failed"`, never `"sent"`, using a real `lettre`
    //! transport at an unreachable host. Real Postgres + Redis only.
    use super::fixtures::*;
    use super::*;

    #[tokio::test]
    async fn forced_smtp_failure_records_failed_never_sent() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-smtp-fail-{suffix}")).await;
        let (mon, user_id) = seed_org_with_monitor(&db, org.id).await;
        let ctx = ctx_with_mailer(&db, redis, closed_port_mailer());

        // destination_id = -1 (direct-subscriber email), called at `attempt = MAX_ATTEMPTS` to
        // exercise the terminal path; the retry path is covered by `retry_tests`.
        let terminal = SendNotificationParams {
            attempt: MAX_ATTEMPTS,
            ..job(org.id, mon.id, -1, user_id)
        };
        handle_inner(&ctx, terminal).await.unwrap();

        let rows = delivery_rows(&db, mon.id).await;
        assert_eq!(rows.len(), 1, "exactly one delivery attempt was made");
        let row = &rows[0];
        assert_eq!(
            row.status, "failed",
            "a real SMTP connection failure at the final attempt must never be recorded as sent"
        );
        assert_eq!(
            row.destination_id, None,
            "the -1 sentinel maps to a null destination_id"
        );
        let error = row.error.as_deref().unwrap_or("");
        assert!(!error.is_empty(), "a failed delivery must record an error");
    }
}

#[cfg(test)]
mod mailpit_alert_tests {
    //! The alert email flow against a real Mailpit. Skips if Mailpit isn't reachable; real
    //! Postgres + Redis only.
    use super::fixtures::*;
    use super::*;
    use crate::scheduler::test_support::exec_sql;

    /// A subscriber-owned, `triggered` monitor named `monitor_name` on the query "Net revenue by
    /// store and month", whose cached first row has `net_revenue` = `net_revenue` and whose
    /// condition is `net_revenue < 20000`.
    async fn seed_alerting_monitor(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        monitor_name: &str,
        net_revenue: f64,
    ) -> (user::Model, query::Model, monitor::Model) {
        let now = now();
        let suffix = uuid::Uuid::new_v4();
        let subscriber = seed_user(db, org_id, vec![]).await;

        let ds = seed_data_source(db, org_id).await;
        let qr = query_result::ActiveModel {
            org_id: Set(org_id),
            data_source_id: Set(ds.id),
            query_hash: Set(format!("hash-{suffix}")),
            query: Set("SELECT 1".to_string()),
            data: Set(Some(
                serde_json::json!({ "rows": [{ "net_revenue": net_revenue }] }),
            )),
            data_location: Set(None),
            data_size_bytes: Set(None),
            runtime: Set(Some(0.01)),
            retrieved_at: Set(now),
            last_used_at: Set(now),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();

        let q = query::ActiveModel {
            org_id: Set(org_id),
            data_source_id: Set(ds.id),
            name: Set("Net revenue by store and month".to_string()),
            description: Set(None),
            query: Set("SELECT 1".to_string()),
            query_hash: Set(format!("hash-{suffix}")),
            api_key: Set(suffix.to_string()),
            user_id: Set(subscriber.id),
            last_modified_by_id: Set(None),
            latest_query_data_id: Set(Some(qr.id)),
            is_archived: Set(false),
            options: Set(serde_json::json!({})),
            tags: Set(None),
            last_execution_status: Set(String::new()),
            last_execution_at: Set(None),
            row_count: Set(None),
            last_execution_error: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();

        let mon = monitor::ActiveModel {
            org_id: Set(org_id),
            name: Set(monitor_name.to_string()),
            query_id: Set(q.id),
            user_id: Set(subscriber.id),
            options: Set(serde_json::json!({ "column": "net_revenue", "op": "<", "value": 20000 })),
            state: Set("triggered".to_string()),
            last_triggered_at: Set(Some(now)),
            rearm: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();

        (subscriber, q, mon)
    }

    #[tokio::test]
    async fn triggered_alert_lands_in_mailpit_with_full_context() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let Some(mailpit) = Mailpit::connect().await else {
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-mailpit-alert-{suffix}")).await;
        // A non-ASCII monitor name doubles as this test's non-ASCII round-trip check.
        let monitor_name = "Café floor breach";
        let (subscriber, q, mon) = seed_alerting_monitor(&db, org.id, monitor_name, 12480.55).await;

        // A prior transition for `previous_state_label` to read back.
        record_event(
            &db,
            org.id,
            None,
            arcodash_core::events::MONITOR_STATE_CHANGED,
            Some("monitor"),
            Some(mon.id),
            serde_json::json!({ "old_state": "ok", "new_state": "triggered" }),
        )
        .await
        .unwrap();

        let ctx = ctx_with_mailer(&db, redis, Mailpit::mailer());
        let outcome = handle_inner(&ctx, job(org.id, mon.id, -1, subscriber.id))
            .await
            .unwrap();
        assert!(outcome, "the notify path must have been reached");

        let row = notification_delivery::Entity::find()
            .filter(notification_delivery::Column::MonitorId.eq(mon.id))
            .one(&db)
            .await
            .unwrap()
            .expect("a delivery row must have been recorded");
        assert_eq!(
            row.status, "sent",
            "a real Mailpit send must record 'sent', got error: {:?}",
            row.error
        );

        let detail = mailpit.message_to(&subscriber.email).await;
        let subject = detail["Subject"].as_str().unwrap_or_default();
        assert_eq!(subject, format!("Alerting: {monitor_name}"));

        let html = detail["HTML"].as_str().unwrap_or_default();
        let text = detail["Text"].as_str().unwrap_or_default();
        assert!(!html.is_empty(), "HTML part must be present");
        assert!(!text.is_empty(), "text alternative must be present");

        for part in [html, text] {
            assert!(
                part.contains(monitor_name),
                "monitor name must be named: {part}"
            );
            assert!(
                part.contains("Net revenue by store and month"),
                "query name must be named: {part}"
            );
            assert!(
                part.contains("net_revenue is less than 20,000"),
                "condition must use plain-language phrasing with grouped threshold: {part}"
            );
            assert!(
                part.contains("12,480.55"),
                "observed value must be grouped, not raw: {part}"
            );
            assert!(
                part.contains("(first row)"),
                "observed value must carry the first-row qualifier: {part}"
            );
            assert!(
                part.contains("OK"),
                "previous state must be read back from the audit event: {part}"
            );
            assert!(
                part.contains(&format!("{}/monitors/{}", ctx.base_url, mon.id)),
                "must link to the monitor: {part}"
            );
            assert!(
                part.contains(&format!("{}/queries/{}", ctx.base_url, q.id)),
                "must link to the query: {part}"
            );
        }
        assert!(
            text.contains("Café"),
            "non-ASCII monitor name must survive in the text part: {text}"
        );
        assert!(
            html.contains("Café"),
            "non-ASCII monitor name must survive in the HTML part too: {html}"
        );
    }

    /// The email must show the value that decided the transition (the `observed` param), not the
    /// live `query_result` row, which can have moved on by send time. The two are made different so
    /// a leak would read as self-contradicting.
    #[tokio::test]
    async fn observed_value_in_email_is_the_evaluation_time_capture_not_a_send_time_reread() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let Some(mailpit) = Mailpit::connect().await else {
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-mailpit-drift-{suffix}")).await;
        // The LIVE result at send time, far from the threshold on the OK side (a recovery during
        // the retry/escalation window).
        let (subscriber, _, mon) =
            seed_alerting_monitor(&db, org.id, "Evaluation-time value monitor", 31204.00).await;

        let ctx = ctx_with_mailer(&db, redis, Mailpit::mailer());
        // The value that decided this transition: 12,480.55, under the 20,000 threshold.
        let evaluation_time_observed = "12,480.55 (first row)";
        let outcome = handle_inner(
            &ctx,
            SendNotificationParams {
                observed: Some(evaluation_time_observed.to_string()),
                ..job(org.id, mon.id, -1, subscriber.id)
            },
        )
        .await
        .unwrap();
        assert!(outcome, "the notify path must have been reached");

        let detail = mailpit.message_to(&subscriber.email).await;
        let html = detail["HTML"].as_str().unwrap_or_default();
        let text = detail["Text"].as_str().unwrap_or_default();
        for part in [html, text] {
            assert!(
                part.contains(evaluation_time_observed),
                "must show the value captured at evaluation time: {part}"
            );
            assert!(
                !part.contains("31,204.00"),
                "must NOT show the live, drifted query_result value: {part}"
            );
        }
    }

    /// A query id the org-scoped lookup can't find (e.g. another org's) must render the same
    /// neutral fact as a DB error, never the unbacked claim "(deleted query)".
    #[tokio::test]
    async fn query_lookup_miss_renders_as_unavailable_not_deleted_query() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let Some(mailpit) = Mailpit::connect().await else {
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org_a = seed_org(&db, &format!("org-unavailable-query-a-{suffix}")).await;
        let org_b = seed_org(&db, &format!("org-unavailable-query-b-{suffix}")).await;
        let (monitor_a, subscriber_id) = seed_org_with_monitor(&db, org_a.id).await;
        let (monitor_b, _) = seed_org_with_monitor(&db, org_b.id).await;

        // Point org_a's monitor at org_b's query: the FK only needs the row to exist, and the
        // org-scoped lookup still misses.
        exec_sql(
            &db,
            "UPDATE monitors SET query_id = $1 WHERE id = $2",
            [monitor_b.query_id.into(), monitor_a.id.into()],
        )
        .await;

        let subscriber = user::Entity::find_by_id(subscriber_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();

        let ctx = ctx_with_mailer(&db, redis, Mailpit::mailer());
        let outcome = handle_inner(
            &ctx,
            SendNotificationParams {
                new_state: "ok".to_string(),
                ..job(org_a.id, monitor_a.id, -1, subscriber.id)
            },
        )
        .await
        .unwrap();
        assert!(outcome, "the notify path must have been reached");

        let detail = mailpit.message_to(&subscriber.email).await;
        let html = detail["HTML"].as_str().unwrap_or_default();
        let text = detail["Text"].as_str().unwrap_or_default();
        for part in [html, text] {
            assert!(
                part.contains("(unavailable)"),
                "an org-scoped lookup miss must render the neutral fallback: {part}"
            );
            assert!(
                !part.contains("(deleted query)"),
                "must never assert deletion as an established fact it can't back up: {part}"
            );
        }
    }
}

#[cfg(test)]
mod retry_tests {
    //! Bounded retry: a transient failure with `attempt < MAX_ATTEMPTS` records "retrying" and
    //! re-enqueues attempt+1 after a backoff; a final-attempt or any non-transient failure is
    //! terminal. Real Postgres + Redis only.
    use super::fixtures::*;
    use super::*;

    use arcodash_core::queue::{JobMessage, Queue, QueueName};

    /// Scans the shared "Default" queue for a `SendNotification` job carrying `delivery_id`,
    /// removing only that entry (a blind drain could eat another test's job).
    async fn find_and_remove_notification_for_delivery(
        redis: &redis::aio::ConnectionManager,
        key: &str,
        delivery_id: i64,
    ) -> Option<JobMessage> {
        crate::scheduler::test_support::find_and_remove_job(redis, key, |job| {
            matches!(job, JobMessage::SendNotification { run_id: Some(rid), .. } if *rid == delivery_id)
        })
        .await
    }

    /// A webhook destination on a closed local port: every attempt is a genuine transient
    /// connection-refused failure.
    async fn seed_webhook_dest(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
    ) -> notification_destination::Model {
        let opts = serde_json::json!({ "url": "http://127.0.0.1:1/hooks/retry-test" });
        seed_dest(db, org_id, "webhook", opts).await
    }

    /// Runs `job` with the SSRF guard bypassed (the webhook targets 127.0.0.1); not parallel-safe
    /// (process-global env var), restored right after the call.
    async fn handle_private(ctx: &ExecuteQueryContext, job: SendNotificationParams) {
        std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
        let result = handle_inner(ctx, job).await;
        std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
        result.unwrap();
    }

    #[tokio::test]
    async fn transient_failure_at_attempt_one_records_retrying_not_failed() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-retry-attempt1-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;
        let dest = seed_webhook_dest(&db, org.id).await;
        let ctx = test_ctx(db.clone(), redis);

        handle_private(&ctx, job(org.id, monitor_model.id, dest.id, user_id)).await;

        let rows = delivery_rows(&db, monitor_model.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].status, "retrying",
            "a transient failure below MAX_ATTEMPTS must record 'retrying', never 'failed'"
        );
        assert_eq!(rows[0].attempt_number, 1);
    }

    #[tokio::test]
    async fn transient_failure_at_final_attempt_records_terminal_failed() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-retry-final-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;
        let dest = seed_webhook_dest(&db, org.id).await;
        let ctx = test_ctx(db.clone(), redis);

        let terminal = SendNotificationParams {
            attempt: MAX_ATTEMPTS,
            ..job(org.id, monitor_model.id, dest.id, user_id)
        };
        handle_private(&ctx, terminal).await;

        let rows = delivery_rows(&db, monitor_model.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].status, "failed",
            "a transient failure AT the final attempt must be terminal, not retried further"
        );
        assert_eq!(rows[0].attempt_number, MAX_ATTEMPTS as i32);
    }

    #[tokio::test]
    async fn non_transient_failure_at_attempt_one_records_terminal_failed_immediately() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-retry-nontransient-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;
        // A PagerDuty destination with no `integration_key`: a non-transient config error.
        let dest = seed_dest(&db, org.id, "pagerduty", serde_json::json!({})).await;
        let ctx = test_ctx(db.clone(), redis);

        handle_inner(&ctx, job(org.id, monitor_model.id, dest.id, user_id))
            .await
            .unwrap();

        let rows = delivery_rows(&db, monitor_model.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].status, "failed",
            "a non-transient (config) failure must record terminal 'failed' immediately, even \
             with attempts remaining — retrying a missing integration key never helps"
        );
        assert_eq!(rows[0].attempt_number, 1);
    }

    #[tokio::test]
    async fn email_destination_missing_to_records_terminal_failed_not_unknown() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-retry-email-missing-to-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;
        // An email destination with no `to` address: a non-transient config error.
        let dest = seed_dest(&db, org.id, "email", serde_json::json!({})).await;
        let ctx = test_ctx(db.clone(), redis);

        handle_inner(&ctx, job(org.id, monitor_model.id, dest.id, user_id))
            .await
            .unwrap();

        let rows = delivery_rows(&db, monitor_model.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].status, "failed",
            "an email destination missing 'to' must record terminal 'failed' immediately, \
             never silently 'sent' to a fabricated (unknown) address"
        );
        assert_eq!(rows[0].attempt_number, 1);
        assert_eq!(
            rows[0].error.as_deref(),
            Some("email destination is missing a 'to' address")
        );
    }

    #[tokio::test]
    async fn completion_after_prior_retry_records_real_attempt_number() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-retry-success-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;
        let ctx = test_ctx(db.clone(), redis);

        // destination_id = -1 with no mailer reports `Skipped`, as the second attempt of a job that
        // failed once.
        let second = SendNotificationParams {
            attempt: 2,
            ..job(org.id, monitor_model.id, -1, user_id)
        };
        handle_inner(&ctx, second).await.unwrap();

        let rows = delivery_rows(&db, monitor_model.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "skipped");
        assert_eq!(
            rows[0].attempt_number, 2,
            "a skip on a retried attempt must still record the real attempt number, not always 1"
        );
    }

    /// Real-dependency test of the retry path: a webhook to a closed local port forces a genuine
    /// transient failure, and the `tokio::spawn` + sleep + re-enqueue path runs for real.
    /// `NOTIFICATION_RETRY_BACKOFF_SECS_BASE=1` shrinks the backoff only to avoid real sleeps.
    /// Claim-in-place: the pre-created `insert_pending` row is passed as `delivery_id`, so the
    /// chain updates the same row across two dispatches; the single-row assertion proves it. Flow:
    /// attempt 1 records "retrying" and spawns the re-enqueue; poll Redis for the attempt-2 job
    /// with the same delivery id; hand it to the real `consumer::dispatch`; confirm one row,
    /// updated in place.
    #[tokio::test]
    async fn transient_failure_genuinely_reenqueues_and_consumer_processes_second_attempt() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-retry-chain-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;
        let dest = seed_webhook_dest(&db, org.id).await;
        let ctx = test_ctx(db.clone(), redis.clone());

        let delivery_id = insert_pending(&db, org.id, monitor_model.id, None, dest.id).await;
        let queue_key = QueueName::Default.queue_key();

        std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
        std::env::set_var("NOTIFICATION_RETRY_BACKOFF_SECS_BASE", "1");
        let first = SendNotificationParams {
            delivery_id: Some(delivery_id),
            ..job(org.id, monitor_model.id, dest.id, user_id)
        };
        handle_inner(&ctx, first).await.unwrap();

        // Poll for the re-enqueued job, scoped to this delivery's id since the shared "Default"
        // queue may carry other tests' jobs.
        let mut requeued: Option<JobMessage> = None;
        for _ in 0..50 {
            if let Some(job) =
                find_and_remove_notification_for_delivery(&redis, &queue_key, delivery_id).await
            {
                requeued = Some(job);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
        std::env::remove_var("NOTIFICATION_RETRY_BACKOFF_SECS_BASE");

        let message = requeued.expect(
            "the spawned retry task must have LPUSHed a real SendNotification job onto the \
             actual Redis queue within the timeout",
        );
        let JobMessage::SendNotification {
            monitor_id: requeued_monitor_id,
            destination_id: requeued_destination_id,
            attempt: requeued_attempt,
            run_id: requeued_delivery_id,
            ..
        } = message.clone()
        else {
            panic!("expected a re-enqueued SendNotification job, got {message:?}");
        };
        assert_eq!(requeued_monitor_id, monitor_model.id);
        assert_eq!(requeued_destination_id, dest.id);
        assert_eq!(
            requeued_attempt, 2,
            "the re-enqueued job must carry attempt = original attempt + 1"
        );
        assert_eq!(
            requeued_delivery_id,
            Some(delivery_id),
            "the re-enqueue must carry the SAME delivery id through the chain"
        );

        // Hand the job to the real consumer dispatch, so attempt 2 runs through production code.
        std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
        crate::consumer::dispatch(&ctx, message).await;
        std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);

        let rows = delivery_rows(&db, monitor_model.id).await;
        assert_eq!(
            rows.len(),
            1,
            "a claimed delivery's retry chain must update the SAME row in place across a real \
             re-enqueue + dispatch cycle, never insert a second row"
        );
        assert_eq!(rows[0].id, delivery_id);
        assert_eq!(
            rows[0].attempt_number, 2,
            "the row's attempt_number must have advanced from 1 to 2 in place"
        );
        assert_eq!(
            rows[0].status, "retrying",
            "still below MAX_ATTEMPTS=3 after the second attempt, so still retrying"
        );
    }

    /// A `Queue` whose `enqueue` always fails, to reach the retry-scheduling-failure branch
    /// deterministically.
    struct FailingQueue;

    #[async_trait::async_trait]
    impl Queue for FailingQueue {
        async fn enqueue(&self, _queue: QueueName, _job: JobMessage) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("queue unreachable"))
        }
    }

    /// When the retry can't be enqueued (queue down), the delivery must end terminal `failed`, not
    /// sit at `retrying`; `finalize`'s fence never matches this unleased row, hence
    /// `delivery_lifecycle::fail_retrying`.
    #[tokio::test]
    async fn a_delivery_whose_retry_cannot_be_scheduled_ends_terminal_not_retrying() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-retry-sched-fail-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;
        let dest = seed_webhook_dest(&db, org.id).await;

        let mut ctx = test_ctx(db.clone(), redis);
        ctx.queue = Arc::new(FailingQueue);

        let delivery_id = insert_pending(&db, org.id, monitor_model.id, None, dest.id).await;

        std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
        std::env::set_var("NOTIFICATION_RETRY_BACKOFF_SECS_BASE", "0");
        let first = SendNotificationParams {
            delivery_id: Some(delivery_id),
            ..job(org.id, monitor_model.id, dest.id, user_id)
        };
        handle_inner(&ctx, first).await.unwrap();

        // The spawned retry task sleeps `retry_backoff_secs(1)` then terminalizes the row; poll
        // rather than assert immediately.
        let mut row = delivery_row(&db, delivery_id).await;
        for _ in 0..50 {
            if row.status == "failed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            row = delivery_row(&db, delivery_id).await;
        }
        std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
        std::env::remove_var("NOTIFICATION_RETRY_BACKOFF_SECS_BASE");

        assert_eq!(
            row.status, "failed",
            "a retry that could never be scheduled must end terminal 'failed', not stuck at \
             'retrying'"
        );
        assert!(row.finished_at.is_some());
        assert_eq!(row.error.as_deref(), Some("failed to schedule retry"));
        assert_eq!(row.attempt_number, 2);
    }

    /// A blocked org's pending delivery is finalized as `skipped`, not sent or left `pending`. The
    /// destination is a connection-refused address, so a bypassed gate would show `failed` instead,
    /// distinguishing "never attempted" from "attempted and failed".
    #[tokio::test]
    async fn blocked_org_finalizes_pending_delivery_as_skipped_without_sending() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-blocked-notif-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;
        let dest = seed_webhook_dest(&db, org.id).await;
        let mut ctx = test_ctx(db.clone(), redis);
        ctx.entitlements = Arc::new(crate::scheduler::test_support::BlockedByTest);

        let delivery_id = insert_pending(&db, org.id, monitor_model.id, None, dest.id).await;

        let blocked = SendNotificationParams {
            delivery_id: Some(delivery_id),
            ..job(org.id, monitor_model.id, dest.id, user_id)
        };
        let reached_send = handle_inner(&ctx, blocked).await.unwrap();
        assert!(
            !reached_send,
            "a blocked org must never reach a send attempt"
        );

        let row = delivery_row(&db, delivery_id).await;
        assert_eq!(row.status, "skipped");
        assert_eq!(row.error.as_deref(), Some("blocked by test"));
        assert!(row.finished_at.is_some());
        assert!(row.lease_expires_at.is_none());
        assert!(row.worker_id.is_none());
    }

    /// Claim-in-place happy path: a pre-created `pending` row completes on one attempt,
    /// transitioning in place with no second row.
    #[tokio::test]
    async fn claim_in_place_happy_path_updates_the_pre_created_row() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-claim-happy-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;
        let ctx = test_ctx(db.clone(), redis);

        let delivery_id = insert_pending(&db, org.id, monitor_model.id, None, -1).await;

        let claimed = SendNotificationParams {
            delivery_id: Some(delivery_id),
            ..job(org.id, monitor_model.id, -1, user_id)
        };
        handle_inner(&ctx, claimed).await.unwrap();

        let rows = delivery_rows(&db, monitor_model.id).await;
        assert_eq!(
            rows.len(),
            1,
            "the pre-created row, updated in place — no second row"
        );
        assert_eq!(rows[0].id, delivery_id);
        assert_eq!(rows[0].status, "skipped");
        assert!(rows[0].finished_at.is_some());
        assert!(rows[0].lease_expires_at.is_none());
        assert!(rows[0].worker_id.is_none());
    }

    /// The conveyor-enabled path end to end: a real `Conveyor` with `SendNotificationHandler`, a
    /// real enqueue, BLPOP pickup and delivery, proving registration, dispatch and payload shape
    /// compose (other tests call `handle_inner` directly).
    #[tokio::test]
    async fn conveyor_dispatch_delivers_and_updates_the_pre_created_row() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let redis_url = std::env::var("TEST_REDIS_URL").unwrap();

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-conveyor-dispatch-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;
        let ctx = Arc::new(test_ctx(db.clone(), redis.clone()));

        // destination_id = -1 with no mailer reports `Skipped`; this test is about dispatch, not
        // send outcomes.
        let delivery_id = insert_pending(&db, org.id, monitor_model.id, None, -1).await;

        let conveyor = arcodash_conveyor::ConveyorBuilder::new(db.clone(), redis, redis_url)
            .register(
                "send_notification",
                SendNotificationHandler { ctx: ctx.clone() },
            )
            .tier("notifications", &["send_notification"], 4)
            .build();

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let run_handle = {
            let conveyor = conveyor.clone();
            tokio::spawn(async move { conveyor.run(shutdown_rx).await })
        };

        let payload = serde_json::to_value(SendNotificationPayload {
            org_id: org.id,
            monitor_id: monitor_model.id,
            destination_id: -1,
            new_state: "triggered".to_string(),
            subscriber_id: user_id,
            subscription_id: None,
            attempt: 1,
            observed: None,
            explanation: None,
        })
        .unwrap();

        conveyor
            .enqueue(arcodash_conveyor::EnqueueRequest {
                task_type: "send_notification".into(),
                payload,
                tenant_id: Some(org.id),
                entity_id: Some(delivery_id),
                resource_key: None,
                resource_cap: None,
            })
            .await
            .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let row = loop {
            let row = delivery_row(&db, delivery_id).await;
            if row.finished_at.is_some() {
                break row;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "conveyor never delivered the task within 5s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };

        let _ = shutdown_tx.send(true);
        run_handle.await.unwrap();

        assert_eq!(row.id, delivery_id, "the pre-created row, updated in place");
        assert_eq!(row.status, "skipped");
        assert!(row.finished_at.is_some());
    }

    /// Unknown delivery id (e.g. a `runs` id): must still send AND write a fresh fallback row,
    /// never drop the notification.
    #[tokio::test]
    async fn nonexistent_delivery_id_falls_back_to_a_fresh_insert() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-nonexistent-delivery-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;
        let ctx = test_ctx(db.clone(), redis);

        let unknown = SendNotificationParams {
            delivery_id: Some(999_999_999),
            ..job(org.id, monitor_model.id, -1, user_id)
        };
        let result = handle_inner(&ctx, unknown).await.unwrap();
        assert!(result, "the fallback path must still reach the notify path");

        let rows = delivery_rows(&db, monitor_model.id).await;
        assert_eq!(
            rows.len(),
            1,
            "a nonexistent delivery id must fall back to a fresh insert, not silently drop the \
             notification"
        );
        assert_eq!(rows[0].status, "skipped");
        assert_ne!(
            rows[0].id, 999_999_999,
            "sanity: the fresh row must not accidentally reuse the nonexistent id"
        );
    }

    /// Fan-out concurrency: two pre-created `pending` rows for the same monitor, sent concurrently
    /// via `tokio::join!`; the per-row CAS claim means neither is dropped by the other.
    #[tokio::test]
    async fn two_concurrent_sends_for_different_subscribers_do_not_block() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-fanout-concurrent-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;
        let ctx = test_ctx(db.clone(), redis);

        let delivery_a = insert_pending(&db, org.id, monitor_model.id, None, -1).await;
        let delivery_b = insert_pending(&db, org.id, monitor_model.id, None, -1).await;
        let claimed = |delivery_id| SendNotificationParams {
            delivery_id: Some(delivery_id),
            ..job(org.id, monitor_model.id, -1, user_id)
        };

        let (r1, r2) = tokio::join!(
            handle_inner(&ctx, claimed(delivery_a)),
            handle_inner(&ctx, claimed(delivery_b)),
        );
        assert!(r1.unwrap());
        assert!(r2.unwrap());

        let row_a = delivery_row(&db, delivery_a).await;
        let row_b = delivery_row(&db, delivery_b).await;
        assert_eq!(
            row_a.status, "skipped",
            "neither concurrent claim may be dropped"
        );
        assert_eq!(
            row_b.status, "skipped",
            "neither concurrent claim may be dropped"
        );
    }
}

#[cfg(test)]
mod delivery_failed_audit_event_tests {
    //! A terminally-failed delivery writes exactly one `notification_delivery_failed` audit event,
    //! never for `sent` or `retrying`. Uses a real Mailpit send and a real connection-refused
    //! webhook failure on one monitor; needs Postgres + Redis + Mailpit.
    use super::fixtures::*;
    use super::*;

    async fn failed_event_count(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
        monitor_id: i64,
    ) -> usize {
        event::Entity::find()
            .filter(event::Column::OrgId.eq(org_id))
            .filter(event::Column::Action.eq("notification_delivery_failed"))
            .filter(event::Column::ObjectType.eq("monitor"))
            .filter(event::Column::ObjectId.eq(monitor_id))
            .all(db)
            .await
            .unwrap()
            .len()
    }

    #[tokio::test]
    async fn one_event_per_exhausted_delivery_none_for_sent_or_retrying() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        if Mailpit::connect().await.is_none() {
            return;
        }

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-delivery-failed-event-{suffix}")).await;
        let (monitor_model, user_id) = seed_org_with_monitor(&db, org.id).await;
        let opts = serde_json::json!({ "url": "http://127.0.0.1:1/hooks/audit-event-test" });
        let dest = seed_dest(&db, org.id, "webhook", opts).await;
        let ctx = ctx_with_mailer(&db, redis, Mailpit::mailer());

        assert_eq!(
            failed_event_count(&db, org.id, monitor_model.id).await,
            0,
            "no delivery-failed event before any attempt"
        );

        // Attempt 1 of a transient webhook failure is "retrying", not terminal: no event yet.
        std::env::set_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV, "1");
        let webhook_job = || job(org.id, monitor_model.id, dest.id, user_id);
        handle_inner(&ctx, webhook_job()).await.unwrap();
        assert_eq!(
            failed_event_count(&db, org.id, monitor_model.id).await,
            0,
            "a non-terminal 'retrying' outcome must not write a delivery-failed event"
        );

        // Attempt MAX_ATTEMPTS: exhausted, terminal "failed" — must write exactly one event.
        let exhausted = SendNotificationParams {
            attempt: MAX_ATTEMPTS,
            ..webhook_job()
        };
        handle_inner(&ctx, exhausted).await.unwrap();
        std::env::remove_var(arcodash_core::net_guard::ALLOW_PRIVATE_ENV);
        assert_eq!(
            failed_event_count(&db, org.id, monitor_model.id).await,
            1,
            "an exhausted, terminally-failed delivery must write exactly one event"
        );

        // A real Mailpit send on the SAME monitor — must not add another event.
        handle_inner(&ctx, job(org.id, monitor_model.id, -1, user_id))
            .await
            .unwrap();
        assert_eq!(
            failed_event_count(&db, org.id, monitor_model.id).await,
            1,
            "a sent delivery must never add a delivery-failed event"
        );

        let rows = delivery_rows(&db, monitor_model.id).await;
        assert_eq!(
            rows.len(),
            3,
            "one row per handle_inner call: retrying, failed, sent"
        );
        let sent_row = rows
            .iter()
            .find(|r| r.status == "sent")
            .expect("the mailpit send recorded a 'sent' row");
        assert!(sent_row.error.is_none());
        let failed_row = rows
            .iter()
            .find(|r| r.status == "failed")
            .expect("the exhausted webhook attempt recorded a 'failed' row");
        assert!(failed_row.error.is_some());
    }
}

#[cfg(test)]
mod subscription_authz_tests {
    //! The send-time re-check that stops a stale subscription emailing a revoked user: a
    //! subscription-driven delivery is skipped once the subscriber loses access, while escalation
    //! sends (`subscription_id: None`) are untouched. Real Postgres + Redis only.
    use super::fixtures::*;
    use super::*;
    use arcodash_core::org_seed::seed_builtin_groups;

    /// A user in the builtin `default` group (real `view_query`/`view_monitor` capability) who owns
    /// nothing; each test varies only the reach half of `can_view_monitor`.
    async fn seed_capable_non_owner(db: &sea_orm::DatabaseConnection, org_id: i64) -> user::Model {
        let groups = seed_builtin_groups(db, org_id).await.unwrap();
        seed_user(db, org_id, vec![groups.default_group_id]).await
    }

    /// A monitor owned by a throwaway user (in no groups) on a draft query; a non-owner is denied
    /// either way for `restricted` visibility.
    async fn seed_restricted_monitor_owned_by_someone_else(
        db: &sea_orm::DatabaseConnection,
        org_id: i64,
    ) -> monitor::Model {
        let ds = seed_data_source(db, org_id).await;
        let q = seed_query(db, org_id, ds.id).await;
        monitor::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("subauthz-monitor-{}", uuid::Uuid::new_v4())),
            query_id: Set(q.id),
            user_id: Set(q.user_id),
            options: Set(serde_json::json!({})),
            state: Set("unknown".to_string()),
            visibility: Set("restricted".to_string()),
            last_triggered_at: Set(None),
            rearm: Set(None),
            created_at: Set(now()),
            updated_at: Set(now()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    /// Published + workspace-visible: the org-open default, cleared on both gates by a capable
    /// subscriber's capability and data-source reach.
    async fn seed_open_monitor(db: &sea_orm::DatabaseConnection, org_id: i64) -> monitor::Model {
        let ds = seed_data_source(db, org_id).await;
        let q = seed_published_query(db, org_id, ds.id).await;
        crate::scheduler::test_support::seed_monitor(
            db,
            org_id,
            q.id,
            q.user_id,
            serde_json::json!({}),
        )
        .await
    }

    #[tokio::test]
    async fn a_subscription_the_user_can_no_longer_view_is_skipped_not_sent() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-subauthz-skip-{suffix}")).await;
        let subscriber = seed_capable_non_owner(&db, org.id).await;
        // The subscriber has real capability but no reach: not the owner, no explicit grant,
        // restricted.
        let monitor_model = seed_restricted_monitor_owned_by_someone_else(&db, org.id).await;
        let sub = subscribe(&db, monitor_model.id, subscriber.id, None).await;
        let delivery_id = insert_pending(&db, org.id, monitor_model.id, Some(sub.id), -1).await;

        // If the gate failed and a send were attempted it would fail loudly, proving nothing was
        // sent.
        let ctx = ctx_with_mailer(&db, redis, closed_port_mailer());
        let revoked = SendNotificationParams {
            subscription_id: Some(sub.id),
            delivery_id: Some(delivery_id),
            ..job(org.id, monitor_model.id, -1, subscriber.id)
        };
        let reached_send = handle_inner(&ctx, revoked).await.unwrap();
        assert!(
            !reached_send,
            "a revoked subscriber must never reach a send attempt"
        );

        let row = delivery_row(&db, delivery_id).await;
        assert_eq!(row.status, "skipped");
        assert_eq!(
            row.error.as_deref(),
            Some("subscriber no longer has access to this monitor")
        );
        assert_eq!(row.subscription_id, Some(sub.id));
    }

    /// Guards the `subscription_id.is_some()` gate: without it the access re-check would silently
    /// break escalations.
    #[tokio::test]
    async fn an_escalation_send_is_not_gated_on_the_subscriber() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        if Mailpit::connect().await.is_none() {
            return;
        }

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-subauthz-escalation-{suffix}")).await;
        // The owner is in no groups: `can_view_monitor` would deny them if an escalation send were
        // wrongly gated on it.
        let monitor_model = seed_restricted_monitor_owned_by_someone_else(&db, org.id).await;
        let owner_id = monitor_model.user_id;
        let ctx = ctx_with_mailer(&db, redis, Mailpit::mailer());

        // destination_id = -1, a real Mailpit send; `subscription_id: None` is the escalation
        // shape.
        let escalation = job(org.id, monitor_model.id, -1, owner_id);
        let reached_send = handle_inner(&ctx, escalation).await.unwrap();
        assert!(
            reached_send,
            "an escalation send must reach the notify path"
        );

        let rows = delivery_rows(&db, monitor_model.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].status, "sent",
            "an escalation (subscription_id: None) must never be gated on the subscriber's \
             monitor access"
        );
    }

    /// Positive control: a subscriber who still has access is unaffected.
    #[tokio::test]
    async fn a_still_authorized_subscriber_is_unaffected() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        if Mailpit::connect().await.is_none() {
            return;
        }

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-subauthz-ok-{suffix}")).await;
        let subscriber = seed_capable_non_owner(&db, org.id).await;
        let monitor_model = seed_open_monitor(&db, org.id).await;
        let sub = subscribe(&db, monitor_model.id, subscriber.id, None).await;
        let ctx = ctx_with_mailer(&db, redis, Mailpit::mailer());

        let authorized = SendNotificationParams {
            subscription_id: Some(sub.id),
            ..job(org.id, monitor_model.id, -1, subscriber.id)
        };
        let reached_send = handle_inner(&ctx, authorized).await.unwrap();
        assert!(
            reached_send,
            "a still-authorized subscriber's send must not be blocked"
        );

        let rows = delivery_rows(&db, monitor_model.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "sent");
    }

    /// The disabled/offboarded half of the access gate: this subscriber has capability and reach,
    /// so only the account-state guard inside `can_view_monitor` can deny them.
    #[tokio::test]
    async fn a_disabled_subscriber_gets_no_alert() {
        let Some((db, redis)) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let org = seed_org(&db, &format!("org-subauthz-disabled-{suffix}")).await;
        let subscriber = seed_capable_non_owner(&db, org.id).await;
        let monitor_model = seed_open_monitor(&db, org.id).await;
        let sub = subscribe(&db, monitor_model.id, subscriber.id, None).await;

        let mut active: user::ActiveModel = subscriber.clone().into();
        active.disabled_at = Set(Some(now()));
        active.update(&db).await.unwrap();

        let delivery_id = insert_pending(&db, org.id, monitor_model.id, Some(sub.id), -1).await;

        // If the gate failed and a send were attempted it would fail loudly, proving nothing was
        // sent.
        let ctx = ctx_with_mailer(&db, redis, closed_port_mailer());
        let disabled = SendNotificationParams {
            subscription_id: Some(sub.id),
            delivery_id: Some(delivery_id),
            ..job(org.id, monitor_model.id, -1, subscriber.id)
        };
        let reached_send = handle_inner(&ctx, disabled).await.unwrap();
        assert!(
            !reached_send,
            "a disabled subscriber must never reach a send attempt"
        );

        let row = delivery_row(&db, delivery_id).await;
        assert_eq!(row.status, "skipped");
        assert_eq!(
            row.error.as_deref(),
            Some("subscriber no longer has access to this monitor"),
            "a disabled account is denied through the same access gate, not a distinct reason"
        );
    }
}

#[cfg(test)]
mod discord_live_tests {
    use super::*;
    use sea_orm::Database;

    fn sample_alert(monitor_url: Option<String>) -> ChatAlert {
        ChatAlert {
            monitor_name: "Orders total".into(),
            monitor_url,
            state: "triggered".into(),
            observed: Some("14,200".into()),
            condition: "> 10,000".into(),
            reason: None,
            explanation: Some(
                "Jumped to 14,200 from a 30-check median of 9,850 (z 4.7). Last 3 checks: 9,900, 9,780, 14,200.\n\nAI analysis (confidence: high): The jump comes from the b2b segment, up from 1,200 to 5,600 orders; b2c and smb are flat.".into(),
            ),
            custom_subject: None,
            custom_body: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn chat_destinations_get_their_card_shape() {
        let a = sample_alert(Some("http://localhost:5173/monitors/42".into()));
        let opts = serde_json::json!({ "icon_emoji": ":bell:", "username": "ops" });
        let build = |t: &str, u: &str| build_chat_body(t, u, &opts, &a, a.explanation.as_deref());

        let slack = build("slack", "https://hooks.slack.com/services/x");
        assert_eq!(slack["attachments"][0]["title"], "Orders total is alerting");
        assert_eq!(slack["attachments"][0]["fields"][0]["value"], "14,200");
        assert!(slack.get("icon_emoji").is_none());

        let mm = build("mattermost", "https://mm.example.test/hook");
        assert_eq!(
            mm["attachments"][0]["title_link"],
            "http://localhost:5173/monitors/42"
        );
        assert_eq!(
            (mm["icon_emoji"].as_str(), mm["username"].as_str()),
            (Some(":bell:"), Some("ops"))
        );

        let native = build("discord", "https://discord.com/api/webhooks/1/tok");
        assert_eq!(native["embeds"][0]["title"], "Orders total is alerting");
        let compat = build("discord", "https://discord.com/api/webhooks/1/tok/slack");
        assert_eq!(
            compat["attachments"][0]["fallback"],
            "Alerting: Orders total"
        );
        assert_eq!(compat["allowed_mentions"]["parse"], serde_json::json!([]));

        let teams = build("microsoft_teams_webhook", "https://x.webhook.office.com/y");
        assert_eq!(teams["type"], "message");
        assert_eq!(teams["attachments"][0]["content"]["version"], "1.4");

        let gchat = build("google_chat", "https://chat.googleapis.com/v1/spaces/x");
        assert!(gchat["text"]
            .as_str()
            .unwrap()
            .contains("*Observed:* 14,200"));
    }

    #[test]
    fn generic_webhook_payload_is_the_flat_status_text() {
        let a = sample_alert(None);
        let body = build_chat_body(
            "webhook",
            "https://example.test/h",
            &serde_json::json!({}),
            &a,
            a.explanation.as_deref(),
        );
        assert_eq!(
            body["text"].as_str().unwrap(),
            format!(
                "Monitor 'Orders total' is triggered\n\n{}",
                a.explanation.as_deref().unwrap()
            )
        );
        assert!(body.get("attachments").is_none());
    }

    #[tokio::test]
    #[ignore = "sends a real message to a live Discord destination"]
    async fn discord_live_dev_destination() {
        let (dest_type, opts) = if let Ok(url) = std::env::var("LIVE_WEBHOOK_URL") {
            let t = std::env::var("LIVE_WEBHOOK_TYPE").unwrap_or_else(|_| "discord".into());
            (t, serde_json::json!({ "url": url }))
        } else {
            let id: i64 = std::env::var("DISCORD_LIVE_DEST_ID")
                .expect("DISCORD_LIVE_DEST_ID unset")
                .parse()
                .unwrap();
            let db = Database::connect(
                std::env::var("DISCORD_LIVE_DB_URL").expect("DISCORD_LIVE_DB_URL unset"),
            )
            .await
            .unwrap();
            let dest = notification_destination::Entity::find_by_id(id)
                .one(&db)
                .await
                .unwrap()
                .expect("destination not found");
            let key = secrets::parse_key_hex(
                &std::env::var("ENCRYPTION_KEY").expect("ENCRYPTION_KEY unset"),
            )
            .unwrap();
            let opts = decrypt_options(&key, dest.options.as_deref()).unwrap();
            (dest.r#type, opts)
        };
        let url = opts.get("url").and_then(|v| v.as_str()).unwrap_or_default();
        let slack = chat_alert_body(&dest_type, url, &sample_alert(None))
            .get("embeds")
            .is_none();
        let alert = sample_alert(Some("http://localhost:5173/monitors/42".into()));
        let out = send_to_destination(
            42,
            "Orders total",
            "triggered",
            &dest_type,
            &opts,
            alert.explanation.as_deref(),
            &alert,
        )
        .await;
        let ok = out.status == Outcome::sent().status;
        println!(
            "ok={ok} url_shape={} status={}",
            if slack { "slack" } else { "native" },
            out.status
        );
        assert!(ok, "send failed (details redacted)");
    }
}
