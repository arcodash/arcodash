//! One row per monitor check: the observed value and resulting state.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "monitor_observations")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub monitor_id: i64,
    pub observed_at: DateTimeWithTimeZone,
    pub value: Option<f64>,
    pub state: String,
    pub run_id: Option<i64>,
    pub query_result_id: Option<i64>,
    pub explanation: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::monitor::Entity",
        from = "Column::MonitorId",
        to = "super::monitor::Column::Id"
    )]
    Monitor,
}

impl Related<super::monitor::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Monitor.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
