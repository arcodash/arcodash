use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "query_results")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub data_source_id: i64,
    pub query_hash: String,
    pub query: String,
    pub data: Option<Json>,
    pub data_location: Option<String>,
    pub data_size_bytes: Option<i64>,
    pub runtime: Option<f64>,
    pub retrieved_at: DateTimeWithTimeZone,
    pub last_used_at: DateTimeWithTimeZone,
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
        belongs_to = "super::data_source::Entity",
        from = "Column::DataSourceId",
        to = "super::data_source::Column::Id"
    )]
    DataSource,
    #[sea_orm(has_many = "super::query::Entity")]
    Query,
}

impl Related<super::organization::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Organization.def()
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

impl ActiveModelBehavior for ActiveModel {}
