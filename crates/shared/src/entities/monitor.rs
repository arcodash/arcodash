use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "monitors")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    pub query_id: i64,
    pub user_id: i64,
    pub options: Json,
    pub state: String,
    pub visibility: String,
    pub last_triggered_at: Option<DateTimeWithTimeZone>,
    pub rearm: Option<i32>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
    /// Set when any org member acknowledges a `triggered` incident — stops
    /// further escalation. Cleared on the next `ok|unknown -> triggered` OR `-> ok` transition.
    pub acked_at: Option<DateTimeWithTimeZone>,
    /// The acking user's id, `SET NULL` if that user is later deleted.
    pub acked_by: Option<i64>,
    /// 0 = no escalation step has fired for the current incident; k = steps 1..k have fired.
    /// Reset to 0 on every new incident (`ok|unknown -> triggered`).
    pub escalation_step: i32,
    /// When `escalation_step` last advanced — the base time the NEXT step's delay counts from.
    pub last_escalated_at: Option<DateTimeWithTimeZone>,
    /// Set only while `state = "unevaluable"` — why the last check couldn't resolve a condition
    /// (e.g. the configured column is missing from the result). Cleared on any other transition.
    pub unevaluable_reason: Option<String>,
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
        belongs_to = "super::query::Entity",
        from = "Column::QueryId",
        to = "super::query::Column::Id"
    )]
    Query,
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::UserId",
        to = "super::user::Column::Id"
    )]
    User,
    #[sea_orm(has_many = "super::monitor_subscription::Entity")]
    MonitorSubscription,
    #[sea_orm(has_many = "super::escalation_step::Entity")]
    EscalationStep,
}

impl Related<super::organization::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Organization.def()
    }
}

impl Related<super::query::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Query.def()
    }
}

impl Related<super::user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::monitor_subscription::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::MonitorSubscription.def()
    }
}

impl Related<super::escalation_step::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::EscalationStep.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
