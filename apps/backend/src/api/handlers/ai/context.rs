use arcodash_core::entities::query;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use crate::db::Db;
use crate::error::ApiError;

pub use arcodash_core::ai::context::*;

/// The data source's profile with sampled values restored from the org's object storage.
///
/// # Errors
///
/// An unreachable or switching store surfaces as 503/409.
pub async fn load_usable_profile(
    state: &crate::state::AppState,
    org_id: i64,
    ds: &arcodash_core::entities::data_source::Model,
) -> Result<Option<arcodash_core::models::DataSourceAiProfile>, ApiError> {
    let storage = state.storage.resolve(&state.db, org_id).await?;
    match usable_profile_loaded(ds, &storage).await {
        Ok(p) => Ok(p),
        Err(e) => {
            arcodash_core::ai::narrative::report_unreachable(
                &*state.storage,
                &state.db,
                org_id,
                &e,
            )
            .await;
            Err(e.into())
        }
    }
}

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
