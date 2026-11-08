use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "widgets")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub dashboard_id: i64,
    pub visualization_id: Option<i64>,
    pub text: Option<String>,
    pub width: i32,
    pub options: Json,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::dashboard::Entity",
        from = "Column::DashboardId",
        to = "super::dashboard::Column::Id"
    )]
    Dashboard,
    #[sea_orm(
        belongs_to = "super::visualization::Entity",
        from = "Column::VisualizationId",
        to = "super::visualization::Column::Id"
    )]
    Visualization,
}

impl Related<super::dashboard::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Dashboard.def()
    }
}

impl Related<super::visualization::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Visualization.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
