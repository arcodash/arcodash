use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "monitor_subscriptions")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub monitor_id: i64,
    pub user_id: i64,
    pub destination_id: Option<i64>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::monitor::Entity",
        from = "Column::MonitorId",
        to = "super::monitor::Column::Id"
    )]
    Monitor,
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::UserId",
        to = "super::user::Column::Id"
    )]
    User,
    #[sea_orm(
        belongs_to = "super::notification_destination::Entity",
        from = "Column::DestinationId",
        to = "super::notification_destination::Column::Id"
    )]
    NotificationDestination,
}

impl Related<super::monitor::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Monitor.def()
    }
}

impl Related<super::user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::notification_destination::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::NotificationDestination.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
