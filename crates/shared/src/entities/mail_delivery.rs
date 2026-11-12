//! Durable row for `SendDigest`/`SendMail`, shared by `task_type` (`send_digest` | `send_mail`).
//!
//! `send_digest` only needs `entity_id` (the `user_id`);
//! `send_mail` needs the full `to_email`/`subject`/`body_html` payload so a lost Redis message is
//! rebuildable from this row alone. No `attempt_number`: neither task type has a bounded-retry
//! loop, so the stale-pending sweep is the sole recovery path for both.

use sea_orm::entity::prelude::*;
use sea_orm::ActiveValue::Set;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "mail_deliveries")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub task_type: String,
    /// `send_digest`: the recipient `user_id`. `send_mail`: unused (`None`).
    pub entity_id: Option<i64>,
    pub to_email: Option<String>,
    pub subject: Option<String>,
    pub body_html: Option<String>,
    pub body_location: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub worker_id: Option<String>,
    pub lease_expires_at: Option<DateTimeWithTimeZone>,
    pub started_at: Option<DateTimeWithTimeZone>,
    pub finished_at: Option<DateTimeWithTimeZone>,
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

/// Insert a not-yet-attempted `send_digest` row and return its id, BEFORE the LPUSH.
pub async fn insert_pending_digest(
    db: &impl sea_orm::ConnectionTrait,
    org_id: i64,
    user_id: i64,
) -> Result<i64, sea_orm::DbErr> {
    let row = ActiveModel {
        org_id: Set(org_id),
        task_type: Set("send_digest".to_string()),
        entity_id: Set(Some(user_id)),
        status: Set("pending".to_string()),
        ..Default::default()
    }
    .insert(db)
    .await?;
    Ok(row.id)
}

/// Insert a not-yet-attempted `send_mail` row and return its id, BEFORE the LPUSH — the full
/// payload is stored so the sweep can rebuild the `SendMail` message from Postgres alone.
pub async fn insert_pending_mail(
    db: &impl sea_orm::ConnectionTrait,
    org_id: i64,
    to: &str,
    subject: &str,
    body_html: &str,
) -> Result<i64, sea_orm::DbErr> {
    let row = ActiveModel {
        org_id: Set(org_id),
        task_type: Set("send_mail".to_string()),
        to_email: Set(Some(to.to_string())),
        subject: Set(Some(subject.to_string())),
        body_html: Set(Some(body_html.to_string())),
        status: Set("pending".to_string()),
        ..Default::default()
    }
    .insert(db)
    .await?;
    Ok(row.id)
}
