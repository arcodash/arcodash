use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "data_sources")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    #[sea_orm(column_name = "type")]
    pub r#type: String,
    pub encrypted_options: Option<Vec<u8>>,
    pub scheduled_queue_name: Option<String>,
    pub schema: Option<Json>,
    pub schema_refreshed_at: Option<DateTimeWithTimeZone>,
    /// Set when the data source has been paused; `None` means active. A paused source refuses
    /// new query executions — see [`Model::pause_refusal`].
    pub paused_at: Option<DateTimeWithTimeZone>,
    /// Optional human-readable reason shown alongside the paused state.
    pub pause_reason: Option<String>,
    /// Independent of `paused_at` (On/Off): "unknown" (never used) | "ok" | "failing". Never
    /// written by polling — only by a query run connecting/failing to connect
    /// (`apps/worker/src/handlers/execute_query.rs`) or a create/update connectivity probe
    /// (`apps/backend/src/services/data_source_service.rs`).
    pub health_state: String,
    pub health_error: Option<String>,
    pub health_checked_at: Option<DateTimeWithTimeZone>,
    /// Sampled-data digest for AI prompts; typed as `DataSourceAiProfile`. Never part of `schema`.
    pub ai_profile: Option<Json>,
    pub ai_profile_built_at: Option<DateTimeWithTimeZone>,
    /// Whole-job failure of the last profile run; `None` after a clean run.
    pub ai_profile_last_error: Option<String>,
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
    #[sea_orm(has_many = "super::data_source_group::Entity")]
    DataSourceGroup,
    #[sea_orm(has_many = "super::query::Entity")]
    Query,
    #[sea_orm(has_many = "super::query_result::Entity")]
    QueryResult,
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

impl Related<super::query::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Query.def()
    }
}

impl Related<super::query_result::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::QueryResult.def()
    }
}

impl Related<super::group::Entity> for Entity {
    fn to() -> RelationDef {
        super::data_source_group::Relation::Group.def()
    }

    fn via() -> Option<RelationDef> {
        Some(super::data_source_group::Relation::DataSource.def().rev())
    }
}

impl Model {
    /// Why a paused source refuses new executions; `None` while it is active. Shared by the
    /// backend's `require_not_paused` and the worker's execution chokepoint so both binaries
    /// speak the same refusal message.
    #[must_use]
    pub fn pause_refusal(&self) -> Option<String> {
        self.paused_at?;
        Some(match &self.pause_reason {
            Some(reason) if !reason.trim().is_empty() => {
                format!("this data source is paused: {reason}")
            }
            _ => "this data source is paused".to_string(),
        })
    }
}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Model {
        let now = chrono::Utc::now().into();
        Model {
            id: 1,
            org_id: 1,
            name: "test".to_string(),
            r#type: "pg".to_string(),
            encrypted_options: None,
            scheduled_queue_name: None,
            schema: None,
            schema_refreshed_at: None,
            paused_at: None,
            pause_reason: None,
            health_state: "unknown".to_string(),
            health_error: None,
            health_checked_at: None,
            ai_profile: None,
            ai_profile_built_at: None,
            ai_profile_last_error: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn pause_refusal_is_none_while_active() {
        assert_eq!(base().pause_refusal(), None);
    }

    #[test]
    fn pause_refusal_uses_the_bare_message_when_reason_is_absent_or_blank() {
        let mut ds = base();
        ds.paused_at = Some(chrono::Utc::now().into());
        assert_eq!(
            ds.pause_refusal().as_deref(),
            Some("this data source is paused")
        );

        ds.pause_reason = Some("   ".to_string());
        assert_eq!(
            ds.pause_refusal().as_deref(),
            Some("this data source is paused")
        );
    }

    #[test]
    fn pause_refusal_includes_the_reason_when_present() {
        let mut ds = base();
        ds.paused_at = Some(chrono::Utc::now().into());
        ds.pause_reason = Some("maintenance window".to_string());
        assert_eq!(
            ds.pause_refusal().as_deref(),
            Some("this data source is paused: maintenance window")
        );
    }
}
