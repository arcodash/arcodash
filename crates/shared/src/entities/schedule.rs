use sea_orm::entity::prelude::*;

/// One row per schedulable entity (`query` or `monitor`), polymorphic on `entity_type`/`entity_id`
/// — no DB FK to `queries`/`monitors` (see `run.rs`'s note on the same pattern).
///
/// `next_run_at` is the materialized deadline the worker's deadline loop keys off; `spec` is the
/// untyped `ScheduleSpec` JSON (`{"kind":"interval"|"cron"|"at", ...}`, typed/validated in
/// `crates/conveyor/src/schedule_spec.rs`).
///
/// A row here is removed by a DB trigger when its `queries`/`monitors` entity row is deleted — not
/// by application code.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "schedules")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub entity_type: String,
    pub entity_id: i64,
    pub spec: Json,
    pub timezone: String,
    pub enabled: bool,
    pub paused_until: Option<DateTimeWithTimeZone>,
    pub auto_paused_at: Option<DateTimeWithTimeZone>,
    pub overlap_policy: String,
    pub next_run_at: Option<DateTimeWithTimeZone>,
    pub consecutive_failures: i32,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::organization::Entity",
        from = "Column::OrgId",
        to = "super::organization::Column::Id"
    )]
    Organization,
    #[sea_orm(has_many = "super::run::Entity")]
    Run,
}

impl Related<super::organization::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Organization.def()
    }
}

impl Related<super::run::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Run.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
