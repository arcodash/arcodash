use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "escalation_steps")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub monitor_id: i64,
    pub position: i32,
    pub delay_minutes: i32,
    pub destination_id: i64,
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
    #[sea_orm(
        belongs_to = "super::monitor::Entity",
        from = "Column::MonitorId",
        to = "super::monitor::Column::Id"
    )]
    Monitor,
    #[sea_orm(
        belongs_to = "super::notification_destination::Entity",
        from = "Column::DestinationId",
        to = "super::notification_destination::Column::Id"
    )]
    NotificationDestination,
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

impl Related<super::notification_destination::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::NotificationDestination.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
