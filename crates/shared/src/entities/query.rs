use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "queries")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub data_source_id: i64,
    pub name: String,
    pub description: Option<String>,
    pub query: String,
    pub query_hash: String,
    pub api_key: String,
    pub user_id: i64,
    pub last_modified_by_id: Option<i64>,
    pub latest_query_data_id: Option<i64>,
    pub is_archived: bool,
    pub visibility: String,
    pub options: Json,
    pub tags: Option<Vec<String>>,
    pub last_execution_status: String,
    pub last_execution_at: Option<DateTimeWithTimeZone>,
    pub row_count: Option<i32>,
    pub last_execution_error: Option<String>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
    // `search_vector tsvector` deliberately not mapped here — full-text population/query is
    // raw SQL; only the DB column + GIN index exist at this layer.
    /// All `published_*` columns are NULL until the first publish; `published_change_id`
    /// being NULL is the "never published" flag other code checks.
    pub published_name: Option<String>,
    pub published_description: Option<String>,
    pub published_query: Option<String>,
    pub published_query_hash: Option<String>,
    pub published_data_source_id: Option<i64>,
    pub published_options: Option<Json>,
    pub published_latest_query_data_id: Option<i64>,
    pub published_change_id: Option<i64>,
    pub published_at: Option<DateTimeWithTimeZone>,
    /// Draft-row concurrency token; bumped on every draft write.
    pub working_version: i32,
    /// Set when an editor explicitly returns a published query to draft. `published_change_id`
    /// is untouched by this — it still means "ever published" for restore/history purposes; this
    /// column is the separate "is it live right now" bit. See `query_service::is_published`.
    pub unpublished_at: Option<DateTimeWithTimeZone>,
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
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::UserId",
        to = "super::user::Column::Id"
    )]
    User,
    #[sea_orm(
        belongs_to = "super::query_result::Entity",
        from = "Column::LatestQueryDataId",
        to = "super::query_result::Column::Id"
    )]
    LatestQueryData,
    #[sea_orm(has_many = "super::visualization::Entity")]
    Visualization,
    #[sea_orm(has_many = "super::monitor::Entity")]
    Monitor,
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

impl Related<super::user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::query_result::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::LatestQueryData.def()
    }
}

impl Related<super::visualization::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Visualization.def()
    }
}

impl Related<super::monitor::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Monitor.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
