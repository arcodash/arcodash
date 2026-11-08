//! Capability-ceiling authz integration tests for the monitor object family.

mod common;
use common::{make_org, set_role_permissions, user_with_role};

use arcodash_backend::authz::authorize::{evaluate, Action, Caller, MonitorVerb};
use arcodash_backend::services::{monitor_service, query_service};
use sea_orm::{ActiveModelTrait, EntityTrait};

/// Build a published query + monitor owned by `owner`, both in `org_id`.
async fn make_monitor(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    owner: &arcodash_core::entities::user::Model,
) -> arcodash_core::entities::monitor::Model {
    use arcodash_core::entities::data_source;

    let ds_id = data_source::Entity::insert(arcodash_core::test_support::new_data_source(org_id))
        .exec(db)
        .await
        .expect("data source insert failed")
        .last_insert_id;
    let mut q_active = arcodash_core::test_support::new_query(org_id, ds_id, owner.id);
    // `new_query`'s fixed api_key would collide across this suite's queries (api_key is globally
    // unique), so use a unique value.
    q_active.api_key = sea_orm::ActiveValue::Set(uuid::Uuid::new_v4().simple().to_string());
    let q = q_active.insert(db).await.expect("query insert failed");
    let published = query_service::publish(db, owner, q, 0)
        .await
        .expect("publish failed");

    monitor_service::create(
        db,
        org_id,
        owner,
        published.id,
        "Test Monitor".to_string(),
        serde_json::json!({ "op": ">", "value": 1 }),
        None,
    )
    .await
    .expect("monitor create failed")
}

/// Owner ceiling: stripping the owner's role down to `view_monitor` denies update/delete on their
/// own monitor. Capability AND reach are required.
#[tokio::test]
async fn owner_without_edit_monitor_capability_is_denied_on_own_monitor() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, role_id) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m = make_monitor(&app.db, org_id, &owner).await;

    set_role_permissions(&app.db, role_id, &["view_monitor", "view_query"]).await;

    let denied = monitor_service::update(
        &app.db,
        &owner,
        m.clone(),
        Some("renamed".to_string()),
        None,
        None,
    )
    .await;
    assert!(
        denied.is_err(),
        "REQUIRED owner-ceiling: owner without edit_monitor must be denied update on their own monitor"
    );

    let denied_delete = monitor_service::delete(&app.db, &owner, m).await;
    assert!(
        denied_delete.is_err(),
        "REQUIRED owner-ceiling: owner without edit_monitor must be denied delete"
    );
}

#[tokio::test]
async fn edit_monitor_capability_without_reach_is_denied() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m = make_monitor(&app.db, org_id, &owner).await;

    let (stranger, _) = user_with_role(
        &app.db,
        org_id,
        &["edit_monitor", "view_monitor", "view_query"],
    )
    .await;
    let denied =
        monitor_service::update(&app.db, &stranger, m, Some("nope".to_string()), None, None).await;
    assert!(
        denied.is_err(),
        "edit_monitor capability without reach on this specific monitor must be denied"
    );
}

#[tokio::test]
async fn owner_with_edit_monitor_capability_succeeds() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m = make_monitor(&app.db, org_id, &owner).await;

    let updated = monitor_service::update(
        &app.db,
        &owner,
        m.clone(),
        Some("Renamed".to_string()),
        None,
        None,
    )
    .await
    .expect("owner with edit_monitor must be allowed to update");
    assert_eq!(updated.name, "Renamed");

    monitor_service::set_muted(&app.db, &owner, updated, true)
        .await
        .expect("owner with edit_monitor must be allowed to mute");
}

/// `delete()` gates on the Owner-tier delete verb: an Editor-tier grantee can rename/mute but not
/// delete; owner and admin can.
#[tokio::test]
async fn editor_grantee_cannot_delete_monitor_owner_and_admin_can() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m = make_monitor(&app.db, org_id, &owner).await;

    let (editor_grantee, _) =
        user_with_role(&app.db, org_id, &["edit_monitor", "view_monitor"]).await;
    arcodash_backend::services::permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "monitor",
        m.id,
        Some(editor_grantee.id),
        None,
        "editor",
    )
    .await
    .expect("grant failed");

    let denied = monitor_service::delete(&app.db, &editor_grantee, m.clone()).await;
    assert!(
        denied.is_err(),
        "an Editor-tier grantee must be denied delete (Owner-tier reach required)"
    );

    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let admin_deleted = monitor_service::delete(&app.db, &admin, m.clone()).await;
    assert!(admin_deleted.is_ok(), "admin must still be able to delete");

    let m2 = make_monitor(&app.db, org_id, &owner).await;
    let owner_deleted = monitor_service::delete(&app.db, &owner, m2).await;
    assert!(
        owner_deleted.is_ok(),
        "the owner (edit_monitor capability + Owner reach) must still be able to delete"
    );
}

/// ack/subscribe/unsubscribe stay view-tier: a `view_monitor`-only user can still do them.
#[tokio::test]
async fn ack_and_subscribe_are_view_tier_not_edit_tier() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let mut m = make_monitor(&app.db, org_id, &owner).await;

    use sea_orm::ActiveValue::Set;
    let mut active: arcodash_core::entities::monitor::ActiveModel = m.clone().into();
    active.state = Set("triggered".to_string());
    m = active.update(&app.db).await.expect("state update failed");

    let (viewer, _) = user_with_role(&app.db, org_id, &["view_monitor", "view_query"]).await;
    permission_service_grant_view(&app.db, org_id, &owner, "monitor", m.id, viewer.id).await;
    // A monitor grant doesn't imply reach on its query (that needs data-source access the viewer
    // lacks), so grant the query explicitly too.
    permission_service_grant_view(&app.db, org_id, &owner, "query", m.query_id, viewer.id).await;

    monitor_service::ack(&app.db, org_id, &viewer, m.id)
        .await
        .expect("view_monitor-only role must be allowed to ack");

    let queue: std::sync::Arc<dyn arcodash_core::queue::Queue> =
        std::sync::Arc::new(arcodash_core::queue::ValkeyQueue::new(app.redis.clone()));
    monitor_service::subscribe(
        &arcodash_backend::db::Db::Pool(app.db.clone()),
        queue,
        None,
        org_id,
        &viewer,
        &m,
        None,
    )
    .await
    .expect("view_monitor-only role must be allowed to subscribe");
}

/// The `caller`-block matrix (owner ceiling, Editor grant with/without capability, admin,
/// no-access). Monitors default to `workspace` visibility, so the no-access case needs an
/// explicitly `restricted` monitor.
#[tokio::test]
async fn caller_block_matches_authz_matrix() {
    use arcodash_backend::authz::access::ObjectRole;
    use sea_orm::ActiveValue::Set;

    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "edit_monitor",
            "view_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m = make_monitor(&app.db, org_id, &owner).await;

    let eval = evaluate(
        &app.db,
        &owner,
        Action::Monitor {
            verb: MonitorVerb::Edit,
            id: m.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Owner));
    assert!(eval.can_edit);
    assert_eq!(Caller::from(eval).object_role, "owner");

    let (owner_view_only, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m2 = make_monitor(&app.db, org_id, &owner_view_only).await;
    let eval = evaluate(
        &app.db,
        &owner_view_only,
        Action::Monitor {
            verb: MonitorVerb::Edit,
            id: m2.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Owner));
    assert!(
        !eval.can_edit,
        "owner-ceiling: view-only role must deny edit despite ownership"
    );
    let caller = Caller::from(eval);
    assert!(!caller.can_edit && !caller.can_delete);

    let (editor_grantee, _) =
        user_with_role(&app.db, org_id, &["edit_monitor", "view_monitor"]).await;
    arcodash_backend::services::permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "monitor",
        m.id,
        Some(editor_grantee.id),
        None,
        "editor",
    )
    .await
    .expect("grant failed");
    let eval = evaluate(
        &app.db,
        &editor_grantee,
        Action::Monitor {
            verb: MonitorVerb::Edit,
            id: m.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Editor));
    assert!(eval.can_edit);

    let (editor_grantee_no_cap, _) = user_with_role(&app.db, org_id, &["view_monitor"]).await;
    arcodash_backend::services::permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "monitor",
        m.id,
        Some(editor_grantee_no_cap.id),
        None,
        "editor",
    )
    .await
    .expect("grant failed");
    let eval = evaluate(
        &app.db,
        &editor_grantee_no_cap,
        Action::Monitor {
            verb: MonitorVerb::Edit,
            id: m.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Editor));
    assert!(
        !eval.can_edit,
        "Editor grant without capability must deny edit"
    );

    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let eval = evaluate(
        &app.db,
        &admin,
        Action::Monitor {
            verb: MonitorVerb::Edit,
            id: m.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Owner));
    assert!(eval.can_edit);

    // No-access -> None, can_edit false. Force `restricted`, since `workspace` gives every member
    // Viewer reach.
    let mut active: arcodash_core::entities::monitor::ActiveModel = m.clone().into();
    active.visibility = Set("restricted".to_string());
    let m_restricted = active
        .update(&app.db)
        .await
        .expect("visibility update failed");
    let (stranger, _) = user_with_role(&app.db, org_id, &["edit_monitor", "view_monitor"]).await;
    let eval = evaluate(
        &app.db,
        &stranger,
        Action::Monitor {
            verb: MonitorVerb::Edit,
            id: m_restricted.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, None);
    assert!(!eval.can_edit);
    let caller = Caller::from(eval);
    assert_eq!(caller.object_role, "none");
    assert!(!caller.can_edit && !caller.can_delete);
}

async fn permission_service_grant_view(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    owner: &arcodash_core::entities::user::Model,
    object_type: &str,
    object_id: i64,
    grantee_id: i64,
) {
    arcodash_backend::services::permission_service::grant(
        db,
        org_id,
        owner,
        object_type,
        object_id,
        Some(grantee_id),
        None,
        "viewer",
    )
    .await
    .expect("grant failed");
}

/// `put_escalation` is edit-tier: a `view_monitor`-only user must not rewrite a monitor's
/// escalation chain, owner or not.
#[tokio::test]
async fn put_escalation_requires_edit_monitor_capability() {
    use arcodash_core::entities::notification_destination;

    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, role_id) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m = make_monitor(&app.db, org_id, &owner).await;

    let dest = notification_destination::ActiveModel {
        org_id: sea_orm::ActiveValue::Set(org_id),
        name: sea_orm::ActiveValue::Set("esc-dest".to_string()),
        r#type: sea_orm::ActiveValue::Set("webhook".to_string()),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .expect("destination insert failed");

    set_role_permissions(&app.db, role_id, &["view_monitor", "view_query"]).await;

    let denied =
        monitor_service::put_escalation(&app.db, org_id, &owner, m.id, vec![(5, dest.id)]).await;
    assert!(
        denied.is_err(),
        "owner without edit_monitor must be denied rewriting the escalation chain"
    );
}

async fn set_group_restricted(db: &sea_orm::DatabaseConnection, group_id: i64) {
    use arcodash_core::entities::group;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

    let g = group::Entity::find_by_id(group_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut active: group::ActiveModel = g.into();
    active.data_source_access_restricted = Set(true);
    active.update(db).await.unwrap();
}

/// A viewer with a monitor grant but no reach to the query's data source must get the SAME denial
/// status as the query check, and the message must not name "query" (no existence leak).
#[tokio::test]
async fn get_for_user_monitor_grant_without_query_reach_hides_query_denial() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m = make_monitor(&app.db, org_id, &owner).await;

    let (viewer, _) = user_with_role(&app.db, org_id, &["view_monitor", "view_query"]).await;
    // Restrict the viewer's group and leave the data source off its allowlist.
    set_group_restricted(&app.db, viewer.groups[0]).await;

    permission_service_grant_view(&app.db, org_id, &owner, "monitor", m.id, viewer.id).await;

    let query_denial = query_service::get_for_user(&app.db, org_id, &viewer, m.query_id).await;
    let expected_status = query_denial
        .expect_err("viewer must not have query reach via a DS-restricted group")
        .status;

    let denied = monitor_service::get_for_user(&app.db, org_id, &viewer, m.id)
        .await
        .expect_err("monitor must still be denied without query reach");
    assert_eq!(
        denied.status, expected_status,
        "denial status must be unchanged from the underlying query check"
    );
    assert!(
        !denied.message.to_lowercase().contains("query"),
        "denial message must not leak that the underlying object is a 'query': {}",
        denied.message
    );
}

/// Positive control: full access (monitor grant + query reach) still succeeds.
#[tokio::test]
async fn get_for_user_succeeds_with_full_access() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m = make_monitor(&app.db, org_id, &owner).await;

    // The default group is unrestricted; grant explicit ACLs on the monitor and its query as for a
    // shared-object viewer.
    let (viewer, _) = user_with_role(&app.db, org_id, &["view_monitor", "view_query"]).await;
    permission_service_grant_view(&app.db, org_id, &owner, "monitor", m.id, viewer.id).await;
    permission_service_grant_view(&app.db, org_id, &owner, "query", m.query_id, viewer.id).await;

    let result = monitor_service::get_for_user(&app.db, org_id, &viewer, m.id).await;
    assert!(
        result.is_ok(),
        "full access (monitor + query grants) must still succeed: {:?}",
        result.err()
    );
}

fn test_queue(app: &common::TestApp) -> std::sync::Arc<dyn arcodash_core::queue::Queue> {
    std::sync::Arc::new(arcodash_core::queue::ValkeyQueue::new(app.redis.clone()))
}

/// `notify_subscription` ("Send now") must deny a caller who neither owns the subscription nor has
/// Edit-tier reach; org-scoping is not an authz check.
#[tokio::test]
async fn notify_subscription_denies_neither_owner_nor_edit_tier() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m = make_monitor(&app.db, org_id, &owner).await;

    let (subscriber, _) = user_with_role(&app.db, org_id, &["view_monitor", "view_query"]).await;
    let db = arcodash_backend::db::Db::Pool(app.db.clone());
    let queue = test_queue(&app);
    let sub = monitor_service::subscribe(&db, queue.clone(), None, org_id, &subscriber, &m, None)
        .await
        .expect("subscribe failed");

    let (stranger, _) = user_with_role(&app.db, org_id, &["view_monitor", "view_query"]).await;
    let denied =
        monitor_service::notify_subscription(&db, queue, None, org_id, &stranger, m.id, sub.id)
            .await;
    let err = denied.expect_err(
        "a caller who is neither the subscriber nor Edit-tier on the monitor must be denied",
    );
    assert_eq!(err.status, axum::http::StatusCode::NOT_FOUND);
}

/// The subscription's owner may always "Send now", even with only view-tier reach.
#[tokio::test]
async fn notify_subscription_allows_the_subscription_owner() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m = make_monitor(&app.db, org_id, &owner).await;

    let (subscriber, _) = user_with_role(&app.db, org_id, &["view_monitor", "view_query"]).await;
    let db = arcodash_backend::db::Db::Pool(app.db.clone());
    let queue = test_queue(&app);
    let sub = monitor_service::subscribe(&db, queue.clone(), None, org_id, &subscriber, &m, None)
        .await
        .expect("subscribe failed");

    monitor_service::notify_subscription(&db, queue, None, org_id, &subscriber, m.id, sub.id)
        .await
        .expect("the subscription's own owner must be allowed to send now, view-tier only");
}

/// A subscriber whose monitor access was revoked after subscribing is denied "Send now": owning the
/// row is not enough.
#[tokio::test]
async fn notify_subscription_denies_the_owner_once_their_monitor_access_is_revoked() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m = make_monitor(&app.db, org_id, &owner).await;

    let (subscriber, role_id) =
        user_with_role(&app.db, org_id, &["view_monitor", "view_query"]).await;
    let db = arcodash_backend::db::Db::Pool(app.db.clone());
    let queue = test_queue(&app);
    let sub = monitor_service::subscribe(&db, queue.clone(), None, org_id, &subscriber, &m, None)
        .await
        .expect("subscribe failed");

    // Access revoked after subscribing; the role now grants nothing monitor-related.
    set_role_permissions(&app.db, role_id, &["view_query"]).await;

    let denied =
        monitor_service::notify_subscription(&db, queue, None, org_id, &subscriber, m.id, sub.id)
            .await;
    assert!(
        denied.is_err(),
        "a subscriber who has since lost monitor access must be denied, even on their own subscription"
    );
}

/// A non-subscriber with Edit-tier reach (`edit_monitor` AND an Editor+ grant) may also "Send now".
#[tokio::test]
async fn notify_subscription_allows_edit_tier_caller_who_is_not_the_subscriber() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m = make_monitor(&app.db, org_id, &owner).await;

    let (subscriber, _) = user_with_role(&app.db, org_id, &["view_monitor", "view_query"]).await;
    let db = arcodash_backend::db::Db::Pool(app.db.clone());
    let queue = test_queue(&app);
    let sub = monitor_service::subscribe(&db, queue.clone(), None, org_id, &subscriber, &m, None)
        .await
        .expect("subscribe failed");

    let (editor, _) = user_with_role(&app.db, org_id, &["edit_monitor", "view_monitor"]).await;
    arcodash_backend::services::permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "monitor",
        m.id,
        Some(editor.id),
        None,
        "editor",
    )
    .await
    .expect("grant failed");

    monitor_service::notify_subscription(&db, queue, None, org_id, &editor, m.id, sub.id)
        .await
        .expect("Edit-tier caller who isn't the subscriber must be allowed to send now");
}

/// A `Queue` whose `enqueue` always fails.
struct FailingQueue;

#[async_trait::async_trait]
impl arcodash_core::queue::Queue for FailingQueue {
    async fn enqueue(
        &self,
        _queue: arcodash_core::queue::QueueName,
        _job: arcodash_core::queue::JobMessage,
    ) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("queue unavailable"))
    }
}

/// A failed enqueue in "Send now" must surface as a real error where possible (no ambient
/// transaction deferring the job), not a silent success.
#[tokio::test]
async fn notify_subscription_surfaces_a_real_enqueue_failure_in_pool_mode() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m = make_monitor(&app.db, org_id, &owner).await;

    let db = arcodash_backend::db::Db::Pool(app.db.clone());
    let sub = monitor_service::subscribe(&db, test_queue(&app), None, org_id, &owner, &m, None)
        .await
        .expect("subscribe failed");

    let failing_queue: std::sync::Arc<dyn arcodash_core::queue::Queue> =
        std::sync::Arc::new(FailingQueue);
    let result = monitor_service::notify_subscription(
        &db,
        failing_queue,
        None,
        org_id,
        &owner,
        m.id,
        sub.id,
    )
    .await;
    assert!(
        result.is_err(),
        "an enqueue failure must be a real error, not a silent success"
    );
}

/// The observations route gates on `monitor_service::get_for_user`: without `view_monitor` is
/// refused.
#[tokio::test]
async fn observations_route_requires_monitor_view() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &[
            "create_monitor",
            "view_monitor",
            "edit_monitor",
            "execute_query",
            "view_query",
            "edit_query",
        ],
    )
    .await;
    let m = make_monitor(&app.db, org_id, &owner).await;

    let (stranger, _) = user_with_role(&app.db, org_id, &["view_query"]).await;
    assert!(
        monitor_service::get_for_user(&app.db, org_id, &stranger, m.id)
            .await
            .is_err()
    );

    let (viewer, _) = user_with_role(&app.db, org_id, &["view_monitor", "view_query"]).await;
    permission_service_grant_view(&app.db, org_id, &owner, "monitor", m.id, viewer.id).await;
    permission_service_grant_view(&app.db, org_id, &owner, "query", m.query_id, viewer.id).await;
    assert!(
        monitor_service::get_for_user(&app.db, org_id, &viewer, m.id)
            .await
            .is_ok()
    );
}
