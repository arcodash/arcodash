use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "users")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    pub email: String,
    pub password_hash: Option<String>,
    pub groups: Vec<i64>,
    /// SHA-256 of the user's personal API key. The key itself is never stored, so it can only be
    /// shown at generation time.
    pub api_key_hash: String,
    pub disabled_at: Option<DateTimeWithTimeZone>,
    pub is_invitation_pending: bool,
    pub profile_image_url: Option<String>,
    pub details: Json,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
    /// Opt-in for the daily digest email.
    pub digest_enabled: bool,
    /// Random `UUIDv4`, regenerated on every digest enable; `None` when the digest is off. Powers
    /// the one-click unsubscribe link (`GET /api/digest/unsubscribe?token=...`).
    pub digest_unsubscribe_token: Option<String>,
    /// Local send hour (0-23) for the daily digest. `None` means never explicitly set — the
    /// first enable must set this together with `digest_timezone` rather than leaving it unset.
    pub digest_hour: Option<i16>,
    /// IANA timezone (e.g. `Europe/Berlin`) the digest send hour is interpreted in. `None` means
    /// never explicitly set.
    pub digest_timezone: Option<String>,
    /// Bumped by `reset_password`/`change_password`. A session cookie's JWT carries the version
    /// it was issued under; a mismatch here means the credential changed since, so the cookie no
    /// longer resolves — see `auth::resolve_from_session_cookie`.
    pub session_token_version: i32,
    /// Watermark for the digest send window — `None` until the first confirmed send.
    pub digest_last_sent_at: Option<DateTimeWithTimeZone>,
}

impl Model {
    /// Whether this account may exercise any access at all. A disabled or never-accepted row
    /// keeps its groups, grants and subscriptions — every gate must read this, not just the
    /// row's existence.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.disabled_at.is_none() && !self.is_invitation_pending
    }
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::organization::Entity",
        from = "Column::OrgId",
        to = "super::organization::Column::Id"
    )]
    Organization,
    #[sea_orm(has_many = "super::query::Entity")]
    Query,
    #[sea_orm(has_many = "super::dashboard::Entity")]
    Dashboard,
    #[sea_orm(has_many = "super::monitor::Entity")]
    Monitor,
    #[sea_orm(has_many = "super::monitor_subscription::Entity")]
    MonitorSubscription,
    #[sea_orm(has_many = "super::notification_destination::Entity")]
    NotificationDestination,
    #[sea_orm(has_many = "super::query_snippet::Entity")]
    QuerySnippet,
    #[sea_orm(has_many = "super::favorite::Entity")]
    Favorite,
    #[sea_orm(has_many = "super::change::Entity")]
    Change,
    #[sea_orm(has_many = "super::event::Entity")]
    Event,
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

impl Related<super::dashboard::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Dashboard.def()
    }
}

impl Related<super::monitor::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Monitor.def()
    }
}

impl Related<super::monitor_subscription::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::MonitorSubscription.def()
    }
}

impl Related<super::notification_destination::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::NotificationDestination.def()
    }
}

impl Related<super::query_snippet::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::QuerySnippet.def()
    }
}

impl Related<super::favorite::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Favorite.def()
    }
}

impl Related<super::change::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Change.def()
    }
}

impl Related<super::event::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Event.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
