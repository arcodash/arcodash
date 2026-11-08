use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "access_permissions")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Polymorphic target type (e.g. "query", "dashboard") — no `Relation` variant, plain
    /// columns only.
    pub object_type: String,
    pub object_id: i64,
    pub access_type: String,
    pub grantor_id: i64,
    /// The grantee user, when this is a user grant. Exactly one of `grantee_id` /
    /// `grantee_group_id` is set per row.
    pub grantee_id: Option<i64>,
    /// The grantee group, when this is a group grant. Exactly one of `grantee_id` /
    /// `grantee_group_id` is set per row.
    pub grantee_group_id: Option<i64>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::GrantorId",
        to = "super::user::Column::Id"
    )]
    Grantor,
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::GranteeId",
        to = "super::user::Column::Id"
    )]
    Grantee,
    #[sea_orm(
        belongs_to = "super::group::Entity",
        from = "Column::GranteeGroupId",
        to = "super::group::Column::Id"
    )]
    GranteeGroup,
}

impl ActiveModelBehavior for ActiveModel {}
