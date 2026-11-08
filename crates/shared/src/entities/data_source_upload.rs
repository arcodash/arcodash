//! An uploaded CSV/SQLite file backing a file-upload data source.
//!
//! `object_key` points at the actual bytes in `ObjectStore` (`crate::storage`); this row is what a
//! connector resolves an `upload_id` to before fetching them. `data_source_id` is NULL until the
//! upload is linked to a saved data source, and is cleared (not cascaded) when that data source is
//! deleted — the worker's orphan sweep reclaims rows/objects that stay NULL past its age cutoff.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "data_source_uploads")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub org_id: i64,
    pub object_key: String,
    pub filename: String,
    pub byte_size: i64,
    pub content_type: String,
    pub created_by: Option<i64>,
    pub data_source_id: Option<i64>,
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
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::CreatedBy",
        to = "super::user::Column::Id"
    )]
    User,
    #[sea_orm(
        belongs_to = "super::data_source::Entity",
        from = "Column::DataSourceId",
        to = "super::data_source::Column::Id"
    )]
    DataSource,
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

impl Related<super::data_source::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DataSource.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
