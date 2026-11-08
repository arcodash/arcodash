use sea_orm::entity::prelude::*;

/// Per-org embed governance.
///
/// One row per org, `org_id` itself is the primary key. A missing row means the all-default-safe
/// values (`public_urls_enabled = true`, no secret) — see
/// `services::embed_service::get_org_embed_settings`.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "org_embed_settings")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub org_id: i64,
    /// Soft, reversible, non-destructive global pause for this org's public/embed URLs (gate #2
    /// in `resolve_public_embed`). Toggling this off/on never touches any `embeds` row.
    pub public_urls_enabled: bool,
    /// `signed` mode only — per-org HMAC signing secret, encrypted at rest, rotatable. `None`
    /// until signed embedding is first configured for this org.
    pub embedding_secret: Option<Vec<u8>>,
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
}

impl Related<super::organization::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Organization.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
