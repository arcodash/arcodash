use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "dashboards")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub slug: String,
    pub name: String,
    pub user_id: i64,
    pub layout: Json,
    pub dashboard_filters_enabled: bool,
    pub is_archived: bool,
    pub visibility: String,
    pub tags: Option<Vec<String>>,
    pub options: Json,
    /// The committed tile arrangement served to non-editor reads. `None` means never published.
    pub published_snapshot: Option<Json>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
    // `search_vector tsvector` deliberately not mapped — see query.rs for rationale.
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
        belongs_to = "super::user::Entity",
        from = "Column::UserId",
        to = "super::user::Column::Id"
    )]
    User,
    #[sea_orm(has_many = "super::widget::Entity")]
    Widget,
}

impl Related<super::organization::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Organization.def()
    }
}

impl Related<super::user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::widget::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Widget.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
