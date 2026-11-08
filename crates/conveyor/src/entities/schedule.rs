use sea_orm::entity::prelude::*;

/// One row per recurring entity. `spec` is the untyped `ScheduleSpec` JSON
/// (`crates/conveyor/src/schedule_spec.rs`); `payload` is the JSON body handed to the handler on
/// every fire.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "conveyor_schedules")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub tenant_id: Option<i64>,
    pub task_type: String,
    pub entity_id: Option<i64>,
    pub payload: Json,
    pub spec: Json,
    pub timezone: String,
    pub enabled: bool,
    pub paused_until: Option<DateTimeWithTimeZone>,
    pub auto_paused_at: Option<DateTimeWithTimeZone>,
    pub overlap_policy: String,
    pub next_run_at: Option<DateTimeWithTimeZone>,
    pub consecutive_failures: i32,
    pub resource_key: Option<String>,
    pub resource_cap: Option<i64>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
