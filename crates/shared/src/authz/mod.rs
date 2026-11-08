//! Authorization primitives usable outside an HTTP request — group capability resolution
//! (`permissions`), per-object sharing + visibility (`access`), and per-group data-source reach
//! (`data_source`).
//!
//! The backend's own `authz` module re-exports these; the logic lives here so the worker (which has
//! no HTTP-facing `ApiError` and no request context) can reuse it.

pub mod access;
pub mod data_source;
pub mod permissions;

use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter};

use crate::entities::{dashboard, data_source_group, monitor, query, user};

use access::ObjectRole;

/// `true` iff `user` can still see `monitor_id`.
///
/// Applies the same two gates `monitor_service::get_for_user` does (the monitor's underlying query
/// must be viewable, and the monitor itself must clear `view_monitor` capability ∩ Viewer reach),
/// expressed for callers outside the request path.
///
/// Fail-closed: a missing/cross-org monitor or query is `false`.
pub async fn can_view_monitor(
    db: &impl ConnectionTrait,
    user: &user::Model,
    monitor_id: i64,
) -> Result<bool, DbErr> {
    if !user.is_active() {
        return Ok(false);
    }

    let Some(m) = monitor::Entity::find_by_id(monitor_id)
        .filter(monitor::Column::OrgId.eq(user.org_id))
        .one(db)
        .await?
    else {
        return Ok(false);
    };

    let groups = permissions::load_effective_groups(db, user).await?;
    let is_admin = permissions::is_admin(&groups);
    if is_admin {
        return Ok(true);
    }

    if !permissions::user_has_permission(user, &groups, "view_query")
        || !access::can_view_object_with_groups(db, user, &groups, is_admin, "query", m.query_id)
            .await?
    {
        return Ok(false);
    }

    if !permissions::user_has_permission(user, &groups, "view_monitor") {
        return Ok(false);
    }
    Ok(
        access::object_role_for_with_groups(db, user, &groups, is_admin, "monitor", monitor_id)
            .await?
            >= Some(ObjectRole::Viewer),
    )
}

/// `true` iff `user` can see `dashboard_id`: active, same org, and either admin or holding
/// `view_dashboard` with at least Viewer reach on it. Fail-closed on missing/cross-org rows.
pub async fn can_view_dashboard(
    db: &impl ConnectionTrait,
    user: &user::Model,
    dashboard_id: i64,
) -> Result<bool, DbErr> {
    if !user.is_active() {
        return Ok(false);
    }
    if dashboard::Entity::find_by_id(dashboard_id)
        .filter(dashboard::Column::OrgId.eq(user.org_id))
        .one(db)
        .await?
        .is_none()
    {
        return Ok(false);
    }

    let groups = permissions::load_effective_groups(db, user).await?;
    let is_admin = permissions::is_admin(&groups);
    if is_admin {
        return Ok(true);
    }
    if !permissions::user_has_permission(user, &groups, "view_dashboard") {
        return Ok(false);
    }
    Ok(
        access::object_role_for_with_groups(db, user, &groups, is_admin, "dashboard", dashboard_id)
            .await?
            >= Some(ObjectRole::Viewer),
    )
}

/// `true` iff `user` may read the stored results of `query_id`.
///
/// The query must be viewable and a single group must grant both `execute_query` and reach to the
/// data source currently serving it (the published one once the query is published).
///
/// Mirrors the backend's `get_for_user` + `result_service::get` pair for callers outside the
/// request path.
pub async fn can_read_query_results(
    db: &impl ConnectionTrait,
    user: &user::Model,
    query_id: i64,
) -> Result<bool, DbErr> {
    if !user.is_active() {
        return Ok(false);
    }
    let Some(q) = query::Entity::find_by_id(query_id)
        .filter(query::Column::OrgId.eq(user.org_id))
        .one(db)
        .await?
    else {
        return Ok(false);
    };

    let groups = permissions::load_effective_groups(db, user).await?;
    let is_admin = permissions::is_admin(&groups);
    if is_admin {
        return Ok(true);
    }
    if !permissions::user_has_permission(user, &groups, "view_query")
        || !access::can_view_object_with_groups(db, user, &groups, is_admin, "query", query_id)
            .await?
    {
        return Ok(false);
    }

    let ds_id = q
        .published_change_id
        .and(q.published_data_source_id)
        .unwrap_or(q.data_source_id);
    let dsgs = data_source_group::Entity::find()
        .filter(data_source_group::Column::DataSourceId.eq(ds_id))
        .all(db)
        .await?;
    Ok(data_source::any_group_grants_with_reach(
        user,
        &groups,
        "execute_query",
        ds_id,
        &dsgs,
    ))
}

/// Postgres fixtures shared by the `authz` DB tests, which skip when `TEST_DATABASE_URL` is unset.
#[cfg(test)]
mod test_db {
    use crate::entities::{access_permission, dashboard, data_source, monitor, query, user};
    use crate::org_seed::seed_builtin_groups;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, DatabaseConnection, DbBackend, Statement};
    use sea_orm::{ConnectionTrait, EntityTrait};

    pub(super) use crate::test_support::test_db as setup;

    pub(super) struct OrgFixture {
        pub(super) org_id: i64,
        pub(super) default_group_id: i64,
        pub(super) viewer_group_id: i64,
        pub(super) admin_group_id: i64,
    }

    pub(super) async fn seed_org(db: &DatabaseConnection, slug: &str) -> OrgFixture {
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
        let org = crate::entities::organization::ActiveModel {
            name: Set(slug.to_string()),
            slug: Set(slug.to_string()),
            settings: Set(serde_json::json!({})),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
        let groups = seed_builtin_groups(db, org.id).await.unwrap();
        OrgFixture {
            org_id: org.id,
            default_group_id: groups.default_group_id,
            viewer_group_id: groups.viewer_group_id,
            admin_group_id: groups.admin_group_id,
        }
    }

    pub(super) async fn seed_user(
        db: &DatabaseConnection,
        org_id: i64,
        groups: Vec<i64>,
    ) -> user::Model {
        let suffix = uuid::Uuid::new_v4();
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
        user::ActiveModel {
            org_id: Set(org_id),
            name: Set("authz-test-user".to_string()),
            email: Set(format!("authz-test-{suffix}@test.local")),
            password_hash: Set(None),
            groups: Set(groups),
            api_key_hash: Set(crate::hash::hash_api_key(&suffix.to_string())),
            disabled_at: Set(None),
            is_invitation_pending: Set(false),
            profile_image_url: Set(None),
            details: Set(serde_json::json!({})),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    pub(super) async fn seed_data_source(
        db: &DatabaseConnection,
        org_id: i64,
    ) -> data_source::Model {
        let suffix = uuid::Uuid::new_v4();
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
        data_source::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("authz-test-ds-{suffix}")),
            r#type: Set("pg".to_string()),
            encrypted_options: Set(None),
            scheduled_queue_name: Set(None),
            schema: Set(None),
            schema_refreshed_at: Set(None),
            paused_at: Set(None),
            pause_reason: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    /// `published`: backed by a throwaway `changes` row, as in a real publish.
    pub(super) async fn seed_query(
        db: &DatabaseConnection,
        org_id: i64,
        ds_id: i64,
        owner_id: i64,
        visibility: &str,
        published: bool,
    ) -> query::Model {
        let suffix = uuid::Uuid::new_v4();
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
        let q = query::ActiveModel {
            org_id: Set(org_id),
            data_source_id: Set(ds_id),
            name: Set(format!("authz-test-query-{suffix}")),
            description: Set(None),
            query: Set("SELECT 1".to_string()),
            query_hash: Set(format!("hash-{suffix}")),
            api_key: Set(suffix.to_string()),
            user_id: Set(owner_id),
            last_modified_by_id: Set(None),
            latest_query_data_id: Set(None),
            is_archived: Set(false),
            visibility: Set(visibility.to_string()),
            options: Set(serde_json::json!({})),
            tags: Set(None),
            last_execution_status: Set(String::new()),
            last_execution_at: Set(None),
            row_count: Set(None),
            last_execution_error: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();

        if !published {
            return q;
        }

        let change_row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "INSERT INTO changes (object_type, object_id, user_id, change, created_at, updated_at) \
                 VALUES ('query', $1, $2, $3, now(), now()) RETURNING id",
                [q.id.into(), owner_id.into(), serde_json::json!({}).into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let change_id: i64 = change_row.try_get("", "id").unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE queries SET published_change_id = $1, published_at = now() WHERE id = $2",
            [change_id.into(), q.id.into()],
        ))
        .await
        .unwrap();

        query::Entity::find_by_id(q.id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    pub(super) async fn seed_dashboard(
        db: &DatabaseConnection,
        org_id: i64,
        owner_id: i64,
        visibility: &str,
        published: bool,
    ) -> dashboard::Model {
        let suffix = uuid::Uuid::new_v4();
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
        dashboard::ActiveModel {
            org_id: Set(org_id),
            slug: Set(format!("authz-test-dash-{suffix}")),
            name: Set(format!("authz-test-dashboard-{suffix}")),
            user_id: Set(owner_id),
            layout: Set(serde_json::json!([])),
            dashboard_filters_enabled: Set(false),
            is_archived: Set(false),
            visibility: Set(visibility.to_string()),
            tags: Set(None),
            options: Set(serde_json::json!({})),
            published_snapshot: Set(published.then(|| serde_json::json!({}))),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    pub(super) async fn seed_monitor(
        db: &DatabaseConnection,
        org_id: i64,
        query_id: i64,
        owner_id: i64,
        visibility: &str,
    ) -> monitor::Model {
        let suffix = uuid::Uuid::new_v4();
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
        monitor::ActiveModel {
            org_id: Set(org_id),
            name: Set(format!("authz-test-monitor-{suffix}")),
            query_id: Set(query_id),
            user_id: Set(owner_id),
            options: Set(serde_json::json!({})),
            state: Set("unknown".to_string()),
            visibility: Set(visibility.to_string()),
            last_triggered_at: Set(None),
            rearm: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn grant(
        db: &DatabaseConnection,
        object_type: &str,
        object_id: i64,
        access_type: &str,
        grantor_id: i64,
        grantee_id: Option<i64>,
        grantee_group_id: Option<i64>,
    ) -> access_permission::Model {
        let now = sea_orm::prelude::DateTimeWithTimeZone::from(chrono::Utc::now());
        access_permission::ActiveModel {
            object_type: Set(object_type.to_string()),
            object_id: Set(object_id),
            access_type: Set(access_type.to_string()),
            grantor_id: Set(grantor_id),
            grantee_id: Set(grantee_id),
            grantee_group_id: Set(grantee_group_id),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::test_db::*;
    use super::*;
    use crate::entities::access_permission;
    use sea_orm::{DatabaseConnection, DbBackend, Statement};

    async fn set_group_restricted(db: &DatabaseConnection, group_id: i64, restricted: bool) {
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE groups SET data_source_access_restricted = $1 WHERE id = $2",
            [restricted.into(), group_id.into()],
        ))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn can_view_dashboard_denies_cross_org() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let a = seed_org(&db, &format!("org-cvd-a-{suffix}")).await;
        let b = seed_org(&db, &format!("org-cvd-b-{suffix}")).await;
        let owner = seed_user(&db, a.org_id, vec![a.admin_group_id]).await;
        let outsider = seed_user(&db, b.org_id, vec![b.admin_group_id]).await;
        let d = seed_dashboard(&db, a.org_id, owner.id, "workspace", false).await;

        assert!(can_view_dashboard(&db, &owner, d.id).await.unwrap());
        assert!(!can_view_dashboard(&db, &outsider, d.id).await.unwrap());
    }

    #[tokio::test]
    async fn can_read_query_results_denies_without_execute_reach() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let f = seed_org(&db, &format!("org-crqr-{suffix}")).await;
        let owner = seed_user(&db, f.org_id, vec![f.default_group_id]).await;
        let viewer = seed_user(&db, f.org_id, vec![f.viewer_group_id]).await;
        let ds = seed_data_source(&db, f.org_id).await;
        let q = seed_query(&db, f.org_id, ds.id, owner.id, "workspace", true).await;

        // Viewer group has no `execute_query` capability: the query is visible but results are not.
        assert!(!can_read_query_results(&db, &viewer, q.id).await.unwrap());
        assert!(!can_read_query_results(&db, &viewer, q.id + 1_000_000)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn admin_reads_everything() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let f = seed_org(&db, &format!("org-adm-{suffix}")).await;
        let admin = seed_user(&db, f.org_id, vec![f.admin_group_id]).await;
        let ds = seed_data_source(&db, f.org_id).await;
        let q = seed_query(&db, f.org_id, ds.id, admin.id, "workspace", false).await;
        let d = seed_dashboard(&db, f.org_id, admin.id, "workspace", false).await;

        assert!(can_read_query_results(&db, &admin, q.id).await.unwrap());
        assert!(can_view_dashboard(&db, &admin, d.id).await.unwrap());
    }

    #[tokio::test]
    async fn owner_with_capability_can_view() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let f = seed_org(&db, &format!("org-cvm-owner-{suffix}")).await;
        let owner = seed_user(&db, f.org_id, vec![f.default_group_id]).await;
        let ds = seed_data_source(&db, f.org_id).await;
        let q = seed_query(&db, f.org_id, ds.id, owner.id, "workspace", false).await;
        let m = seed_monitor(&db, f.org_id, q.id, owner.id, "workspace").await;

        assert!(can_view_monitor(&db, &owner, m.id).await.unwrap());
    }

    #[tokio::test]
    async fn no_groups_cannot_view() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let f = seed_org(&db, &format!("org-cvm-nogroup-{suffix}")).await;
        let owner = seed_user(&db, f.org_id, vec![]).await;
        let ds = seed_data_source(&db, f.org_id).await;
        let q = seed_query(&db, f.org_id, ds.id, owner.id, "workspace", false).await;
        let m = seed_monitor(&db, f.org_id, q.id, owner.id, "workspace").await;

        assert!(
            !can_view_monitor(&db, &owner, m.id).await.unwrap(),
            "capability is the ceiling — ownership alone isn't enough with no groups at all"
        );
    }

    #[tokio::test]
    async fn viewer_group_can_view_a_workspace_monitor() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let f = seed_org(&db, &format!("org-cvm-viewer-{suffix}")).await;
        let owner = seed_user(&db, f.org_id, vec![f.default_group_id]).await;
        let viewer = seed_user(&db, f.org_id, vec![f.viewer_group_id]).await;
        let ds = seed_data_source(&db, f.org_id).await;
        let q = seed_query(&db, f.org_id, ds.id, owner.id, "workspace", true).await;
        let m = seed_monitor(&db, f.org_id, q.id, owner.id, "workspace").await;

        assert!(can_view_monitor(&db, &viewer, m.id).await.unwrap());
    }

    #[tokio::test]
    async fn restricted_monitor_without_a_grant_cannot_be_viewed() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let f = seed_org(&db, &format!("org-cvm-restricted-{suffix}")).await;
        let owner = seed_user(&db, f.org_id, vec![f.default_group_id]).await;
        let viewer = seed_user(&db, f.org_id, vec![f.viewer_group_id]).await;
        let ds = seed_data_source(&db, f.org_id).await;
        let q = seed_query(&db, f.org_id, ds.id, owner.id, "workspace", true).await;
        let m = seed_monitor(&db, f.org_id, q.id, owner.id, "restricted").await;

        assert!(!can_view_monitor(&db, &viewer, m.id).await.unwrap());
    }

    #[tokio::test]
    async fn restricted_monitor_with_a_viewer_grant_can_be_viewed() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let f = seed_org(&db, &format!("org-cvm-grant-{suffix}")).await;
        let owner = seed_user(&db, f.org_id, vec![f.default_group_id]).await;
        let viewer = seed_user(&db, f.org_id, vec![f.viewer_group_id]).await;
        let ds = seed_data_source(&db, f.org_id).await;
        let q = seed_query(&db, f.org_id, ds.id, owner.id, "workspace", true).await;
        let m = seed_monitor(&db, f.org_id, q.id, owner.id, "restricted").await;
        grant(
            &db,
            "monitor",
            m.id,
            "viewer",
            owner.id,
            Some(viewer.id),
            None,
        )
        .await;

        assert!(can_view_monitor(&db, &viewer, m.id).await.unwrap());
    }

    /// The revocation-shaped assertion: the same fixture that just proved `true` above must flip
    /// to `false` the moment the grant row is gone, with nothing else about the fixture touched.
    #[tokio::test]
    async fn revoking_the_grant_flips_it_back() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let f = seed_org(&db, &format!("org-cvm-revoke-{suffix}")).await;
        let owner = seed_user(&db, f.org_id, vec![f.default_group_id]).await;
        let viewer = seed_user(&db, f.org_id, vec![f.viewer_group_id]).await;
        let ds = seed_data_source(&db, f.org_id).await;
        let q = seed_query(&db, f.org_id, ds.id, owner.id, "workspace", true).await;
        let m = seed_monitor(&db, f.org_id, q.id, owner.id, "restricted").await;
        let grant = grant(
            &db,
            "monitor",
            m.id,
            "viewer",
            owner.id,
            Some(viewer.id),
            None,
        )
        .await;
        assert!(
            can_view_monitor(&db, &viewer, m.id).await.unwrap(),
            "sanity: the grant must actually open access before it's revoked"
        );

        access_permission::Entity::delete_by_id(grant.id)
            .exec(&db)
            .await
            .unwrap();

        assert!(!can_view_monitor(&db, &viewer, m.id).await.unwrap());
    }

    /// The vector a monitor-only check misses: the query gate's data-source half. A workspace
    /// monitor on a workspace query is not enough if the caller's only group can't reach the
    /// query's data source.
    #[tokio::test]
    async fn losing_data_source_reach_blocks_a_workspace_monitor() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let f = seed_org(&db, &format!("org-cvm-dsreach-{suffix}")).await;
        let owner = seed_user(&db, f.org_id, vec![f.default_group_id]).await;
        let restricted_viewer = seed_user(&db, f.org_id, vec![f.default_group_id]).await;
        set_group_restricted(&db, f.default_group_id, true).await;
        let ds = seed_data_source(&db, f.org_id).await;
        let q = seed_query(&db, f.org_id, ds.id, owner.id, "workspace", true).await;
        let m = seed_monitor(&db, f.org_id, q.id, owner.id, "workspace").await;
        // No `data_source_groups` row links `default` to `ds` — the group has the capability but
        // no reach.

        assert!(!can_view_monitor(&db, &restricted_viewer, m.id)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn cross_org_monitor_is_not_viewable() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let home = seed_org(&db, &format!("org-cvm-cross-home-{suffix}")).await;
        let other = seed_org(&db, &format!("org-cvm-cross-other-{suffix}")).await;
        let owner = seed_user(&db, other.org_id, vec![other.default_group_id]).await;
        let ds = seed_data_source(&db, other.org_id).await;
        let q = seed_query(&db, other.org_id, ds.id, owner.id, "workspace", true).await;
        let m = seed_monitor(&db, other.org_id, q.id, owner.id, "workspace").await;

        let outsider = seed_user(&db, home.org_id, vec![home.default_group_id]).await;
        assert!(!can_view_monitor(&db, &outsider, m.id).await.unwrap());
    }

    #[tokio::test]
    async fn missing_monitor_is_not_viewable() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let f = seed_org(&db, &format!("org-cvm-missing-{suffix}")).await;
        let u = seed_user(&db, f.org_id, vec![f.default_group_id]).await;

        assert!(!can_view_monitor(&db, &u, 9_999_999_999).await.unwrap());
    }

    #[tokio::test]
    async fn admin_can_view_anything() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let f = seed_org(&db, &format!("org-cvm-admin-{suffix}")).await;
        let owner = seed_user(&db, f.org_id, vec![f.default_group_id]).await;
        let admin = seed_user(&db, f.org_id, vec![f.admin_group_id]).await;
        let ds = seed_data_source(&db, f.org_id).await;
        // Draft, restricted -- irrelevant, since admin short-circuits before either gate.
        let q = seed_query(&db, f.org_id, ds.id, owner.id, "workspace", false).await;
        let m = seed_monitor(&db, f.org_id, q.id, owner.id, "restricted").await;

        assert!(can_view_monitor(&db, &admin, m.id).await.unwrap());
    }

    async fn disable_user(db: &DatabaseConnection, user_id: i64) -> user::Model {
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE users SET disabled_at = now() WHERE id = $1",
            [user_id.into()],
        ))
        .await
        .unwrap();
        user::Entity::find_by_id(user_id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    async fn set_invitation_pending(db: &DatabaseConnection, user_id: i64) -> user::Model {
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE users SET is_invitation_pending = true WHERE id = $1",
            [user_id.into()],
        ))
        .await
        .unwrap();
        user::Entity::find_by_id(user_id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    /// Ownership plus capability plus reach is not enough: access is still denied once the
    /// account is disabled.
    #[tokio::test]
    async fn a_disabled_user_cannot_view_a_monitor_they_own() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let f = seed_org(&db, &format!("org-cvm-disabled-owner-{suffix}")).await;
        let owner = seed_user(&db, f.org_id, vec![f.default_group_id]).await;
        let ds = seed_data_source(&db, f.org_id).await;
        let q = seed_query(&db, f.org_id, ds.id, owner.id, "workspace", false).await;
        let m = seed_monitor(&db, f.org_id, q.id, owner.id, "workspace").await;
        let owner = disable_user(&db, owner.id).await;

        assert!(!can_view_monitor(&db, &owner, m.id).await.unwrap());
    }

    #[tokio::test]
    async fn a_pending_invitation_cannot_view_a_monitor() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let f = seed_org(&db, &format!("org-cvm-pending-{suffix}")).await;
        let owner = seed_user(&db, f.org_id, vec![f.default_group_id]).await;
        let ds = seed_data_source(&db, f.org_id).await;
        let q = seed_query(&db, f.org_id, ds.id, owner.id, "workspace", false).await;
        let m = seed_monitor(&db, f.org_id, q.id, owner.id, "workspace").await;
        let owner = set_invitation_pending(&db, owner.id).await;

        assert!(!can_view_monitor(&db, &owner, m.id).await.unwrap());
    }

    #[tokio::test]
    async fn an_admin_who_is_disabled_cannot_view_anything() {
        let Some(db) = setup().await else { return };
        let suffix = uuid::Uuid::new_v4();
        let f = seed_org(&db, &format!("org-cvm-disabled-admin-{suffix}")).await;
        let owner = seed_user(&db, f.org_id, vec![f.default_group_id]).await;
        let admin = seed_user(&db, f.org_id, vec![f.admin_group_id]).await;
        let ds = seed_data_source(&db, f.org_id).await;
        let q = seed_query(&db, f.org_id, ds.id, owner.id, "workspace", false).await;
        let m = seed_monitor(&db, f.org_id, q.id, owner.id, "restricted").await;
        let admin = disable_user(&db, admin.id).await;

        assert!(
            !can_view_monitor(&db, &admin, m.id).await.unwrap(),
            "the admin short-circuit must not outrank the disabled-account guard"
        );
    }
}
