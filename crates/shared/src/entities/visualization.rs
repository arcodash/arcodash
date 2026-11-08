use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "visualizations")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub query_id: i64,
    #[sea_orm(column_name = "type")]
    pub r#type: String,
    pub name: String,
    pub description: Option<String>,
    pub options: Json,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::query::Entity",
        from = "Column::QueryId",
        to = "super::query::Column::Id"
    )]
    Query,
    #[sea_orm(has_many = "super::widget::Entity")]
    Widget,
}

impl Related<super::query::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Query.def()
    }
}

impl Related<super::widget::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Widget.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
