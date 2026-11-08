use sea_orm::entity::prelude::*;
use sea_orm::ActiveValue::Set;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "notification_deliveries")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub monitor_id: i64,
    pub subscription_id: Option<i64>,
    pub destination_id: Option<i64>,
    pub state: String,
    pub status: String,
    pub error: Option<String>,
    pub attempt_number: i32,
    /// The monitor `runs` id whose evaluation produced this delivery, set by the producer at
    /// INSERT time — `None` for sends with no run behind them (e.g. escalation-step sends).
    ///
    /// NOT the same value as `JobMessage::SendNotification.run_id` on the wire: the wire field
    /// carries THIS delivery row's own id (so the handler can claim/finalize it), while this
    /// column carries the evaluation run's id. The names coincide but the values differ — do
    /// not conflate them.
    pub run_id: Option<i64>,
    /// Worker identity holding the current lease (`run_lifecycle::worker_id()`), set when a
    /// worker claims the row. `None` when `pending` or terminal.
    pub worker_id: Option<String>,
    /// Heartbeat-extended while `status = 'running'`; a `pending` row past this having never
    /// been set (`NULL`) is what the stale-pending sweep targets.
    pub lease_expires_at: Option<DateTimeWithTimeZone>,
    pub started_at: Option<DateTimeWithTimeZone>,
    /// Set only on a terminal finalize (`sent`/`failed`) — left `NULL` on a `retrying`
    /// intermediate finalize so the row is still recognizable as "not yet done".
    pub finished_at: Option<DateTimeWithTimeZone>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::organization::Entity",
        from = "Column::OrgId",
        to = "super::organization::Column::Id"
    )]
    Organization,
    #[sea_orm(
        belongs_to = "super::monitor::Entity",
        from = "Column::MonitorId",
        to = "super::monitor::Column::Id"
    )]
    Monitor,
    #[sea_orm(
        belongs_to = "super::monitor_subscription::Entity",
        from = "Column::SubscriptionId",
        to = "super::monitor_subscription::Column::Id"
    )]
    MonitorSubscription,
}

impl Related<super::organization::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Organization.def()
    }
}

impl Related<super::monitor::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Monitor.def()
    }
}

impl Related<super::monitor_subscription::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::MonitorSubscription.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

/// Insert a not-yet-attempted delivery row and return its id, BEFORE the LPUSH — a lost Redis
/// message is then recoverable from this row by the sweep.
///
/// `eval_run_id` is the monitor `runs` id (this row's `run_id` DB column) — `None` for sends with
/// no evaluation run behind them. `destination_id = -1` (the direct-subscriber-email sentinel,
/// matching `send_notification.rs`'s own convention) maps to a `NULL` `destination_id` column, same
/// as every existing delivery insert in this codebase.
pub async fn insert_pending(
    db: &impl sea_orm::ConnectionTrait,
    org_id: i64,
    monitor_id: i64,
    subscription_id: Option<i64>,
    destination_id: i64,
    new_state: &str,
    eval_run_id: Option<i64>,
) -> Result<i64, sea_orm::DbErr> {
    let row = ActiveModel {
        org_id: Set(org_id),
        monitor_id: Set(monitor_id),
        subscription_id: Set(subscription_id),
        destination_id: Set((destination_id != -1).then_some(destination_id)),
        state: Set(new_state.to_string()),
        status: Set("pending".to_string()),
        attempt_number: Set(1),
        run_id: Set(eval_run_id),
        ..Default::default()
    }
    .insert(db)
    .await?;
    Ok(row.id)
}
