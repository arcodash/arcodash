use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "organizations")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub name: String,
    pub slug: String,
    pub settings: Json,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::user::Entity")]
    User,
    #[sea_orm(has_many = "super::group::Entity")]
    Group,
    #[sea_orm(has_many = "super::data_source::Entity")]
    DataSource,
    #[sea_orm(has_many = "super::query::Entity")]
    Query,
    #[sea_orm(has_many = "super::query_result::Entity")]
    QueryResult,
    #[sea_orm(has_many = "super::dashboard::Entity")]
    Dashboard,
    #[sea_orm(has_many = "super::monitor::Entity")]
    Monitor,
    #[sea_orm(has_many = "super::notification_destination::Entity")]
    NotificationDestination,
    #[sea_orm(has_many = "super::query_snippet::Entity")]
    QuerySnippet,
    #[sea_orm(has_many = "super::favorite::Entity")]
    Favorite,
    #[sea_orm(has_many = "super::event::Entity")]
    Event,
    #[sea_orm(has_many = "super::api_key::Entity")]
    ApiKey,
}

impl Related<super::user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::group::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Group.def()
    }
}

impl Related<super::data_source::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DataSource.def()
    }
}

impl Related<super::query::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Query.def()
    }
}

impl Related<super::query_result::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::QueryResult.def()
    }
}

impl Related<super::dashboard::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Dashboard.def()
    }
}

impl Related<super::monitor::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Monitor.def()
    }
}

impl Related<super::notification_destination::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::NotificationDestination.def()
    }
}

impl Related<super::query_snippet::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::QuerySnippet.def()
    }
}

impl Related<super::favorite::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Favorite.def()
    }
}

impl Related<super::event::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Event.def()
    }
}

impl Related<super::api_key::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::ApiKey.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
