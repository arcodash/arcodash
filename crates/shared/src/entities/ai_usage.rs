//! One row per real AI-provider call, for org-scoped usage metering (aggregates only — see
//! `ai_usage.rs` in the backend crate for the one place these rows are read, never individually).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "ai_usage")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub provider: String,
    pub model: String,
    pub feature: String,
    pub status: String,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub user_id: Option<i64>,
    pub cost_usd: Option<f64>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::organization::Entity",
        from = "Column::OrgId",
        to = "super::organization::Column::Id"
    )]
    Organization,
}

impl Related<super::organization::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Organization.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
