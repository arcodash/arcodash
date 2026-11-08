use sea_orm::entity::prelude::*;

/// Run history + the in-flight/overlap correctness mechanism.
///
/// Polymorphic on `entity_type`/`entity_id` (`query`|`monitor`) like `schedules` — deliberately no
/// DB FK to `queries`/`monitors` for that pair, since a single column can't FK two different
/// tables; entity-delete cleanup is a service-layer concern.
/// `schedule_id`/`data_source_id`/`waiting_on_run_id` are plain nullable columns with no FK either
/// (`schedule_id`'s `ON DELETE SET NULL` FK is the one exception).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "runs")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub entity_type: String,
    pub entity_id: i64,
    pub schedule_id: Option<i64>,
    pub data_source_id: Option<i64>,
    pub scheduled_for: Option<DateTimeWithTimeZone>,
    pub triggered_by: String,
    pub status: String,
    pub waiting_on_run_id: Option<i64>,
    pub job_id: Option<String>,
    pub worker_id: Option<String>,
    pub lease_expires_at: Option<DateTimeWithTimeZone>,
    pub started_at: Option<DateTimeWithTimeZone>,
    pub finished_at: Option<DateTimeWithTimeZone>,
    pub duration_ms: Option<i64>,
    pub row_count: Option<i64>,
    pub error: Option<String>,
    /// Monitor runs: age (seconds) of the evaluated data at check time.
    pub data_age_secs: Option<i64>,
    /// The `query_results` row produced (query runs) or evaluated (monitor runs) — what makes
    /// per-run CSV download possible. Same provenance as `data_age_secs`.
    pub query_result_id: Option<i64>,
    /// Monitor runs: whether the checked condition was met at finalize (true = alerting,
    /// false = ok). NULL for query runs and for a run that failed to check.
    pub condition_met: Option<bool>,
    pub created_at: DateTimeWithTimeZone,
    /// Set the moment this `pending` run actually lands on a queue — `NULL` means either not yet
    /// enqueued at all, or already past `pending` (terminal/running rows don't need it). Lets the
    /// concurrency cap count "queued, waiting for a worker" `pending` rows alongside `running`
    /// ones, distinct from a `pending` row still parked over the cap.
    pub enqueued_at: Option<DateTimeWithTimeZone>,
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
        belongs_to = "super::schedule::Entity",
        from = "Column::ScheduleId",
        to = "super::schedule::Column::Id"
    )]
    Schedule,
    #[sea_orm(
        belongs_to = "super::query_result::Entity",
        from = "Column::QueryResultId",
        to = "super::query_result::Column::Id"
    )]
    QueryResult,
}

impl Related<super::organization::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Organization.def()
    }
}

impl Related<super::schedule::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Schedule.def()
    }
}

impl Related<super::query_result::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::QueryResult.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
