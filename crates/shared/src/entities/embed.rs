use sea_orm::entity::prelude::*;

/// The unified embed record (`public` token links and `signed` embeds).
/// Rows are immutable except `revoked_at` and are never hard-deleted (audit trail). All fields
/// besides `revoked_at` are set once at creation.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "embeds")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    /// `"dashboard"` | `"visualization"` — polymorphic target, no `Relation` variant (matches
    /// the `api_keys`/`events` convention: free-text + app validation, not a DB enum).
    pub object_type: String,
    pub object_id: i64,
    /// `"public"` | `"signed"`.
    pub mode: String,
    /// The bearer secret for `public` mode. `None` for `signed`, which locks params
    /// cryptographically instead of via a token in the URL.
    pub token: Option<String>,
    /// `public`: frozen `{key: value}` snapshot baked at share time. `signed`: the
    /// allowed key set.
    pub locked_params: Json,
    /// `signed`-mode domain allowlist. `None` = any (`public` embeds are meant to be
    /// frameable anywhere).
    pub frame_ancestors: Option<Json>,
    pub expires_at: Option<DateTimeWithTimeZone>,
    /// Who published this embed (audit). `SET NULL` on user hard-delete so the audit row
    /// survives; gate #4 in `resolve_public_embed` re-checks the *live* user, not this snapshot.
    pub created_by_id: Option<i64>,
    /// Snapshot of the creator's `share_public_anonymous` permission at publish time — consulted
    /// by gate #4 (OSS/anonymous mode only), never re-derived from the creator's current roles.
    pub anon_authorized: bool,
    /// The ONLY mutable field post-create. Set on revoke, never cleared.
    pub revoked_at: Option<DateTimeWithTimeZone>,
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
        from = "Column::CreatedById",
        to = "super::user::Column::Id"
    )]
    CreatedBy,
}

impl Related<super::organization::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Organization.def()
    }
}

impl Related<super::user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::CreatedBy.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
