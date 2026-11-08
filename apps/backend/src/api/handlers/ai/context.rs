use arcodash_core::entities::query;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use crate::db::Db;
use crate::error::ApiError;

pub use arcodash_core::ai::context::*;

/// The caller's most recent non-archived saved queries on this source.
pub(crate) async fn load_recent_queries(
    db: &Db,
    org_id: i64,
    user_id: i64,
    data_source_id: i64,
) -> Result<Vec<RecentQuery>, ApiError> {
    Ok(query::Entity::find()
        .filter(query::Column::OrgId.eq(org_id))
        .filter(query::Column::UserId.eq(user_id))
        .filter(query::Column::DataSourceId.eq(data_source_id))
        .filter(query::Column::IsArchived.eq(false))
        .order_by_desc(query::Column::UpdatedAt)
        .limit(RECENT_LIMIT)
        .all(db)
        .await?
        .into_iter()
        .map(|q| RecentQuery {
            name: q.name,
            sql: q.query,
        })
        .collect())
}
