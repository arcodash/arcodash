//! `SendDigest` handler: builds and sends one user's daily digest of the last 24h of monitor state
//! changes plus still-`triggered` monitors. No empty digests: nothing to report means no email.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};

use arcodash_core::ai::narrative;
use arcodash_core::ai::settings::{
    dashboard_narratives, fallback_enabled, include_sample_values, resolve_provider_chain,
};

use arcodash_conveyor::{TaskContext, TaskOutcome, TaskResult, TypedTaskHandler};
use arcodash_core::email_template::{render, EmailLayout};
use arcodash_core::entities::{
    dashboard, event, favorite, monitor, monitor_subscription, organization, user,
};
use arcodash_core::mail_notify::SendDigestPayload;
use arcodash_core::mailer::MailOutcome;

use crate::ai_sink::DbAttemptSink;
use crate::handlers::execute_query::{send_html_email, ExecuteQueryContext};
use crate::handlers::send_mail::send_claimed;
use arcodash_core::chat_body::state_label;

/// Thin wrapper over [`deliver`] for the Redis-queue path; logs and swallows its error (a genuine
/// infra/claim failure).
pub async fn handle(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    user_id: i64,
    delivery_id: Option<i64>,
) {
    if let Err(e) = deliver(ctx, org_id, user_id, delivery_id).await {
        tracing::error!(user_id, error = %e, "send_digest failed");
    }
}

/// `delivery_id` is the pre-created `mail_deliveries` row, claimed so the outcome finalizes it in
/// place; `None` sends directly with no claim/finalize. Returns `Err` only for a genuine claim/DB
/// failure; duplicates, empty digests and failed sends are `Ok(())`. An empty-digest no-op reports
/// `Sent` (nothing to send, delivery completed).
pub async fn deliver(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    user_id: i64,
    delivery_id: Option<i64>,
) -> anyhow::Result<()> {
    let Some(id) = delivery_id else {
        handle_inner(ctx, org_id, user_id).await?;
        return Ok(());
    };
    if let Some(Err(e)) = send_claimed(ctx, id, handle_inner(ctx, org_id, user_id)).await? {
        tracing::error!(user_id, error = %e, "send_digest failed");
    }
    Ok(())
}

/// Adapts `deliver` onto the conveyor SDK. `entity_id` is the `mail_deliveries` row; reports
/// `Success` unless `deliver` hit an internal error (the task tracks processing, not email
/// delivery).
pub struct SendDigestHandler {
    pub ctx: Arc<ExecuteQueryContext>,
}

#[async_trait]
impl TypedTaskHandler for SendDigestHandler {
    type Payload = SendDigestPayload;

    async fn run(&self, task: TaskContext, p: SendDigestPayload) -> TaskResult {
        Ok(
            match deliver(&self.ctx, p.org_id, p.user_id, task.entity_id).await {
                Ok(()) => TaskOutcome::Success { result_ref: None },
                Err(e) => TaskOutcome::Failed {
                    error: format!("{e:#}"),
                },
            },
        )
    }
}

async fn handle_inner(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    user_id: i64,
) -> anyhow::Result<MailOutcome> {
    // Sampled once and used for both the content bound and the watermark write-back, so a state
    // change can't fall in the gap between two `now()` calls.
    let now = Utc::now();
    let Some(u) = user::Entity::find_by_id(user_id)
        .one(&ctx.db)
        .await?
        .filter(|u| u.org_id == org_id)
    else {
        tracing::warn!(user_id, "send_digest: user not found");
        return Ok(MailOutcome::Sent);
    };

    if !u.digest_enabled {
        tracing::info!(user_id, "send_digest: digest no longer enabled, skipping");
        return Ok(MailOutcome::Sent);
    }

    if !u.is_active() {
        // Backstop for a task enqueued before the recipient was disabled or revoked (fan-out runs
        // hourly).
        tracing::info!(
            user_id,
            "send_digest: recipient is disabled or pending, skipping"
        );
        return Ok(MailOutcome::Sent);
    }

    let Some(token) = u.digest_unsubscribe_token.as_deref() else {
        // Should not happen, but never send an email with no working unsubscribe link.
        tracing::warn!(
            user_id,
            "send_digest: digest enabled but no unsubscribe token, skipping"
        );
        return Ok(MailOutcome::Sent);
    };

    let dashboards = dashboard_lines(ctx, org_id, &u).await;
    let Some((subject, text, html)) =
        build_digest_content_with(&ctx.db, org_id, &u, token, &ctx.base_url, now, &dashboards)
            .await?
    else {
        return Ok(MailOutcome::Sent);
    };

    match send_html_email(
        &ctx.mailer,
        &ctx.mail_from,
        &u.email,
        &subject,
        &text,
        &html,
    )
    .await
    {
        Ok(MailOutcome::Sent) => {
            if let Err(e) = user::Entity::update_many()
                .col_expr(
                    user::Column::DigestLastSentAt,
                    sea_orm::sea_query::Expr::value(now),
                )
                .filter(user::Column::Id.eq(u.id))
                .filter(
                    Condition::any()
                        .add(user::Column::DigestLastSentAt.is_null())
                        .add(user::Column::DigestLastSentAt.lt(now)),
                )
                .exec(&ctx.db)
                .await
            {
                tracing::error!(user_id, error = %e, "send_digest: failed to advance digest_last_sent_at watermark");
            }
            Ok(MailOutcome::Sent)
        }
        Ok(MailOutcome::Skipped) => Ok(MailOutcome::Skipped),
        Err(e) => {
            // A terminal send failure must release the per-day guard so the next hourly tick can
            // retry, rather than skipping the day.
            let local_date = arcodash_core::digest_schedule::user_local_date(
                u.digest_timezone.as_deref(),
                Utc::now(),
            );
            crate::scheduler::release_digest_send_guard(&ctx.redis, user_id, local_date).await;
            Err(e)
        }
    }
}

struct ChangeLine {
    monitor_id: i64,
    monitor_name: String,
    old_state: String,
    new_state: String,
    at: chrono::DateTime<Utc>,
}

struct TriggeredLine {
    monitor_name: String,
    since: chrono::DateTime<Utc>,
    acked: bool,
    silenced: bool,
}

/// Digest window start: since the last confirmed send, capped at 24h so a dormant user doesn't get
/// a full backlog.
fn digest_window_start(
    last_sent: Option<chrono::DateTime<Utc>>,
    now: chrono::DateTime<Utc>,
) -> chrono::DateTime<Utc> {
    let cutoff = now - Duration::hours(24);
    last_sent.map_or(cutoff, |t| t.max(cutoff))
}

/// Comfortably below the daily cadence and far above the gap between duplicate invocations; a
/// strict 24h would drop reminders when sends land slightly under a day apart (early tick, DST).
const TRIGGERED_REMINDER_INTERVAL: Duration = Duration::hours(20);

/// A still-triggered monitor appears under "Currently Alerting" only if the last confirmed digest
/// is old enough for a repeat to be a reminder.
fn should_remind_triggered(
    last_sent: Option<chrono::DateTime<Utc>>,
    now: chrono::DateTime<Utc>,
) -> bool {
    last_sent.is_none_or(|t| now - t >= TRIGGERED_REMINDER_INTERVAL)
}

struct DashboardLine {
    name: String,
    slug: String,
    summary: String,
}

const MAX_DASHBOARD_BRIEFS: usize = 3;
const DASHBOARD_SECTION_BUDGET: std::time::Duration = std::time::Duration::from_secs(75);

/// AI briefs for the user's newest favorite dashboards they can still view; any failure skips that
/// dashboard, never the digest.
async fn dashboard_lines(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    user: &user::Model,
) -> Vec<DashboardLine> {
    let mut lines = Vec::new();
    let _ = tokio::time::timeout(
        DASHBOARD_SECTION_BUDGET,
        collect_dashboard_lines(ctx, org_id, user, &mut lines),
    )
    .await;
    lines
}

async fn collect_dashboard_lines(
    ctx: &ExecuteQueryContext,
    org_id: i64,
    user: &user::Model,
    lines: &mut Vec<DashboardLine>,
) {
    let Some(ai) = ctx.ai.as_ref() else { return };
    let Ok(Some(org)) = organization::Entity::find_by_id(org_id).one(&ctx.db).await else {
        return;
    };
    if !ctx.entitlements.ai_features(&org)
        || !dashboard_narratives(&org.settings)
        || !include_sample_values(&org.settings)
    {
        return;
    }
    let Ok(chain) = resolve_provider_chain(&org, &ai.config) else {
        return;
    };
    if chain.is_empty() {
        return;
    }

    let Ok(favorites) = favorite::Entity::find()
        .filter(favorite::Column::UserId.eq(user.id))
        .filter(favorite::Column::ObjectType.eq("dashboard"))
        .filter(favorite::Column::OrgId.eq(org_id))
        .order_by_desc(favorite::Column::CreatedAt)
        .order_by_desc(favorite::Column::Id)
        .all(&ctx.db)
        .await
    else {
        return;
    };

    let mut taken = 0;
    for fav in favorites {
        if taken == MAX_DASHBOARD_BRIEFS {
            break;
        }
        let Ok(Some(d)) = dashboard::Entity::find_by_id(fav.object_id)
            .one(&ctx.db)
            .await
        else {
            continue;
        };
        if d.org_id != org_id || d.is_archived {
            continue;
        }
        if !matches!(
            arcodash_core::authz::can_view_dashboard(&ctx.db, user, d.id).await,
            Ok(true)
        ) {
            continue;
        }
        taken += 1;
        match brief_for(ctx, &org, &chain, user, &d).await {
            Ok(summary) => lines.push(DashboardLine {
                name: d.name,
                slug: d.slug,
                summary,
            }),
            Err(e) => tracing::warn!(
                dashboard_id = d.id,
                user_id = user.id,
                error = %e,
                "send_digest: dashboard brief skipped"
            ),
        }
    }
}

async fn brief_for(
    ctx: &ExecuteQueryContext,
    org: &organization::Model,
    chain: &[arcodash_core::ai::settings::ResolvedProvider],
    user: &user::Model,
    d: &dashboard::Model,
) -> anyhow::Result<String> {
    let ai = ctx
        .ai
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("ai runtime absent"))?;
    let widgets = narrative::dashboard_widgets(&ctx.db, d).await?;
    let mut readable = HashSet::new();
    for w in &widgets {
        if !readable.contains(&w.query.id)
            && arcodash_core::authz::can_read_query_results(&ctx.db, user, w.query.id).await?
        {
            readable.insert(w.query.id);
        }
    }
    let current = narrative::build_digest(&ctx.db, org.id, &widgets, &readable).await?;
    if current.widgets.is_empty() {
        anyhow::bail!("no readable widgets");
    }
    let previous = narrative::previous_digest(&ctx.db, d.id, &current, &readable).await?;
    let prompt = narrative::build_prompt(&d.name, &current, previous.as_ref());
    let sink = DbAttemptSink::new(&ctx.db, org.clone(), Some(user.id));
    let (_, model, outcome) = narrative::generate(
        &ai.http,
        chain,
        fallback_enabled(&org.settings),
        ai.provider_lookup,
        &sink,
        "dashboard_brief",
        &prompt,
        &current,
    )
    .await;
    let n = outcome.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    narrative::store(
        &ctx.db,
        org.id,
        d.id,
        Some(user.id),
        "digest",
        &n,
        &current,
        &model,
    )
    .await?;
    Ok(n.summary)
}

/// Build the digest subject+body for `user`, or `None` if there's nothing to report. Factored out
/// for unit tests without a mailer.
async fn build_digest_content_with(
    db: &DatabaseConnection,
    org_id: i64,
    user: &user::Model,
    unsubscribe_token: &str,
    base_url: &str,
    now: chrono::DateTime<Utc>,
    dashboards: &[DashboardLine],
) -> anyhow::Result<Option<(String, String, String)>> {
    let subscribed_ids: HashSet<i64> = monitor_subscription::Entity::find()
        .filter(monitor_subscription::Column::UserId.eq(user.id))
        .all(db)
        .await?
        .into_iter()
        .map(|s| s.monitor_id)
        .collect();

    let subscribed = monitor::Entity::find()
        .filter(monitor::Column::Id.is_in(subscribed_ids))
        .filter(monitor::Column::OrgId.eq(org_id))
        .all(db)
        .await?;

    // A subscription outlives the user's access when grants change; re-check at send time.
    let mut monitors = Vec::with_capacity(subscribed.len());
    for m in subscribed {
        if arcodash_core::authz::can_view_monitor(db, user, m.id).await? {
            monitors.push(m);
        }
    }

    if monitors.is_empty() && dashboards.is_empty() {
        return Ok(None);
    }

    let names: HashMap<i64, String> = monitors.iter().map(|m| (m.id, m.name.clone())).collect();

    let since = digest_window_start(user.digest_last_sent_at.map(|t| t.with_timezone(&Utc)), now);
    let events = event::Entity::find()
        .filter(event::Column::Action.eq(arcodash_core::events::MONITOR_STATE_CHANGED))
        .filter(event::Column::ObjectType.eq("monitor"))
        .filter(event::Column::ObjectId.is_in(monitors.iter().map(|m| m.id)))
        .filter(event::Column::CreatedAt.gte(since))
        .filter(event::Column::CreatedAt.lt(now))
        .filter(event::Column::OrgId.eq(org_id))
        .order_by_asc(event::Column::CreatedAt)
        .all(db)
        .await?;

    let changes: Vec<ChangeLine> = events
        .iter()
        .filter_map(|ev| {
            let monitor_id = ev.object_id?;
            let monitor_name = names.get(&monitor_id)?.clone();
            let state = |key: &str| {
                ev.additional_properties
                    .get(key)
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string()
            };
            Some(ChangeLine {
                monitor_id,
                monitor_name,
                old_state: state("old_state"),
                new_state: state("new_state"),
                at: ev.created_at.with_timezone(&Utc),
            })
        })
        .collect();

    // A monitor already in "Changes" isn't repeated under "Currently Alerting".
    let changed_ids: HashSet<i64> = changes.iter().map(|c| c.monitor_id).collect();
    let triggered: Vec<TriggeredLine> = if should_remind_triggered(Some(since), now) {
        monitors
            .iter()
            .filter(|m| m.state == "triggered" && !changed_ids.contains(&m.id))
            .map(|m| TriggeredLine {
                monitor_name: m.name.clone(),
                since: m
                    .last_triggered_at
                    .map_or_else(Utc::now, |t| t.with_timezone(&Utc)),
                acked: m.acked_at.is_some(),
                silenced: arcodash_core::models::parse_monitor_options(m.id, &m.options)
                    .is_silenced(Utc::now()),
            })
            .collect()
    } else {
        Vec::new()
    };

    if changes.is_empty() && triggered.is_empty() && dashboards.is_empty() {
        return Ok(None);
    }

    Ok(Some(render_digest(
        &changes,
        &triggered,
        dashboards,
        base_url,
        unsubscribe_token,
    )))
}

/// Render the subject + both MIME parts for a non-empty digest.
fn render_digest(
    changes: &[ChangeLine],
    triggered: &[TriggeredLine],
    dashboards: &[DashboardLine],
    base_url: &str,
    unsubscribe_token: &str,
) -> (String, String, String) {
    let has_monitors = !changes.is_empty() || !triggered.is_empty();
    let k = dashboards.len();
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let subject = if has_monitors {
        let n = changes.len();
        let lead = if n > 0 {
            format!("arcodash digest — {n} monitor update{}", plural(n))
        } else {
            // No pluralization needed: "1 still Alerting" and "3 still Alerting" are both
            // grammatical.
            let m = triggered.len();
            format!("arcodash digest — {m} still {}", state_label("triggered"))
        };
        if k > 0 {
            format!("{lead} · {k} dashboard brief{}", plural(k))
        } else {
            lead
        }
    } else if k == 1 {
        format!("arcodash digest — {}", dashboards[0].name)
    } else {
        format!("arcodash digest — {k} dashboard briefs")
    };

    let changes_rows: Vec<(String, String)> = changes
        .iter()
        .map(|c| {
            (
                c.monitor_name.clone(),
                format!(
                    "{} → {} · {}",
                    state_label(&c.old_state),
                    state_label(&c.new_state),
                    c.at.format("%b %-d, %Y, %-I:%M %p UTC")
                ),
            )
        })
        .collect();

    let triggered_rows: Vec<(String, String)> = triggered
        .iter()
        .map(|t| {
            let mut parts = vec![format!(
                "Since {}",
                t.since.format("%b %-d, %Y, %-I:%M %p UTC")
            )];
            if t.acked {
                parts.push("Acknowledged".to_string());
            }
            if t.silenced {
                parts.push("Snoozed".to_string());
            }
            (t.monitor_name.clone(), parts.join(" · "))
        })
        .collect();

    let dashboard_rows: Vec<(String, String)> = dashboards
        .iter()
        .map(|d| (d.name.clone(), d.summary.clone()))
        .collect();

    let triggered_title = format!("Currently {}", state_label("triggered"));

    let mut sections: Vec<(&str, &[(String, String)])> = Vec::new();
    if !changes_rows.is_empty() {
        sections.push(("Changes", &changes_rows));
    }
    if !triggered_rows.is_empty() {
        sections.push((&triggered_title, &triggered_rows));
    }
    if !dashboard_rows.is_empty() {
        sections.push(("Dashboards", &dashboard_rows));
    }

    let (cta_label, cta_url) = if has_monitors {
        ("View your monitors", format!("{base_url}/monitors"))
    } else {
        ("View your dashboards", format!("{base_url}/dashboards"))
    };
    let open_labels: Vec<String> = dashboards
        .iter()
        .map(|d| format!("Open {}", d.name))
        .collect();
    let mut secondary: Vec<(&str, String)> = open_labels
        .iter()
        .zip(dashboards)
        .map(|(l, d)| (l.as_str(), format!("{base_url}/dashboards/{}", d.slug)))
        .collect();
    secondary.push((
        "Unsubscribe",
        format!("{base_url}/api/digest/unsubscribe?token={unsubscribe_token}"),
    ));

    let intro = match (has_monitors, k > 0) {
        (true, true) => "The last 24 hours for the monitors you follow, plus a brief on your favorite dashboards.",
        (false, _) => "What changed on your favorite dashboards.",
        (true, false) => "The last 24 hours for the monitors you follow.",
    };
    let footer = if k > 0 {
        "You're getting this because you turned on the daily digest in arcodash. Dashboard briefs are written by AI from each widget's latest results."
    } else {
        "You're getting this because you turned on the daily digest in arcodash."
    };

    let layout = EmailLayout {
        preheader: intro,
        heading: "arcodash digest",
        intro,
        facts: &[],
        sections: &sections,
        cta: Some((cta_label, &cta_url)),
        secondary: &secondary,
        footer,
    };
    let (text, html) = render(&layout);

    (subject, text, html)
}

#[cfg(test)]
mod tests {
    //! `build_digest_content` unit tests; need Postgres only (skipped without `TEST_DATABASE_URL`).
    use super::*;
    use crate::scheduler::test_support::exec_sql;
    use arcodash_core::entities::{organization, query};
    use sea_orm::ActiveModelTrait;
    use sea_orm::ActiveValue::Set;

    /// Also seeds the builtin groups and returns the `default` group's id (`can_view_monitor`
    /// denies users in no group).
    async fn seed_org(db: &DatabaseConnection, slug: &str) -> (organization::Model, i64) {
        let org = crate::scheduler::test_support::seed_org(db, slug).await;
        let groups = arcodash_core::org_seed::seed_builtin_groups(db, org.id)
            .await
            .unwrap();
        (org, groups.default_group_id)
    }

    async fn seed_user(db: &DatabaseConnection, org_id: i64, default_group_id: i64) -> user::Model {
        user::ActiveModel {
            digest_enabled: Set(true),
            digest_unsubscribe_token: Set(Some(uuid::Uuid::new_v4().to_string())),
            ..crate::scheduler::test_support::new_user(org_id, vec![default_group_id])
        }
        .insert(db)
        .await
        .unwrap()
    }

    /// `owner_id` is the subscriber: ownership clears the reach half of `can_view_monitor`, leaving
    /// the capability half to vary.
    #[allow(clippy::too_many_arguments)]
    async fn seed_monitor(
        db: &DatabaseConnection,
        org_id: i64,
        owner_id: i64,
        state: &str,
        last_triggered_at: Option<chrono::DateTime<Utc>>,
    ) -> monitor::Model {
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(Utc::now());
        let suffix = uuid::Uuid::new_v4();

        let ds = crate::scheduler::test_support::seed_data_source(db, org_id).await;

        let q = query::ActiveModel {
            org_id: Set(org_id),
            data_source_id: Set(ds.id),
            name: Set(format!("digest-test-query-{suffix}")),
            description: Set(None),
            query: Set("SELECT 1".to_string()),
            query_hash: Set(format!("hash-{suffix}")),
            api_key: Set(suffix.to_string()),
            user_id: Set(owner_id),
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

        monitor::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("digest-test-monitor-{suffix}")),
            query_id: Set(q.id),
            user_id: Set(owner_id),
            options: Set(serde_json::json!({})),
            state: Set(state.to_string()),
            last_triggered_at: Set(last_triggered_at.map(Into::into)),
            rearm: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    /// An `ok` monitor on a published query owned by someone else: workspace-visible until a test
    /// flips it to `restricted`.
    async fn seed_monitor_owned_by_someone_else(
        db: &DatabaseConnection,
        org_id: i64,
    ) -> monitor::Model {
        let ds = crate::scheduler::test_support::seed_data_source(db, org_id).await;
        let q = crate::scheduler::test_support::seed_published_query(db, org_id, ds.id).await;
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(Utc::now());
        monitor::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!(
                "digest-other-owner-monitor-{}",
                uuid::Uuid::new_v4()
            )),
            query_id: Set(q.id),
            user_id: Set(q.user_id),
            options: Set(serde_json::json!({})),
            state: Set("ok".to_string()),
            last_triggered_at: Set(None),
            rearm: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    async fn subscribe(db: &DatabaseConnection, monitor_id: i64, user_id: i64) {
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(Utc::now());
        monitor_subscription::ActiveModel {
            monitor_id: Set(monitor_id),
            user_id: Set(user_id),
            destination_id: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn seed_state_change_event(
        db: &DatabaseConnection,
        org_id: i64,
        monitor_id: i64,
        old_state: &str,
        new_state: &str,
        created_at: chrono::DateTime<Utc>,
    ) {
        event::ActiveModel {
            org_id: Set(org_id),
            user_id: Set(None),
            action: Set(arcodash_core::events::MONITOR_STATE_CHANGED.to_string()),
            object_type: Set(Some("monitor".to_string())),
            object_id: Set(Some(monitor_id)),
            additional_properties: Set(
                serde_json::json!({ "old_state": old_state, "new_state": new_state }),
            ),
            created_at: Set(created_at.into()),
            updated_at: Set(created_at.into()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    /// A real SMTP transport on a closed local port: any send attempt fails.
    fn closed_port_mailer_ctx(
        db: DatabaseConnection,
        redis: redis::aio::ConnectionManager,
    ) -> ExecuteQueryContext {
        let bad_mailer =
            lettre::AsyncSmtpTransport::<lettre::Tokio1Executor>::builder_dangerous("127.0.0.1")
                .port(1)
                .build();
        let mut ctx = crate::scheduler::test_support::test_ctx(db, redis);
        ctx.mailer = Some(Arc::new(bad_mailer));
        ctx
    }

    async fn setup() -> Option<DatabaseConnection> {
        let database_url = std::env::var("TEST_DATABASE_URL").ok()?;
        Some(arcodash_core::db::connect(&database_url).await.unwrap())
    }

    async fn build_digest_content(
        db: &DatabaseConnection,
        org_id: i64,
        user: &user::Model,
    ) -> anyhow::Result<Option<(String, String, String)>> {
        let base = "http://localhost:3000";
        build_digest_content_with(db, org_id, user, "tok", base, Utc::now(), &[]).await
    }

    #[tokio::test]
    async fn event_older_than_24h_is_excluded() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-window-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;
        let m = seed_monitor(&db, org.id, u.id, "ok", None).await;
        subscribe(&db, m.id, u.id).await;

        seed_state_change_event(
            &db,
            org.id,
            m.id,
            "ok",
            "triggered",
            Utc::now() - Duration::hours(25),
        )
        .await;

        let result = build_digest_content(&db, org.id, &u).await.unwrap();
        assert!(
            result.is_none(),
            "an event older than 24h + no currently-triggered monitor must yield no digest"
        );
    }

    #[tokio::test]
    async fn event_within_24h_is_included() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-window2-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;
        let m = seed_monitor(&db, org.id, u.id, "ok", None).await;
        subscribe(&db, m.id, u.id).await;

        seed_state_change_event(
            &db,
            org.id,
            m.id,
            "ok",
            "triggered",
            Utc::now() - Duration::hours(1),
        )
        .await;

        let (subject, text, html) = build_digest_content(&db, org.id, &u)
            .await
            .unwrap()
            .expect("a recent change must produce a digest");
        assert!(subject.contains("1 monitor update"));
        assert!(text.contains(&m.name));
        assert!(
            text.contains("OK → Alerting"),
            "must use the shared state vocabulary, not the raw DB string: {text}"
        );
        assert!(html.contains(&m.name));
        assert!(
            html.contains("Changes"),
            "html must carry the section title: {html}"
        );
    }

    /// Guards the writer/reader coupling on `MONITOR_STATE_CHANGED`: the "Changes" section must
    /// surface an event stored under the writer's constant.
    #[tokio::test]
    async fn monitor_state_changed_event_surfaces_in_the_changes_section() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-mscguard-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;
        let m = seed_monitor(&db, org.id, u.id, "ok", None).await;
        subscribe(&db, m.id, u.id).await;

        seed_state_change_event(
            &db,
            org.id,
            m.id,
            "ok",
            "triggered",
            Utc::now() - Duration::hours(1),
        )
        .await;

        let (_, text, html) = build_digest_content(&db, org.id, &u)
            .await
            .unwrap()
            .expect("a recent monitor_state_changed event must produce a digest");
        assert!(text.contains(&m.name));
        assert!(
            html.contains("Changes"),
            "html must carry the Changes section: {html}"
        );
    }

    /// A watermark after the event means it was already reported; with no watermark it is within
    /// the 24h floor and reported.
    #[tokio::test]
    async fn a_watermark_past_the_event_suppresses_it_but_no_watermark_includes_it() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-watermark-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;
        let m = seed_monitor(&db, org.id, u.id, "ok", None).await;
        subscribe(&db, m.id, u.id).await;

        let event_at = Utc::now() - Duration::hours(1);
        seed_state_change_event(&db, org.id, m.id, "ok", "triggered", event_at).await;

        let before = build_digest_content(&db, org.id, &u).await.unwrap();
        assert!(
            before.is_some(),
            "with no watermark, a recent event must still be reported"
        );

        let mut active: user::ActiveModel = u.clone().into();
        active.digest_last_sent_at = Set(Some((event_at + Duration::minutes(1)).into()));
        let u_with_watermark = active.update(&db).await.unwrap();

        let after = build_digest_content(&db, org.id, &u_with_watermark)
            .await
            .unwrap();
        assert!(
            after.is_none(),
            "a watermark past the event's timestamp must suppress it from the next digest"
        );
    }

    #[tokio::test]
    async fn no_changes_and_no_triggered_sends_nothing() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-empty-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;
        let m = seed_monitor(&db, org.id, u.id, "ok", None).await;
        subscribe(&db, m.id, u.id).await;

        let result = build_digest_content(&db, org.id, &u).await.unwrap();
        assert!(
            result.is_none(),
            "no-empty-digest rule: nothing to report -> no email"
        );
    }

    #[tokio::test]
    async fn currently_triggered_only_lists_subscribed_monitors() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-triggered-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;

        let subscribed = seed_monitor(&db, org.id, u.id, "triggered", Some(Utc::now())).await;
        subscribe(&db, subscribed.id, u.id).await;

        let _not_subscribed = seed_monitor(&db, org.id, u.id, "triggered", Some(Utc::now())).await;

        let (subject, text, html) = build_digest_content(&db, org.id, &u)
            .await
            .unwrap()
            .expect("a subscribed triggered monitor must produce a digest");
        assert!(
            subject.contains("1 still Alerting"),
            "must use the shared state vocabulary, not the raw DB string: {subject}"
        );
        assert!(text.contains(&subscribed.name));
        assert!(!text.contains(&_not_subscribed.name));
        assert!(!html.contains(&_not_subscribed.name));
    }

    /// A second invocation seconds after a confirmed send must not repeat a still-triggered
    /// monitor, but it returns once the reminder interval passes.
    #[tokio::test]
    async fn a_still_triggered_monitor_is_suppressed_seconds_after_a_send_but_reminded_after_the_interval(
    ) {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) =
            seed_org(&db, &format!("org-digest-triggered-reminder-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;
        let m = seed_monitor(&db, org.id, u.id, "triggered", Some(Utc::now())).await;
        subscribe(&db, m.id, u.id).await;

        // Confirm the fixture produces content before any watermark narrows it, so the later
        // `is_none()` isn't vacuous.
        let before = build_digest_content(&db, org.id, &u).await.unwrap();
        assert!(
            before.is_some(),
            "with no prior send, a subscribed triggered monitor must be reported"
        );

        let mut active: user::ActiveModel = u.clone().into();
        active.digest_last_sent_at = Set(Some((Utc::now() - Duration::seconds(5)).into()));
        let u_recent = active.update(&db).await.unwrap();
        let repeated = build_digest_content(&db, org.id, &u_recent).await.unwrap();
        assert!(
            repeated.is_none(),
            "an unchanged still-triggered monitor must not be repeated seconds after the last send"
        );

        // Past the reminder interval the monitor is reported again.
        let mut active: user::ActiveModel = u_recent.clone().into();
        active.digest_last_sent_at = Set(Some((Utc::now() - Duration::hours(21)).into()));
        let u_overdue = active.update(&db).await.unwrap();
        let (subject, text, _html) = build_digest_content(&db, org.id, &u_overdue)
            .await
            .unwrap()
            .expect("past the reminder interval, the still-triggered monitor must be reported");
        assert!(subject.contains("still Alerting"));
        assert!(text.contains(&m.name));
    }

    /// The reminder gate must not suppress a fresh state change inside the 24h window.
    #[tokio::test]
    async fn a_recent_watermark_does_not_suppress_a_fresh_state_change() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) =
            seed_org(&db, &format!("org-digest-changes-not-collateral-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;
        let m = seed_monitor(&db, org.id, u.id, "ok", None).await;
        subscribe(&db, m.id, u.id).await;

        let mut active: user::ActiveModel = u.clone().into();
        active.digest_last_sent_at = Set(Some((Utc::now() - Duration::minutes(1)).into()));
        let u_recent = active.update(&db).await.unwrap();

        seed_state_change_event(
            &db,
            org.id,
            m.id,
            "ok",
            "triggered",
            Utc::now() - Duration::seconds(10),
        )
        .await;

        let result = build_digest_content(&db, org.id, &u_recent).await.unwrap();
        assert!(
            result.is_some(),
            "the triggered-reminder gate must not suppress a fresh Changes-section event"
        );
    }

    #[tokio::test]
    async fn subject_line_pluralizes_correctly() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-plural-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;

        let m1 = seed_monitor(&db, org.id, u.id, "ok", None).await;
        let m2 = seed_monitor(&db, org.id, u.id, "ok", None).await;
        subscribe(&db, m1.id, u.id).await;
        subscribe(&db, m2.id, u.id).await;

        seed_state_change_event(&db, org.id, m1.id, "ok", "triggered", Utc::now()).await;
        seed_state_change_event(&db, org.id, m2.id, "triggered", "ok", Utc::now()).await;

        let (subject, _, _) = build_digest_content(&db, org.id, &u)
            .await
            .unwrap()
            .expect("two changes must produce a digest");
        assert!(subject.contains("2 monitor updates"));

        let (org2, group_id2) = seed_org(&db, &format!("org-digest-plural2-{suffix}")).await;
        let u2 = seed_user(&db, org2.id, group_id2).await;
        let t1 = seed_monitor(&db, org2.id, u2.id, "triggered", Some(Utc::now())).await;
        let t2 = seed_monitor(&db, org2.id, u2.id, "triggered", Some(Utc::now())).await;
        subscribe(&db, t1.id, u2.id).await;
        subscribe(&db, t2.id, u2.id).await;

        let (subject2, _, _) = build_digest_content(&db, org2.id, &u2)
            .await
            .unwrap()
            .expect("two triggered monitors must produce a digest");
        assert!(subject2.contains("2 still Alerting"));
    }

    /// A monitor already under "Changes" must not also appear under "Currently Alerting".
    #[tokio::test]
    async fn a_monitor_that_changed_state_is_not_listed_twice() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-dedup-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;
        let m = seed_monitor(&db, org.id, u.id, "triggered", Some(Utc::now())).await;
        subscribe(&db, m.id, u.id).await;

        seed_state_change_event(
            &db,
            org.id,
            m.id,
            "ok",
            "triggered",
            Utc::now() - Duration::hours(1),
        )
        .await;

        let (_, text, _) = build_digest_content(&db, org.id, &u)
            .await
            .unwrap()
            .expect("a changed-and-still-triggered monitor must produce a digest");

        assert_eq!(
            text.matches(&m.name).count(),
            1,
            "the monitor name must appear exactly once, not once per section: {text}"
        );
        assert!(
            !text.contains("Currently Alerting"),
            "the whole section must be dropped once its only row is deduped away: {text}"
        );
    }

    /// Timestamps render as `%b %-d, %Y, %-I:%M %p UTC`, never a bare ISO date; detects
    /// `\d{4}-\d{2}-\d{2}` without a regex dependency.
    fn contains_iso_date(s: &str) -> bool {
        let b = s.as_bytes();
        (0..b.len().saturating_sub(9)).any(|i| {
            b[i..i + 4].iter().all(u8::is_ascii_digit)
                && b[i + 4] == b'-'
                && b[i + 5..i + 7].iter().all(u8::is_ascii_digit)
                && b[i + 7] == b'-'
                && b[i + 8..i + 10].iter().all(u8::is_ascii_digit)
        })
    }

    #[tokio::test]
    async fn timestamps_match_the_monitor_alert_format() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-timestamp-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;
        let m = seed_monitor(&db, org.id, u.id, "ok", None).await;
        subscribe(&db, m.id, u.id).await;

        let ts = Utc::now() - Duration::hours(1);
        seed_state_change_event(&db, org.id, m.id, "ok", "triggered", ts).await;

        let (_, text, _) = build_digest_content(&db, org.id, &u)
            .await
            .unwrap()
            .expect("a recent change must produce a digest");

        // Byte-identical to the monitor-alert email's "Checked" timestamp.
        let expected = ts.format("%b %-d, %Y, %-I:%M %p UTC").to_string();
        assert!(
            text.contains(&expected),
            "expected timestamp {expected:?} not found in: {text}"
        );
        assert!(
            !contains_iso_date(&text),
            "the old ISO-ish timestamp format must be gone: {text}"
        );
    }

    /// Real dispatch through a `Conveyor`: with no mailer configured the row must finalize
    /// "skipped", not "sent".
    #[tokio::test]
    async fn conveyor_dispatch_delivers_and_finalizes_the_digest_row() {
        let Some((db, redis)) = crate::scheduler::test_support::setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-conveyor-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;
        let m = seed_monitor(&db, org.id, u.id, "ok", None).await;
        subscribe(&db, m.id, u.id).await;
        seed_state_change_event(
            &db,
            org.id,
            m.id,
            "ok",
            "triggered",
            Utc::now() - Duration::hours(1),
        )
        .await;
        let ctx = Arc::new(crate::scheduler::test_support::test_ctx(
            db.clone(),
            redis.clone(),
        ));

        let delivery_id =
            arcodash_core::entities::mail_delivery::insert_pending_digest(&db, org.id, u.id)
                .await
                .unwrap();

        let redis_url = std::env::var("TEST_REDIS_URL").unwrap();
        let conveyor = arcodash_conveyor::ConveyorBuilder::new(db.clone(), redis, redis_url)
            .register("send_digest", SendDigestHandler { ctx: ctx.clone() })
            .tier("email", &["send_digest"], 4)
            .build();

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let run_handle = {
            let conveyor = conveyor.clone();
            tokio::spawn(async move { conveyor.run(shutdown_rx).await })
        };

        let payload = serde_json::to_value(SendDigestPayload {
            org_id: org.id,
            user_id: u.id,
        })
        .unwrap();

        conveyor
            .enqueue(arcodash_conveyor::EnqueueRequest {
                task_type: "send_digest".into(),
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
            let row = arcodash_core::entities::mail_delivery::Entity::find_by_id(delivery_id)
                .one(&db)
                .await
                .unwrap()
                .unwrap();
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

        assert_eq!(row.status, "skipped");
        assert!(row.finished_at.is_some());
    }

    /// A terminal send failure releases the per-day guard (real SMTP pointed at a closed port, no
    /// mock).
    #[tokio::test]
    async fn terminal_send_failure_releases_the_daily_guard() {
        let Some((db, redis)) = crate::scheduler::test_support::setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-guard-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;
        // Needs non-empty digest content, or `handle_inner` returns before the send.
        let mon = seed_monitor(&db, org.id, u.id, "triggered", Some(Utc::now())).await;
        subscribe(&db, mon.id, u.id).await;

        let local_date = arcodash_core::digest_schedule::user_local_date(
            u.digest_timezone.as_deref(),
            Utc::now(),
        );
        // A recycled BIGSERIAL id can inherit a stale guard key; start clean.
        crate::scheduler::release_digest_send_guard(&redis, u.id, local_date).await;
        assert!(
            crate::scheduler::claim_digest_send_for_user(&redis, u.id, local_date).await,
            "must be able to claim a fresh guard"
        );

        let ctx = closed_port_mailer_ctx(db.clone(), redis.clone());

        let result = deliver(&ctx, org.id, u.id, None).await;
        assert!(
            result.is_err(),
            "a genuine SMTP connection failure must propagate as Err on the no-delivery-id path"
        );

        // A fresh claim only succeeds if the guard was released; release it afterward so a passing
        // run doesn't poison the next.
        let reclaimed =
            crate::scheduler::claim_digest_send_for_user(&redis, u.id, local_date).await;
        crate::scheduler::release_digest_send_guard(&redis, u.id, local_date).await;
        assert!(
            reclaimed,
            "the guard must be released on a terminal send failure so the next tick can retry"
        );
    }

    /// Backstop for a task enqueued before the recipient was disabled: reaching the send would
    /// finalize "failed" (mailer on a closed port) instead of a clean "sent" no-op.
    #[tokio::test]
    async fn handle_inner_skips_a_recipient_disabled_after_enqueue() {
        let Some((db, redis)) = crate::scheduler::test_support::setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };

        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) =
            seed_org(&db, &format!("org-digest-disabled-after-enqueue-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;
        // Needs non-empty digest content so `handle_inner` reaches the guard.
        let mon = seed_monitor(&db, org.id, u.id, "triggered", Some(Utc::now())).await;
        subscribe(&db, mon.id, u.id).await;

        let mut active: user::ActiveModel = u.clone().into();
        active.disabled_at = Set(Some(sea_orm::prelude::DateTimeWithTimeZone::from(
            Utc::now(),
        )));
        active.update(&db).await.unwrap();

        let ctx = closed_port_mailer_ctx(db.clone(), redis.clone());

        let delivery_id =
            arcodash_core::entities::mail_delivery::insert_pending_digest(&db, org.id, u.id)
                .await
                .unwrap();

        deliver(&ctx, org.id, u.id, Some(delivery_id))
            .await
            .unwrap();

        let row = arcodash_core::entities::mail_delivery::Entity::find_by_id(delivery_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.status, "sent",
            "a disabled recipient's digest must finalize as a clean no-op, never reach the mailer"
        );
    }

    /// A failure after local midnight recomputes `local_date` a day later than the claim, so
    /// `release_digest_send_guard` must delete both days. Fixed instant in `Pacific/Auckland` keeps
    /// it deterministic.
    #[tokio::test]
    async fn a_send_failure_spanning_local_midnight_still_releases_the_claimed_guard() {
        let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
            eprintln!("skipping: TEST_REDIS_URL not set");
            return;
        };
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();

        let tz = Some("Pacific/Auckland");
        // 2026-01-10T10:00:00Z is 23:00 NZDT; two hours later crosses local midnight.
        let claimed_at = chrono::DateTime::parse_from_rfc3339("2026-01-10T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let failed_at = claimed_at + Duration::hours(2);
        let claimed_date = arcodash_core::digest_schedule::user_local_date(tz, claimed_at);
        let failure_date = arcodash_core::digest_schedule::user_local_date(tz, failed_at);
        assert_ne!(
            claimed_date, failure_date,
            "fixture must actually straddle the user's local midnight"
        );

        // A fake id: only the Redis guard keys are exercised.
        let user_id = (uuid::Uuid::new_v4().as_u128() % 1_000_000_000) as i64 + 1;

        assert!(
            crate::scheduler::claim_digest_send_for_user(&redis, user_id, claimed_date).await,
            "must be able to claim the guard for the day fan-out actually claimed it on"
        );

        crate::scheduler::release_digest_send_guard(&redis, user_id, failure_date).await;

        let reclaimed =
            crate::scheduler::claim_digest_send_for_user(&redis, user_id, claimed_date).await;
        crate::scheduler::release_digest_send_guard(&redis, user_id, claimed_date).await;
        assert!(
            reclaimed,
            "the guard claimed on claimed_date must be gone even though release was called \
             with the later failure_date computed at failure time"
        );
    }

    /// Confirm the fixture produces content before the revocation so the later `is_none()` isn't
    /// vacuous.
    #[tokio::test]
    async fn a_monitor_the_user_can_no_longer_view_is_excluded() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-revoke-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;

        // Owned by someone else, workspace-visible at subscribe time.
        let mon = seed_monitor_owned_by_someone_else(&db, org.id).await;
        subscribe(&db, mon.id, u.id).await;
        seed_state_change_event(
            &db,
            org.id,
            mon.id,
            "ok",
            "triggered",
            Utc::now() - Duration::hours(1),
        )
        .await;

        let before = build_digest_content(&db, org.id, &u).await.unwrap();
        assert!(
            before.is_some(),
            "sanity: the fixture must produce a digest before access is revoked"
        );

        // Revoke: the monitor goes restricted and the subscriber has no grant.
        exec_sql(
            &db,
            "UPDATE monitors SET visibility = 'restricted' WHERE id = $1",
            [mon.id.into()],
        )
        .await;

        let after = build_digest_content(&db, org.id, &u).await.unwrap();
        assert!(
            after.is_none(),
            "a monitor the subscriber can no longer view must not produce a digest"
        );
    }

    /// Events are only loaded for monitors that pass the send-time access re-check, so a revoked
    /// monitor's events don't surface in "Changes".
    #[tokio::test]
    async fn a_revoked_monitors_events_do_not_leak_into_changes() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-leak-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;

        let surviving = seed_monitor(&db, org.id, u.id, "ok", None).await;
        subscribe(&db, surviving.id, u.id).await;
        seed_state_change_event(
            &db,
            org.id,
            surviving.id,
            "ok",
            "triggered",
            Utc::now() - Duration::hours(1),
        )
        .await;

        let revoked = seed_monitor_owned_by_someone_else(&db, org.id).await;
        subscribe(&db, revoked.id, u.id).await;
        seed_state_change_event(
            &db,
            org.id,
            revoked.id,
            "ok",
            "triggered",
            Utc::now() - Duration::hours(1),
        )
        .await;

        exec_sql(
            &db,
            "UPDATE monitors SET visibility = 'restricted' WHERE id = $1",
            [revoked.id.into()],
        )
        .await;

        let (_, text, html) = build_digest_content(&db, org.id, &u)
            .await
            .unwrap()
            .expect("the surviving monitor's event must still produce a digest");

        assert!(text.contains(&surviving.name));
        assert!(!text.contains(&revoked.name));
        assert!(!html.contains(&revoked.name));
    }

    /// A subscription row pointing at a monitor in another org (inserted directly) must never be
    /// joined into the digest.
    #[tokio::test]
    async fn a_monitor_from_another_org_is_never_joined_in() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-crossorg-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;

        let legit = seed_monitor(&db, org.id, u.id, "ok", None).await;
        subscribe(&db, legit.id, u.id).await;
        seed_state_change_event(
            &db,
            org.id,
            legit.id,
            "ok",
            "triggered",
            Utc::now() - Duration::hours(1),
        )
        .await;

        let (foreign_org, foreign_group_id) =
            seed_org(&db, &format!("org-digest-crossorg-foreign-{suffix}")).await;
        let foreign_owner = seed_user(&db, foreign_org.id, foreign_group_id).await;
        let foreign = seed_monitor(
            &db,
            foreign_org.id,
            foreign_owner.id,
            "triggered",
            Some(Utc::now()),
        )
        .await;
        subscribe(&db, foreign.id, u.id).await;

        let (_, text, html) = build_digest_content(&db, org.id, &u)
            .await
            .unwrap()
            .expect("the legitimate same-org monitor must still produce a digest");

        assert!(text.contains(&legit.name));
        assert!(!text.contains(&foreign.name));
        assert!(!html.contains(&foreign.name));
    }

    /// An acknowledged monitor stays listed under "Currently Alerting" but its row says so.
    #[tokio::test]
    async fn an_acknowledged_monitor_says_so() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-acked-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;
        let m = seed_monitor(&db, org.id, u.id, "triggered", Some(Utc::now())).await;
        subscribe(&db, m.id, u.id).await;

        exec_sql(
            &db,
            "UPDATE monitors SET acked_at = now(), acked_by = $1 WHERE id = $2",
            [u.id.into(), m.id.into()],
        )
        .await;

        let (_, text, _) = build_digest_content(&db, org.id, &u)
            .await
            .unwrap()
            .expect("an acknowledged-but-still-triggered monitor must still produce a digest");

        assert!(
            text.contains(&m.name),
            "the annotation must not turn into an exclusion: {text}"
        );
        assert!(
            text.contains("Acknowledged"),
            "must annotate the row: {text}"
        );
    }

    /// A snoozed monitor stays listed under "Currently Alerting" but its row says so.
    #[tokio::test]
    async fn a_snoozed_monitor_says_so() {
        let Some(db) = setup().await else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let (org, group_id) = seed_org(&db, &format!("org-digest-snoozed-{suffix}")).await;
        let u = seed_user(&db, org.id, group_id).await;
        let m = seed_monitor(&db, org.id, u.id, "triggered", Some(Utc::now())).await;
        subscribe(&db, m.id, u.id).await;

        exec_sql(
            &db,
            "UPDATE monitors SET options = $1 WHERE id = $2",
            [
                serde_json::json!({ "muted_until": (Utc::now() + Duration::hours(1)).to_rfc3339() })
                    .into(),
                m.id.into(),
            ],
        )
        .await;

        let (_, text, _) = build_digest_content(&db, org.id, &u)
            .await
            .unwrap()
            .expect("a snoozed-but-still-triggered monitor must still produce a digest");

        assert!(
            text.contains(&m.name),
            "the annotation must not turn into an exclusion: {text}"
        );
        assert!(text.contains("Snoozed"), "must annotate the row: {text}");
    }

    #[test]
    fn digest_window_start_defaults_to_24h_ago_when_never_sent() {
        let now = Utc::now();
        assert_eq!(digest_window_start(None, now), now - Duration::hours(24));
    }

    #[test]
    fn digest_window_start_uses_the_watermark_when_recent() {
        let now = Utc::now();
        let last_sent = now - Duration::hours(1);
        assert_eq!(digest_window_start(Some(last_sent), now), last_sent);
    }

    #[test]
    fn digest_window_start_clamps_an_ancient_watermark_to_24h() {
        let now = Utc::now();
        let last_sent = now - Duration::days(30);
        assert_eq!(
            digest_window_start(Some(last_sent), now),
            now - Duration::hours(24)
        );
    }

    #[test]
    fn should_remind_triggered_with_no_prior_send_reminds() {
        assert!(should_remind_triggered(None, Utc::now()));
    }

    #[test]
    fn should_remind_triggered_seconds_after_the_last_send_does_not_remind() {
        let now = Utc::now();
        assert!(!should_remind_triggered(
            Some(now - Duration::minutes(1)),
            now
        ));
    }

    #[test]
    fn should_remind_triggered_well_past_the_interval_reminds() {
        let now = Utc::now();
        assert!(should_remind_triggered(
            Some(now - Duration::hours(21)),
            now
        ));
    }

    #[test]
    fn should_remind_triggered_at_exactly_the_boundary_reminds() {
        let now = Utc::now();
        assert!(should_remind_triggered(
            Some(now - TRIGGERED_REMINDER_INTERVAL),
            now
        ));
    }

    // ---- dashboard briefs ----

    use crate::scheduler::test_support::test_ctx;
    use arcodash_core::ai::{AiError, AiProvider, AiProviderKind, JsonGeneration, SqlGeneration};
    use arcodash_core::entities::{dashboard_narrative, query_result, visualization, widget};
    use serde_json::{json, Value};
    use std::sync::Mutex;

    /// Prompts seen by the scripted provider, and a name whose prompts fail.
    struct Script {
        fail_when_contains: Option<String>,
        prompts: Vec<String>,
    }
    static SCRIPT: Mutex<Option<Script>> = Mutex::new(None);

    fn set_script(fail_when_contains: Option<&str>) {
        *SCRIPT.lock().unwrap() = Some(Script {
            fail_when_contains: fail_when_contains.map(str::to_string),
            prompts: vec![],
        });
    }

    fn prompts() -> Vec<String> {
        SCRIPT.lock().unwrap().as_ref().unwrap().prompts.clone()
    }

    struct ScriptedProvider;

    #[async_trait::async_trait]
    impl AiProvider for ScriptedProvider {
        fn kind(&self) -> AiProviderKind {
            AiProviderKind::Anthropic
        }
        fn default_model(&self) -> &'static str {
            "fake-model"
        }
        fn curated_models(&self) -> &'static [&'static str] {
            &[]
        }
        async fn test_key(&self, _: &reqwest::Client, _: &str, _: &str) -> Result<(), AiError> {
            unimplemented!()
        }
        async fn generate_sql(
            &self,
            _: &reqwest::Client,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<SqlGeneration, AiError> {
            unimplemented!()
        }
        async fn suggest_visualization(
            &self,
            _: &reqwest::Client,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<arcodash_core::ai::viz::VizGeneration, AiError> {
            unimplemented!()
        }
        async fn suggest_monitors(
            &self,
            _: &reqwest::Client,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<arcodash_core::ai::monitor_suggestions::MonitorSuggestionGeneration, AiError>
        {
            unimplemented!()
        }
        async fn describe_schema(
            &self,
            _: &reqwest::Client,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<arcodash_core::ai::describe::SchemaDescriptions, AiError> {
            unimplemented!()
        }
        async fn generate_json(
            &self,
            _: &reqwest::Client,
            _: &str,
            _: &str,
            prompt: &str,
            _: &Value,
        ) -> Result<JsonGeneration, AiError> {
            let fail = {
                let mut g = SCRIPT.lock().unwrap();
                let sc = g.as_mut().unwrap();
                sc.prompts.push(prompt.to_string());
                sc.fail_when_contains
                    .as_deref()
                    .is_some_and(|n| prompt.contains(n))
            };
            if fail {
                return Err(AiError::GenerationFailed("boom".into()));
            }
            Ok(JsonGeneration {
                value: json!({"summary": "Revenue is up.", "body": "", "confidence": "high"}),
                tokens_in: Some(7),
                tokens_out: Some(3),
            })
        }
    }

    fn scripted_lookup(_: AiProviderKind, _: Option<String>) -> Box<dyn AiProvider> {
        Box::new(ScriptedProvider)
    }

    struct Fx {
        ctx: ExecuteQueryContext,
        user: user::Model,
    }

    async fn fx(settings: Value) -> Option<Fx> {
        let db = setup().await?;
        let redis_url = std::env::var("TEST_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:6380".to_string());
        let redis = arcodash_core::valkey_client::connect(&redis_url)
            .await
            .unwrap();
        let (org, gid) = seed_org(&db, &format!("digest-dash-{}", uuid::Uuid::new_v4())).await;
        arcodash_core::org_settings::merge_org_settings_returning(&db, org.id, settings)
            .await
            .unwrap();
        let user = seed_user(&db, org.id, gid).await;
        let mut ctx = test_ctx(db, redis);
        ctx.ai = Some(crate::handlers::explain_alert::tests::ai_runtime(
            scripted_lookup,
        ));
        set_script(None);
        Some(Fx { ctx, user })
    }

    /// A dashboard owned by `owner_id` with one widget over a cached result.
    async fn seed_dashboard(
        db: &DatabaseConnection,
        org_id: i64,
        owner_id: i64,
        name: &str,
    ) -> dashboard::Model {
        let sfx = uuid::Uuid::new_v4();
        let now: sea_orm::prelude::DateTimeWithTimeZone = Utc::now().into();
        let ds = crate::scheduler::test_support::seed_data_source(db, org_id).await;
        let result = query_result::ActiveModel {
            org_id: Set(org_id),
            data_source_id: Set(ds.id),
            query_hash: Set(format!("h-{sfx}")),
            query: Set("SELECT 1".into()),
            data: Set(Some(
                json!({"columns": [{"name": "v"}], "rows": [{"v": 5}]}),
            )),
            retrieved_at: Set(now),
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
            name: Set(format!("q-{sfx}")),
            description: Set(None),
            query: Set("SELECT 1".into()),
            query_hash: Set(format!("h-{sfx}")),
            api_key: Set(sfx.to_string()),
            user_id: Set(owner_id),
            last_modified_by_id: Set(None),
            latest_query_data_id: Set(Some(result.id)),
            is_archived: Set(false),
            options: Set(json!({})),
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
        let viz = visualization::ActiveModel {
            query_id: Set(q.id),
            r#type: Set("TABLE".into()),
            name: Set(format!("viz-{sfx}")),
            description: Set(None),
            options: Set(json!({})),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
        let dash = dashboard::ActiveModel {
            org_id: Set(org_id),
            slug: Set(format!("dash-{sfx}")),
            name: Set(name.into()),
            user_id: Set(owner_id),
            layout: Set(json!([])),
            dashboard_filters_enabled: Set(false),
            is_archived: Set(false),
            visibility: Set("org".into()),
            tags: Set(None),
            options: Set(json!({})),
            published_snapshot: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
        widget::ActiveModel {
            dashboard_id: Set(dash.id),
            visualization_id: Set(Some(viz.id)),
            text: Set(None),
            width: Set(1),
            options: Set(json!({"position": {"row": 0, "col": 0}})),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
        dash
    }

    /// `age_mins` ago, so a larger value is an older favorite.
    async fn favorite_dashboard(f: &Fx, dashboard_id: i64, age_mins: i64) {
        let at: sea_orm::prelude::DateTimeWithTimeZone =
            (Utc::now() - Duration::minutes(age_mins)).into();
        favorite::ActiveModel {
            org_id: Set(f.user.org_id),
            object_type: Set("dashboard".into()),
            object_id: Set(dashboard_id),
            user_id: Set(f.user.id),
            created_at: Set(at),
            updated_at: Set(at),
            ..Default::default()
        }
        .insert(&f.ctx.db)
        .await
        .unwrap();
    }

    async fn content(f: &Fx, lines: &[DashboardLine]) -> Option<(String, String, String)> {
        build_digest_content_with(
            &f.ctx.db,
            f.user.org_id,
            &f.user,
            "tok",
            "http://base",
            Utc::now(),
            lines,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn starred_dashboard_adds_dashboards_section() {
        let Some(f) = fx(json!({})).await else { return };
        let d = seed_dashboard(&f.ctx.db, f.user.org_id, f.user.id, "Sales").await;
        favorite_dashboard(&f, d.id, 1).await;

        let lines = dashboard_lines(&f.ctx, f.user.org_id, &f.user).await;
        assert_eq!(lines.len(), 1);
        let (subject, text, html) = content(&f, &lines).await.expect("digest");
        assert_eq!(subject, "arcodash digest — Sales");
        for body in [&text, &html] {
            assert!(body.contains("Dashboards"), "{body}");
            assert!(body.contains("Revenue is up."), "{body}");
            assert!(body.contains("Open Sales"), "{body}");
            assert!(body.contains(&format!("http://base/dashboards/{}", d.slug)));
            assert!(body.contains("View your dashboards"));
            assert!(body.contains("Dashboard briefs are written by AI"));
        }
        let stored = dashboard_narrative::Entity::find()
            .filter(dashboard_narrative::Column::DashboardId.eq(d.id))
            .all(&f.ctx.db)
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].source, "digest");
        assert_eq!(stored[0].user_id, Some(f.user.id));
    }

    #[tokio::test]
    async fn dashboards_only_digest_is_sent() {
        let Some(f) = fx(json!({})).await else { return };
        assert!(content(&f, &[]).await.is_none());
        let one = [DashboardLine {
            name: "Sales".into(),
            slug: "sales".into(),
            summary: "Up.".into(),
        }];
        assert!(content(&f, &one).await.is_some());
    }

    #[tokio::test]
    async fn failing_dashboard_is_skipped_digest_still_built() {
        let Some(f) = fx(json!({})).await else { return };
        let ok = seed_dashboard(&f.ctx.db, f.user.org_id, f.user.id, "Sales").await;
        let bad = seed_dashboard(&f.ctx.db, f.user.org_id, f.user.id, "Broken").await;
        favorite_dashboard(&f, ok.id, 2).await;
        favorite_dashboard(&f, bad.id, 1).await;
        set_script(Some("Broken"));

        let m = seed_monitor(&f.ctx.db, f.user.org_id, f.user.id, "ok", None).await;
        subscribe(&f.ctx.db, m.id, f.user.id).await;
        seed_state_change_event(
            &f.ctx.db,
            f.user.org_id,
            m.id,
            "ok",
            "triggered",
            Utc::now() - Duration::hours(1),
        )
        .await;

        let lines = dashboard_lines(&f.ctx, f.user.org_id, &f.user).await;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].name, "Sales");
        let (subject, text, _) = content(&f, &lines).await.expect("digest");
        assert_eq!(
            subject,
            "arcodash digest — 1 monitor update · 1 dashboard brief"
        );
        assert!(text.contains("View your monitors"));
        assert!(!text.contains("Broken"));
    }

    #[tokio::test]
    async fn setting_off_leaves_digest_unchanged() {
        let Some(f) = fx(json!({"ai": {"dashboard_narratives": false}})).await else {
            return;
        };
        let d = seed_dashboard(&f.ctx.db, f.user.org_id, f.user.id, "Sales").await;
        favorite_dashboard(&f, d.id, 1).await;
        assert!(dashboard_lines(&f.ctx, f.user.org_id, &f.user)
            .await
            .is_empty());
        assert!(prompts().is_empty());
    }

    #[tokio::test]
    async fn sample_values_off_skips_dashboard_briefs() {
        let Some(f) = fx(json!({"ai": {"include_sample_values": false}})).await else {
            return;
        };
        let d = seed_dashboard(&f.ctx.db, f.user.org_id, f.user.id, "Sales").await;
        favorite_dashboard(&f, d.id, 1).await;
        assert!(dashboard_lines(&f.ctx, f.user.org_id, &f.user)
            .await
            .is_empty());
        assert!(prompts().is_empty());
    }

    #[tokio::test]
    async fn unviewable_starred_dashboard_is_ignored() {
        let Some(f) = fx(json!({})).await else { return };
        let d = seed_dashboard(&f.ctx.db, f.user.org_id, f.user.id, "Sales").await;
        favorite_dashboard(&f, d.id, 1).await;
        let mut am: user::ActiveModel = f.user.clone().into();
        am.groups = Set(vec![]);
        let user = am.update(&f.ctx.db).await.unwrap();
        assert!(dashboard_lines(&f.ctx, user.org_id, &user).await.is_empty());
        assert!(prompts().is_empty());
    }

    #[tokio::test]
    async fn at_most_three_newest_starred() {
        let Some(f) = fx(json!({})).await else { return };
        for (name, age) in [("D1", 40), ("D2", 30), ("D3", 20), ("D4", 10)] {
            let d = seed_dashboard(&f.ctx.db, f.user.org_id, f.user.id, name).await;
            favorite_dashboard(&f, d.id, age).await;
        }
        let lines = dashboard_lines(&f.ctx, f.user.org_id, &f.user).await;
        let names: Vec<&str> = lines.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["D4", "D3", "D2"]);
        assert_eq!(prompts().len(), 3);
    }

    fn line(name: &str) -> DashboardLine {
        DashboardLine {
            name: name.into(),
            slug: name.to_lowercase(),
            summary: "s".into(),
        }
    }

    fn change() -> ChangeLine {
        ChangeLine {
            monitor_id: 1,
            monitor_name: "m".into(),
            old_state: "ok".into(),
            new_state: "triggered".into(),
            at: Utc::now(),
        }
    }

    #[test]
    fn subject_and_intro_variants() {
        let r = |c: &[ChangeLine], d: &[DashboardLine]| render_digest(c, &[], d, "http://b", "t");

        let (s, text, _) = r(&[change()], &[]);
        assert_eq!(s, "arcodash digest — 1 monitor update");
        assert!(text.contains("The last 24 hours for the monitors you follow."));
        assert!(!text.contains("Dashboards"));
        assert!(!text.contains("written by AI"));

        let (s, text, _) = r(&[change()], &[line("A"), line("B")]);
        assert_eq!(s, "arcodash digest — 1 monitor update · 2 dashboard briefs");
        assert!(text.contains("plus a brief on your favorite dashboards."));

        let (s, text, _) = r(&[], &[line("A")]);
        assert_eq!(s, "arcodash digest — A");
        assert!(text.contains("What changed on your favorite dashboards."));
        assert!(text.contains("http://b/dashboards\n") || text.contains("http://b/dashboards"));

        let (s, _, _) = r(&[], &[line("A"), line("B"), line("C")]);
        assert_eq!(s, "arcodash digest — 3 dashboard briefs");
    }
}
