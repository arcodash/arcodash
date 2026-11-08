use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "groups")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    pub permissions: Vec<String>,
    #[sea_orm(column_name = "type")]
    pub r#type: String,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
    /// `false` (default) = sees & queries every data source in the org. `true` = scoped to this
    /// group's `data_source_groups` allowlist only.
    pub data_source_access_restricted: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::organization::Entity",
        from = "Column::OrgId",
        to = "super::organization::Column::Id"
    )]
    Organization,
    #[sea_orm(has_many = "super::data_source_group::Entity")]
    DataSourceGroup,
    #[sea_orm(has_many = "super::group_role::Entity")]
    GroupRole,
}

impl Related<super::organization::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Organization.def()
    }
}

impl Related<super::data_source_group::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DataSourceGroup.def()
    }
}

impl Related<super::group_role::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::GroupRole.def()
    }
}

impl Related<super::data_source::Entity> for Entity {
    fn to() -> RelationDef {
        super::data_source_group::Relation::DataSource.def()
    }

    fn via() -> Option<RelationDef> {
        Some(super::data_source_group::Relation::Group.def().rev())
    }
}

impl ActiveModelBehavior for ActiveModel {}
