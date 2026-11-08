//! Capability-ceiling authz integration tests for the dashboard + widget object family.

mod common;
use common::{make_org, user_with_role};

use arcodash_backend::authz::authorize::{
    authorize, evaluate, Action, Caller, DashboardVerb, InheritedVerb,
};
use arcodash_backend::services::{dashboard_service, permission_service, widget_service};
use sea_orm::ActiveModelTrait;

async fn make_dashboard(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    owner: &arcodash_core::entities::user::Model,
) -> arcodash_core::entities::dashboard::Model {
    let active = arcodash_core::test_support::new_dashboard(org_id, owner.id);
    active.insert(db).await.expect("dashboard insert failed")
}

/// Canonical example: a role holding ONLY `view_dashboard`, given
/// a `modify` (Editor) grant on dashboard D, is still denied edit — capability wins over a share
/// grant. Viewing, however, is allowed.
#[tokio::test]
async fn canonical_view_only_role_with_modify_grant_cannot_edit_but_can_view() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["create_dashboard", "edit_dashboard"]).await;
    let d = dashboard_service::create(&app.db, org_id, &owner, "Owner's Dashboard".to_string())
        .await
        .expect("create failed");

    let (grantee, _) = user_with_role(&app.db, org_id, &["view_dashboard"]).await;
    permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "dashboard",
        d.id,
        Some(grantee.id),
        None,
        "modify",
    )
    .await
    .expect("grant failed");

    let edit_denied = authorize(
        &app.db,
        &grantee,
        Action::Dashboard {
            verb: DashboardVerb::Edit,
            id: d.id,
        },
    )
    .await;
    assert!(
        edit_denied.is_err(),
        "REQUIRED canonical example: view_dashboard-only role with a modify grant must be DENIED edit"
    );

    let view_allowed = authorize(
        &app.db,
        &grantee,
        Action::Dashboard {
            verb: DashboardVerb::View,
            id: d.id,
        },
    )
    .await;
    assert!(
        view_allowed.is_ok(),
        "the same role must still be ALLOWED to view"
    );
}

/// Dashboard creation is gated on `create_dashboard`.
#[tokio::test]
async fn create_requires_create_dashboard_capability() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (no_cap, _) = user_with_role(&app.db, org_id, &[]).await;
    let denied = dashboard_service::create(&app.db, org_id, &no_cap, "Nope".to_string()).await;
    assert!(
        denied.is_err(),
        "create_dashboard capability must be required"
    );

    let (has_cap, _) = user_with_role(&app.db, org_id, &["create_dashboard"]).await;
    dashboard_service::create(&app.db, org_id, &has_cap, "Yes".to_string())
        .await
        .expect("create_dashboard holder must be allowed to create");
}

/// An Editor grant that ALSO carries `edit_dashboard` successfully edits: grants are honored,
/// not ignored by the owner-or-admin check.
#[tokio::test]
async fn editor_grant_with_edit_dashboard_capability_can_now_edit() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["create_dashboard", "edit_dashboard"]).await;
    let d = make_dashboard(&app.db, org_id, &owner).await;

    let (grantee, _) = user_with_role(&app.db, org_id, &["edit_dashboard", "view_dashboard"]).await;
    permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "dashboard",
        d.id,
        Some(grantee.id),
        None,
        "editor",
    )
    .await
    .expect("grant failed");

    let updated = dashboard_service::update(
        &app.db,
        org_id,
        &grantee,
        &d.slug,
        Some("Edited By Grantee".to_string()),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("Editor grant + edit_dashboard capability must be allowed to edit");
    assert_eq!(updated.name, "Edited By Grantee");
}

/// Owner-ceiling: an owner whose role is view-only is denied edit on their own dashboard.
#[tokio::test]
async fn owner_with_view_only_role_is_denied_edit() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["create_dashboard", "view_dashboard"]).await;
    let d = dashboard_service::create(&app.db, org_id, &owner, "Mine".to_string())
        .await
        .expect("create failed");

    let denied = dashboard_service::update(
        &app.db,
        org_id,
        &owner,
        &d.slug,
        Some("Renamed".to_string()),
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(
        denied.is_err(),
        "owner-ceiling: a view-only role must not let an owner edit their own dashboard"
    );

    let denied_archive = dashboard_service::archive(&app.db, org_id, &owner, &d.slug).await;
    assert!(denied_archive.is_err());
}

/// Reads: a role without `view_dashboard` cannot see a dashboard it otherwise has reach to.
#[tokio::test]
async fn read_requires_view_dashboard_capability() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    // `edit_dashboard` on top of `create_dashboard`: the owner must hold edit capability to
    // share, separate from what this test actually exercises (the grantee's own
    // `view_dashboard` gate).
    let (owner, _) = user_with_role(&app.db, org_id, &["create_dashboard", "edit_dashboard"]).await;
    let d = dashboard_service::create(&app.db, org_id, &owner, "Readable".to_string())
        .await
        .expect("create failed");

    let (no_view_cap, _) = user_with_role(&app.db, org_id, &[]).await;
    permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "dashboard",
        d.id,
        Some(no_view_cap.id),
        None,
        "viewer",
    )
    .await
    .expect("grant failed");

    let denied = dashboard_service::get_by_slug(&app.db, org_id, &no_view_cap, &d.slug).await;
    assert!(
        denied.is_err(),
        "a Viewer grant without view_dashboard capability must not be able to read"
    );

    let list = dashboard_service::list(&app.db, org_id, &no_view_cap, false, None, None, None)
        .await
        .expect("list must not error, just come back empty");
    assert!(
        list.is_empty(),
        "view_dashboard-less role must see nothing in the list"
    );
}

// ── Widgets inherit the parent dashboard's action ────────────────────────────────

#[tokio::test]
async fn widget_create_and_edit_inherit_parent_dashboard_edit_gate() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["create_dashboard", "edit_dashboard"]).await;
    let d = dashboard_service::create(&app.db, org_id, &owner, "Widget Host".to_string())
        .await
        .expect("create failed");

    // A view-only grantee cannot add a widget.
    let (viewer, _) = user_with_role(&app.db, org_id, &["view_dashboard"]).await;
    permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "dashboard",
        d.id,
        Some(viewer.id),
        None,
        "viewer",
    )
    .await
    .expect("grant failed");
    let denied = widget_service::create(
        &app.db,
        org_id,
        &viewer,
        d.id,
        None,
        Some("hello".to_string()),
        1,
        serde_json::json!({}),
    )
    .await;
    assert!(
        denied.is_err(),
        "Viewer reach must not be enough to add a widget"
    );

    // The owner (Editor+ reach and capability) can.
    let widget = widget_service::create(
        &app.db,
        org_id,
        &owner,
        d.id,
        None,
        Some("hello".to_string()),
        1,
        serde_json::json!({}),
    )
    .await
    .expect("owner with edit_dashboard must be allowed to add a widget");

    // Update inherits the same gate.
    let denied_update = widget_service::update(
        &app.db,
        org_id,
        &viewer,
        widget.id,
        Some("nope".to_string()),
        None,
        None,
    )
    .await;
    assert!(denied_update.is_err());

    widget_service::update(
        &app.db,
        org_id,
        &owner,
        widget.id,
        Some("updated text".to_string()),
        None,
        None,
    )
    .await
    .expect("owner must be allowed to update the widget");
}

/// Direct action-spec check: `InheritedVerb::View`/`Edit` resolve through the dashboard's own
/// gate (redundant with the service-level test above, but pins the mapping itself).
#[tokio::test]
async fn widget_action_resolves_to_parent_dashboard_gate() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["create_dashboard", "edit_dashboard"]).await;
    let d = dashboard_service::create(&app.db, org_id, &owner, "Direct".to_string())
        .await
        .expect("create failed");

    let allowed = authorize(
        &app.db,
        &owner,
        Action::Widget {
            verb: InheritedVerb::Edit,
            dashboard_id: d.id,
        },
    )
    .await;
    assert!(allowed.is_ok());
}

// ── `caller` block ──────────────────────────────────────────

/// The full `caller`-block matrix for dashboards, including `can_share`. Real enforcement
/// (`embed_service::create_embed` + `require_can_share_object`) is `create_embed` capability AND
/// creator-or-admin reach — NOT `can_edit`, which every Editor holds regardless of ownership.
/// The Editor-grant case below pins this: an Editor grantee with `edit_dashboard`
/// AND `create_embed` can edit, but is neither the creator nor an admin, so `can_share` must be
/// `false` even though `can_edit` is `true` (otherwise the UI offers a share action that 403s).
#[tokio::test]
async fn caller_block_matches_authz_matrix_and_can_share() {
    use arcodash_backend::authz::access::ObjectRole;

    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    // owner-with-capability, holding create_embed -> Owner, can_edit/can_delete/can_share all true.
    let (owner, _) = user_with_role(
        &app.db,
        org_id,
        &["create_dashboard", "edit_dashboard", "create_embed"],
    )
    .await;
    let d = make_dashboard(&app.db, org_id, &owner).await;
    let eval = evaluate(
        &app.db,
        &owner,
        Action::Dashboard {
            verb: DashboardVerb::Edit,
            id: d.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Owner));
    assert!(eval.can_edit);
    let caller = Caller::from(eval).with_share(true, true);
    assert_eq!(caller.object_role, "owner");
    assert!(caller.can_edit && caller.can_delete && caller.can_share == Some(true));

    // owner-without-capability (ceiling) -> Owner, can_edit/can_delete false. can_share depends
    // only on create_embed + creator-or-admin, not can_edit — this owner is still the creator, so
    // can_share tracks whether they separately hold create_embed (they don't here).
    let org_id_2 = make_org(&app.db).await;
    let (owner_view_only, _) =
        user_with_role(&app.db, org_id_2, &["create_dashboard", "view_dashboard"]).await;
    let d2 = make_dashboard(&app.db, org_id_2, &owner_view_only).await;
    let eval = evaluate(
        &app.db,
        &owner_view_only,
        Action::Dashboard {
            verb: DashboardVerb::Edit,
            id: d2.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Owner));
    assert!(
        !eval.can_edit,
        "owner-ceiling: view-only role must deny edit despite ownership"
    );
    let caller = Caller::from(eval).with_share(false, true);
    assert!(!caller.can_edit && !caller.can_delete && caller.can_share == Some(false));

    // Editor grant + edit_dashboard + create_embed, but NOT the creator or an admin -> can_edit
    // true, can_share false: `can_share` must not simply follow `can_edit`, or `POST /api/embeds`
    // would 403 this exact caller.
    let (editor_grantee, _) = user_with_role(
        &app.db,
        org_id,
        &["edit_dashboard", "view_dashboard", "create_embed"],
    )
    .await;
    arcodash_backend::services::permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "dashboard",
        d.id,
        Some(editor_grantee.id),
        None,
        "editor",
    )
    .await
    .expect("grant failed");
    let eval = evaluate(
        &app.db,
        &editor_grantee,
        Action::Dashboard {
            verb: DashboardVerb::Edit,
            id: d.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Editor));
    assert!(eval.can_edit);
    let caller = Caller::from(eval).with_share(true, false);
    assert!(
        caller.can_edit && caller.can_share == Some(false),
        "an Editor grantee who is neither creator nor admin must not be told they can share"
    );

    // Editor grant WITHOUT edit_dashboard capability -> Editor role, can_edit false, can_share
    // false (missing create_embed too).
    let (editor_grantee_no_cap, _) = user_with_role(&app.db, org_id, &["view_dashboard"]).await;
    arcodash_backend::services::permission_service::grant(
        &app.db,
        org_id,
        &owner,
        "dashboard",
        d.id,
        Some(editor_grantee_no_cap.id),
        None,
        "editor",
    )
    .await
    .expect("grant failed");
    let eval = evaluate(
        &app.db,
        &editor_grantee_no_cap,
        Action::Dashboard {
            verb: DashboardVerb::Edit,
            id: d.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Editor));
    assert!(
        !eval.can_edit,
        "Editor grant without capability must deny edit"
    );
    let caller = Caller::from(eval).with_share(false, false);
    assert!(!caller.can_edit && caller.can_share == Some(false));

    // admin -> Owner, can_edit true, on a dashboard the admin neither owns nor was granted.
    // Admin implies every permission, so both `with_share` inputs are true for an admin caller.
    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let eval = evaluate(
        &app.db,
        &admin,
        Action::Dashboard {
            verb: DashboardVerb::Edit,
            id: d.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Owner));
    assert!(eval.can_edit);
    let caller = Caller::from(eval).with_share(true, true);
    assert_eq!(caller.can_share, Some(true));

    // no-access -> None, everything false. Dashboards default to `workspace` visibility (like
    // monitors — see `access::object_role_for`'s dashboard arm), so force `restricted` here;
    // otherwise the org-open default alone would hand every org member Viewer reach.
    use sea_orm::ActiveValue::Set;
    let mut active: arcodash_core::entities::dashboard::ActiveModel = d.clone().into();
    active.visibility = Set("restricted".to_string());
    let d_restricted = active
        .update(&app.db)
        .await
        .expect("visibility update failed");

    let (stranger, _) =
        user_with_role(&app.db, org_id, &["edit_dashboard", "view_dashboard"]).await;
    let eval = evaluate(
        &app.db,
        &stranger,
        Action::Dashboard {
            verb: DashboardVerb::Edit,
            id: d_restricted.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, None);
    assert!(!eval.can_edit);
    let caller = Caller::from(eval).with_share(false, false);
    assert_eq!(caller.object_role, "none");
    assert!(!caller.can_edit && caller.can_share == Some(false));
}
