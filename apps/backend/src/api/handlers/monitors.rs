//! `/api/monitors` and `/api/monitor_subscriptions` — CRUD, mute/unmute, subscriptions.

use crate::extract::{Json, Path};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use arcodash_core::entities::{
    escalation_step, monitor, monitor_observation, monitor_subscription, notification_delivery,
    notification_destination, query, user,
};

use super::deserialize_present;
use crate::api::handlers::runs::RunResponse;
use crate::auth::CurrentUser;
use crate::authz::authorize::{self, Action, Caller, MonitorVerb};
use crate::db::Db;
use crate::error::ApiError;
use crate::services::{monitor_service, run_service};
use crate::state::AppState;

/// The `caller` block for monitor `id`, from the Edit-tier `evaluate(...)` the mutation gates use.
/// Monitors have no share surface, so `can_share` stays unset.
async fn monitor_caller(db: &Db, user: &user::Model, id: i64) -> Result<Caller, ApiError> {
    let eval = authorize::evaluate(
        db,
        user,
        Action::Monitor {
            verb: MonitorVerb::Edit,
            id,
        },
    )
    .await?;
    Ok(eval.into())
}

// ── Response types ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct UserStub {
    pub id: i64,
    pub name: String,
    pub email: String,
}

impl From<user::Model> for UserStub {
    fn from(u: user::Model) -> Self {
        Self {
            id: u.id,
            name: u.name,
            email: u.email,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct QueryStub {
    pub id: i64,
    pub name: String,
}

impl From<query::Model> for QueryStub {
    fn from(q: query::Model) -> Self {
        Self {
            id: q.id,
            name: q.name,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct EscalationStepDto {
    pub delay_minutes: i32,
    pub destination_id: i64,
}

impl From<escalation_step::Model> for EscalationStepDto {
    fn from(s: escalation_step::Model) -> Self {
        Self {
            delay_minutes: s.delay_minutes,
            destination_id: s.destination_id,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MonitorResponse {
    pub id: i64,
    pub name: String,
    pub query: QueryStub,
    pub options: Value,
    pub state: String,
    pub last_triggered_at: Option<DateTime<Utc>>,
    pub rearm: Option<i32>,
    pub user: UserStub,
    pub org_id: i64,
    pub visibility: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub acked_at: Option<DateTime<Utc>>,
    /// Server-resolved acker email (never the raw `acked_by` id), so the frontend needs no users
    /// lookup.
    pub acked_by_email: Option<String>,
    pub escalation_step: i32,
    /// Set only while `state == "unevaluable"`: why the last check couldn't resolve a condition.
    pub unevaluable_reason: Option<String>,
    /// Ordered escalation chain (may be empty).
    pub escalation: Vec<EscalationStepDto>,
    /// `"threshold"` or `"baseline"`.
    pub kind: String,
    pub learning: Option<monitor_service::Learning>,
    /// Server-computed authz summary (see [`monitor_caller`]); handlers overwrite this no-access
    /// default.
    pub caller: Caller,
}

/// The full single-monitor response: parent query, owner/acker/escalation, and `caller`.
/// `missing_query` is the (logged-only) 500 message if the parent query is gone.
async fn monitor_json(
    db: &Db,
    user: &user::Model,
    a: monitor::Model,
    missing_query: &str,
) -> Result<Json<MonitorResponse>, ApiError> {
    let id = a.id;
    let q = query::Entity::find_by_id(a.query_id)
        .one(db)
        .await?
        .ok_or_else(|| ApiError::internal(missing_query))?;
    let mut resp = build_monitor_responses(db, vec![(a, q)])
        .await?
        .pop()
        .expect("one pair in, one response out");
    resp.caller = monitor_caller(db, user, id).await?;
    Ok(Json(resp))
}

/// Monitor responses (with a no-access `caller`) whose owners/ackers and escalation chains are each
/// resolved in one `IN (...)` fetch.
async fn build_monitor_responses(
    db: &Db,
    pairs: Vec<(monitor::Model, query::Model)>,
) -> Result<Vec<MonitorResponse>, ApiError> {
    let monitor_ids: Vec<i64> = pairs.iter().map(|(a, _)| a.id).collect();
    let user_ids: std::collections::HashSet<i64> = pairs
        .iter()
        .flat_map(|(a, _)| std::iter::once(a.user_id).chain(a.acked_by))
        .collect();
    let users: std::collections::HashMap<i64, user::Model> = if user_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        user::Entity::find()
            .filter(user::Column::Id.is_in(user_ids))
            .all(db)
            .await?
            .into_iter()
            .map(|u| (u.id, u))
            .collect()
    };

    let mut escalations: std::collections::HashMap<i64, Vec<EscalationStepDto>> =
        std::collections::HashMap::new();
    if !monitor_ids.is_empty() {
        let rows = escalation_step::Entity::find()
            .filter(escalation_step::Column::MonitorId.is_in(monitor_ids.clone()))
            .order_by_asc(escalation_step::Column::Position)
            .all(db)
            .await?;
        for s in rows {
            escalations
                .entry(s.monitor_id)
                .or_default()
                .push(EscalationStepDto::from(s));
        }
    }

    let summaries = monitor_service::observation_summaries(db, &monitor_ids).await?;
    let mut out = Vec::with_capacity(pairs.len());
    for (a, q) in pairs {
        let monitor_user = users
            .get(&a.user_id)
            .cloned()
            .ok_or_else(|| ApiError::internal("monitor owner not found"))?;
        let acked_by_email = a
            .acked_by
            .and_then(|uid| users.get(&uid).map(|u| u.email.clone()));
        let escalation = escalations.remove(&a.id).unwrap_or_default();
        let (kind, learning) =
            monitor_service::kind_and_learning(&a.options, a.id, summaries.get(&a.id));

        out.push(MonitorResponse {
            id: a.id,
            name: a.name,
            query: QueryStub::from(q),
            options: a.options,
            state: a.state,
            last_triggered_at: a.last_triggered_at.map(|t| t.with_timezone(&Utc)),
            rearm: a.rearm,
            user: UserStub::from(monitor_user),
            org_id: a.org_id,
            visibility: a.visibility,
            created_at: a.created_at.with_timezone(&Utc),
            updated_at: a.updated_at.with_timezone(&Utc),
            acked_at: a.acked_at.map(|t| t.with_timezone(&Utc)),
            acked_by_email,
            escalation_step: a.escalation_step,
            unevaluable_reason: a.unevaluable_reason,
            escalation,
            kind,
            learning,
            caller: Caller::default(),
        });
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
pub struct DestinationStub {
    pub id: i64,
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
}

#[derive(Debug, Serialize)]
pub struct SubscriptionResponse {
    pub id: i64,
    pub monitor_id: i64,
    pub user: UserStub,
    pub destination: Option<DestinationStub>,
}

async fn build_sub_response(
    db: &Db,
    sub: monitor_subscription::Model,
) -> Result<SubscriptionResponse, ApiError> {
    let sub_user = user::Entity::find_by_id(sub.user_id)
        .one(db)
        .await?
        .ok_or_else(|| ApiError::internal("subscription user not found"))?;

    let destination = if let Some(did) = sub.destination_id {
        notification_destination::Entity::find_by_id(did)
            .one(db)
            .await?
            .map(|d| DestinationStub {
                id: d.id,
                name: d.name,
                type_name: d.r#type,
            })
    } else {
        None
    };

    Ok(SubscriptionResponse {
        id: sub.id,
        monitor_id: sub.monitor_id,
        user: UserStub::from(sub_user),
        destination,
    })
}

// ── Monitor handlers ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct ListMonitorsQuery {
    /// `all` (default) | `by-me` | `shared-with-me` — narrows the list beyond plain visibility.
    pub filter: Option<String>,
    /// Narrows to monitors on this one query (the query screen's cross-linking panel).
    pub query_id: Option<i64>,
    /// Narrows to monitors whose query targets this data source (the AI suggestion feature's dedup
    /// lookup).
    pub data_source_id: Option<i64>,
}

/// `GET /api/monitors` — supports `?filter=all|by-me|shared-with-me`, `?query_id=` and
/// `?data_source_id=` narrowing.
pub async fn list(
    db: Db,
    CurrentUser(user): CurrentUser,
    Query(params): Query<ListMonitorsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    super::validate_list_filter(params.filter.as_deref())?;

    let pairs = monitor_service::list_for_user(
        &db,
        user.org_id,
        &user,
        params.filter.as_deref(),
        params.query_id,
        params.data_source_id,
        None,
    )
    .await?;
    // Groups/admin status are per-user, so load once for every row's `caller` block.
    let groups = crate::authz::permissions::load_effective_groups(&db, &user).await?;
    let is_admin = crate::authz::permissions::is_admin(&groups);

    let ids: Vec<i64> = pairs.iter().map(|(a, _)| a.id).collect();
    let roles = authorize::prefetch_roles(&db, &user, &groups, is_admin, "monitor", &ids).await?;

    let mut responses = build_monitor_responses(&db, pairs).await?;
    for resp in &mut responses {
        let eval = authorize::evaluate_with_roles(
            &db,
            &user,
            &groups,
            is_admin,
            &roles,
            Action::Monitor {
                verb: MonitorVerb::Edit,
                id: resp.id,
            },
        )
        .await?;
        resp.caller = eval.into();
    }
    Ok(Json(responses))
}

#[derive(Debug, Deserialize)]
pub struct CreateMonitorRequest {
    pub query_id: i64,
    pub name: String,
    pub options: Value,
    pub rearm: Option<i32>,
}

/// `POST /api/monitors`
pub async fn create(
    db: Db,
    CurrentUser(user): CurrentUser,
    Json(payload): Json<CreateMonitorRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let a = monitor_service::create(
        &db,
        user.org_id,
        &user,
        payload.query_id,
        payload.name,
        payload.options,
        payload.rearm,
    )
    .await?;

    monitor_json(&db, &user, a, "query not found after monitor create").await
}

pub async fn get(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let a = monitor_service::get_for_user(&db, user.org_id, &user, id).await?;
    monitor_json(&db, &user, a, "query not found").await
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateMonitorRequest {
    pub name: Option<String>,
    pub options: Option<Value>,
    /// `None` = leave unchanged; `Some(None)` = clear rearm; `Some(Some(n))` = set rearm
    #[serde(default, deserialize_with = "deserialize_present")]
    pub rearm: Option<Option<i32>>,
}

/// `POST /api/monitors/:id`
pub async fn update(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateMonitorRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = monitor_service::get_for_user(&db, user.org_id, &user, id).await?;
    let updated = monitor_service::update(
        &db,
        &user,
        existing,
        payload.name,
        payload.options,
        payload.rearm,
    )
    .await?;
    monitor_json(&db, &user, updated, "query not found").await
}

pub async fn delete(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = monitor_service::get_for_user(&db, user.org_id, &user, id).await?;
    monitor_service::delete(&db, &user, existing).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn mute(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = monitor_service::get_for_user(&db, user.org_id, &user, id).await?;
    let updated = monitor_service::set_muted(&db, &user, existing, true).await?;
    monitor_json(&db, &user, updated, "query not found").await
}

pub async fn unmute(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = monitor_service::get_for_user(&db, user.org_id, &user, id).await?;
    let updated = monitor_service::set_muted(&db, &user, existing, false).await?;
    monitor_json(&db, &user, updated, "query not found").await
}

#[derive(Debug, Deserialize)]
pub struct SnoozeRequest {
    pub minutes: i64,
}

pub async fn snooze(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<SnoozeRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = monitor_service::get_for_user(&db, user.org_id, &user, id).await?;
    let updated = monitor_service::snooze(&db, &user, existing, payload.minutes).await?;
    monitor_json(&db, &user, updated, "query not found").await
}

/// `POST /api/monitors/:id/run` — manual check now through the runs pipeline; same gate as
/// Mute/Snooze (`MonitorVerb::Edit`).
pub async fn run(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    monitor_service::get_for_user(&db, user.org_id, &user, id).await?;
    authorize::authorize(
        &db,
        &user,
        Action::Monitor {
            verb: MonitorVerb::Edit,
            id,
        },
    )
    .await?;
    let (run, already) = run_service::run_now(
        &db,
        state.queue.clone(),
        state.conveyor.clone(),
        state.redis.clone(),
        state.conveyor_db.clone(),
        user.org_id,
        user.id,
        "monitor",
        id,
        None,
    )
    .await?;
    let code = if already {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((code, Json(RunResponse::from_model(run))))
}

// ── Subscription handlers ────────────────────────────────────────────────────

/// `GET /api/monitors/:id/subscriptions`
pub async fn list_subscriptions(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let a = monitor_service::get_for_user(&db, user.org_id, &user, id).await?;
    let subs = monitor_service::list_subscriptions(&db, a.id).await?;
    let mut responses = Vec::with_capacity(subs.len());
    for sub in subs {
        responses.push(build_sub_response(&db, sub).await?);
    }
    Ok(Json(responses))
}

#[derive(Debug, Deserialize)]
pub struct CreateSubscriptionRequest {
    pub destination_id: Option<i64>,
}

pub async fn subscribe(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<CreateSubscriptionRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let a = monitor_service::get_for_user(&db, user.org_id, &user, id).await?;
    let conveyor =
        arcodash_core::notify::producer_conveyor_if_enabled(&state.conveyor_db, &state.redis);
    let sub = monitor_service::subscribe(
        &db,
        state.queue.clone(),
        conveyor,
        user.org_id,
        &user,
        &a,
        payload.destination_id,
    )
    .await?;
    Ok(Json(build_sub_response(&db, sub).await?))
}

pub async fn delete_subscription(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path((_monitor_id, sub_id)): Path<(i64, i64)>,
) -> Result<impl IntoResponse, ApiError> {
    monitor_service::unsubscribe(&db, user.org_id, &user, sub_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/monitors/:id/subscriptions/:sub_id/notify` — manual "Send now" regardless of the
/// monitor's state.
pub async fn notify_subscription(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path((monitor_id, sub_id)): Path<(i64, i64)>,
) -> Result<impl IntoResponse, ApiError> {
    let conveyor =
        arcodash_core::notify::producer_conveyor_if_enabled(&state.conveyor_db, &state.redis);
    monitor_service::notify_subscription(
        &db,
        state.queue.clone(),
        conveyor,
        user.org_id,
        &user,
        monitor_id,
        sub_id,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Delivery history ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct DeliveryResponse {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub destination_id: Option<i64>,
    pub subscription_id: Option<i64>,
    pub state: String,
    pub status: String,
    pub error: Option<String>,
    pub attempt_number: i32,
}

impl From<notification_delivery::Model> for DeliveryResponse {
    fn from(d: notification_delivery::Model) -> Self {
        Self {
            id: d.id,
            created_at: d.created_at.with_timezone(&Utc),
            destination_id: d.destination_id,
            subscription_id: d.subscription_id,
            state: d.state,
            status: d.status,
            error: d.error,
            attempt_number: d.attempt_number,
        }
    }
}

/// `GET /api/monitors/:id/deliveries` — newest-first, capped at 50.
pub async fn list_deliveries(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let a = monitor_service::get_for_user(&db, user.org_id, &user, id).await?;
    let deliveries = notification_delivery::Entity::find()
        .filter(notification_delivery::Column::MonitorId.eq(a.id))
        .order_by(notification_delivery::Column::CreatedAt, Order::Desc)
        .limit(50)
        .all(&db)
        .await?;
    Ok(Json(
        deliveries
            .into_iter()
            .map(DeliveryResponse::from)
            .collect::<Vec<_>>(),
    ))
}

#[derive(Deserialize)]
pub struct ObservationsQuery {
    limit: Option<u64>,
}

#[derive(Serialize)]
pub struct ObservationResponse {
    observed_at: DateTime<Utc>,
    value: Option<f64>,
    state: String,
    explanation: Option<String>,
    run_id: Option<i64>,
}

/// `GET /api/monitors/:id/observations?limit=` — newest `limit` (default 100, 1..=500),
/// oldest-first.
pub async fn list_observations(
    State(state): State<AppState>,
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<ObservationsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let a = monitor_service::get_for_user(&db, user.org_id, &user, id).await?;
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let mut rows = monitor_observation::Entity::find()
        .filter(monitor_observation::Column::MonitorId.eq(a.id))
        .order_by(monitor_observation::Column::ObservedAt, Order::Desc)
        .order_by(monitor_observation::Column::Id, Order::Desc)
        .limit(limit)
        .all(&db)
        .await?;
    rows.reverse();
    let range = rows.first().zip(rows.last()).map(|(first, last)| {
        (
            first.observed_at.with_timezone(&Utc),
            last.observed_at.with_timezone(&Utc),
        )
    });
    let stored = match range {
        Some((from, to)) => {
            let storage = state.storage.resolve(&state.db, user.org_id).await?;
            match arcodash_core::observation_store::load_observations(&storage, a.id, from, to)
                .await
            {
                Ok(stored) => stored,
                Err(e) => {
                    if matches!(
                        e,
                        arcodash_core::org_storage::StorageError::Unreachable { .. }
                    ) {
                        state
                            .storage
                            .report_failure(&state.db, user.org_id, &e)
                            .await;
                    }
                    return Err(e.into());
                }
            }
        }
        None => std::collections::HashMap::new(),
    };
    Ok(Json(
        rows.into_iter()
            .map(|o| {
                let values = stored.get(&o.id);
                ObservationResponse {
                    observed_at: o.observed_at.with_timezone(&Utc),
                    value: arcodash_core::observation_store::merged_value(o.value, values),
                    state: o.state,
                    explanation: arcodash_core::observation_store::merged_explanation(
                        o.explanation,
                        values,
                    ),
                    run_id: o.run_id,
                }
            })
            .collect::<Vec<_>>(),
    ))
}

/// `DELETE /api/monitor_subscriptions/:id` — flat alias of the nested subscription delete route.
pub async fn delete_subscription_flat(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(sub_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    monitor_service::unsubscribe(&db, user.org_id, &user, sub_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Escalation + acknowledgment ────────────────────────────────

pub async fn ack(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let updated = monitor_service::ack(&db, user.org_id, &user, id).await?;
    monitor_json(&db, &user, updated, "query not found").await
}

/// `GET /api/monitors/:id/escalation`
pub async fn get_escalation(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let steps = monitor_service::get_escalation(&db, user.org_id, &user, id).await?;
    Ok(Json(
        steps
            .into_iter()
            .map(EscalationStepDto::from)
            .collect::<Vec<_>>(),
    ))
}

#[derive(Debug, Deserialize)]
pub struct PutEscalationStepRequest {
    pub delay_minutes: i32,
    pub destination_id: i64,
}

/// `PUT /api/monitors/:id/escalation` — replaces the whole chain (positions from array order); `PUT
/// []` clears it.
pub async fn put_escalation(
    db: Db,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Json(payload): Json<Vec<PutEscalationStepRequest>>,
) -> Result<impl IntoResponse, ApiError> {
    let steps = payload
        .into_iter()
        .map(|s| (s.delay_minutes, s.destination_id))
        .collect();
    let saved = monitor_service::put_escalation(&db, user.org_id, &user, id, steps).await?;
    Ok(Json(
        saved
            .into_iter()
            .map(EscalationStepDto::from)
            .collect::<Vec<_>>(),
    ))
}

#[cfg(test)]
mod tests {
    use super::UpdateMonitorRequest;

    #[test]
    fn rearm_field_omitted_leaves_unchanged() {
        let req: UpdateMonitorRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req.rearm, None);
    }

    #[test]
    fn rearm_field_null_clears() {
        let req: UpdateMonitorRequest = serde_json::from_str(r#"{"rearm":null}"#).unwrap();
        assert_eq!(req.rearm, Some(None));
    }

    #[test]
    fn rearm_field_number_sets() {
        let req: UpdateMonitorRequest = serde_json::from_str(r#"{"rearm":1800}"#).unwrap();
        assert_eq!(req.rearm, Some(Some(1800)));
    }
}
