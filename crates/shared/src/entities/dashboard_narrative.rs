//! One AI-generated dashboard narrative and the data digest it was built from.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "dashboard_narratives")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub dashboard_id: i64,
    pub user_id: Option<i64>,
    pub created_at: DateTimeWithTimeZone,
    /// `manual` or `digest`.
    pub source: String,
    pub summary: Option<String>,
    pub highlights: Option<Json>,
    /// Markdown; absent on rows written before it existed.
    pub body: Option<String>,
    pub digest: Option<Json>,
    /// Object-storage pointer when the content columns are empty.
    pub data_location: Option<String>,
    /// `low`, `medium` or `high`.
    pub confidence: String,
    pub model: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::dashboard::Entity",
        from = "Column::DashboardId",
        to = "super::dashboard::Column::Id"
    )]
    Dashboard,
}

impl Related<super::dashboard::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Dashboard.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
