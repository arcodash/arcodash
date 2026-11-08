//! Capability-ceiling authz integration tests for the data-source object family.

mod common;
use common::{make_org, user_with_role};

use arcodash_backend::authz::authorize::{evaluate, Action, Caller, DataSourceVerb};
use arcodash_backend::services::data_source_service;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

async fn make_data_source(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
) -> arcodash_core::entities::data_source::Model {
    arcodash_core::test_support::new_data_source(org_id)
        .insert(db)
        .await
        .expect("data source insert failed")
}

async fn assign_group(db: &sea_orm::DatabaseConnection, data_source_id: i64, group_id: i64) {
    use arcodash_core::entities::data_source_group;
    data_source_group::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        data_source_id: Set(data_source_id),
        group_id: Set(group_id),
        created_at: sea_orm::ActiveValue::NotSet,
        updated_at: sea_orm::ActiveValue::NotSet,
    }
    .insert(db)
    .await
    .expect("data_source_group insert failed");
}

/// Groups default unrestricted: flip this one to restricted so its reach depends solely on its
/// allowlist.
async fn restrict_group(db: &sea_orm::DatabaseConnection, group_id: i64) {
    use arcodash_core::entities::group;
    let g = group::Entity::find_by_id(group_id)
        .one(db)
        .await
        .unwrap()
        .expect("group not found");
    let mut active: group::ActiveModel = g.into();
    active.data_source_access_restricted = Set(true);
    active.update(db).await.expect("group restrict failed");
}

/// `edit_data_source` capability but a restricted group with no allowlist row -> edit denied. The
/// service's CRUD does no authz itself, so tests call `authorize` directly like
/// `handlers/data_sources.rs`.
#[tokio::test]
async fn edit_data_source_out_of_scope_is_denied() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let ds = make_data_source(&app.db, org_id).await;

    let (user, _) = user_with_role(&app.db, org_id, &["edit_data_source"]).await;
    restrict_group(&app.db, user.groups[0]).await; // restricted, no allowlist row -> out of scope

    let denied = arcodash_backend::authz::authorize::authorize(
        &app.db,
        &user,
        Action::DataSource {
            verb: DataSourceVerb::Edit,
            id: ds.id,
        },
    )
    .await;
    assert!(
        denied.is_err(),
        "edit_data_source capability with a restricted, out-of-scope group must be denied edit"
    );
}

/// Same capability, but the restricted group has the source on its allowlist -> allowed.
#[tokio::test]
async fn edit_data_source_in_scope_via_allowlist_is_allowed() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let ds = make_data_source(&app.db, org_id).await;

    let (user, _) = user_with_role(&app.db, org_id, &["edit_data_source"]).await;
    restrict_group(&app.db, user.groups[0]).await;
    assign_group(&app.db, ds.id, user.groups[0]).await;

    let allowed = arcodash_backend::authz::authorize::authorize(
        &app.db,
        &user,
        Action::DataSource {
            verb: DataSourceVerb::Edit,
            id: ds.id,
        },
    )
    .await;
    assert!(
        allowed.is_ok(),
        "edit_data_source capability with the source on the group's allowlist must be allowed"
    );
}

/// A role without `view_data_source` never sees the data source even in an unrestricted group:
/// capability is the ceiling.
#[tokio::test]
async fn view_less_capability_hides_data_source_from_list_and_get() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let ds = make_data_source(&app.db, org_id).await;

    let (user, _) = user_with_role(&app.db, org_id, &[]).await; // no view_data_source

    let list = data_source_service::list_for_user(&app.db, org_id, &user)
        .await
        .expect("list must not error");
    assert!(
        list.is_empty(),
        "view_data_source-less role must see nothing in the list, even though its group is unrestricted"
    );

    let denied = data_source_service::get_for_user(&app.db, org_id, &user, ds.id).await;
    assert!(
        denied.is_err(),
        "view_data_source-less role must be denied a direct get too"
    );
}

/// The mirror-image positive case: `view_data_source` + an unrestricted group (the default) can
/// list and get.
#[tokio::test]
async fn view_data_source_in_scope_can_list_and_get() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let ds = make_data_source(&app.db, org_id).await;

    let (user, _) = user_with_role(&app.db, org_id, &["view_data_source"]).await;

    let list = data_source_service::list_for_user(&app.db, org_id, &user)
        .await
        .expect("list must not error");
    assert_eq!(
        list.len(),
        1,
        "view_data_source with an unrestricted group must see the DS in the list"
    );

    data_source_service::get_for_user(&app.db, org_id, &user, ds.id)
        .await
        .expect("view_data_source with an unrestricted group must be allowed to get");
}

/// The `caller`-block matrix for data sources. Reach is binary — in-scope maps to Editor
/// (never Viewer), so the query-vs-manage split comes entirely from the capability half.
#[tokio::test]
async fn caller_block_matches_authz_matrix() {
    use arcodash_backend::authz::access::ObjectRole;

    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;
    let ds = make_data_source(&app.db, org_id).await;

    // In scope (unrestricted group, the default) + edit_data_source capability -> Editor, can_edit
    // true.
    let (in_scope_with_cap, _) = user_with_role(&app.db, org_id, &["edit_data_source"]).await;
    let eval = evaluate(
        &app.db,
        &in_scope_with_cap,
        Action::DataSource {
            verb: DataSourceVerb::Edit,
            id: ds.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Editor));
    assert!(eval.can_edit);
    let caller = Caller::from(eval);
    assert_eq!(caller.object_role, "editor");
    assert!(caller.can_edit && !caller.can_delete); // Editor reach can edit but not delete
    assert_eq!(
        caller.can_share, None,
        "data sources have no can_share field"
    );

    // In scope WITHOUT edit_data_source -> Editor role (reach is binary), can_edit false
    // (capability ceiling denies).
    let (in_scope_no_cap, _) = user_with_role(&app.db, org_id, &[]).await;
    let eval = evaluate(
        &app.db,
        &in_scope_no_cap,
        Action::DataSource {
            verb: DataSourceVerb::Edit,
            id: ds.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Editor));
    assert!(
        !eval.can_edit,
        "in-scope without edit_data_source must deny edit"
    );

    // Restricted group, no allowlist row + edit_data_source -> None role, can_edit false:
    // capability never substitutes for reach.
    let (out_of_scope, _) = user_with_role(&app.db, org_id, &["edit_data_source"]).await;
    restrict_group(&app.db, out_of_scope.groups[0]).await;
    let eval = evaluate(
        &app.db,
        &out_of_scope,
        Action::DataSource {
            verb: DataSourceVerb::Edit,
            id: ds.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, None);
    assert!(
        !eval.can_edit,
        "restricted, out-of-scope group must deny edit even with capability"
    );
    let caller = Caller::from(eval);
    assert_eq!(caller.object_role, "none");
    assert!(!caller.can_edit && !caller.can_delete);

    // admin -> Owner, can_edit true, regardless of scope.
    let (admin, _) = user_with_role(&app.db, org_id, &["admin"]).await;
    let eval = evaluate(
        &app.db,
        &admin,
        Action::DataSource {
            verb: DataSourceVerb::Edit,
            id: ds.id,
        },
    )
    .await
    .expect("evaluate failed");
    assert_eq!(eval.object_role, Some(ObjectRole::Owner));
    assert!(eval.can_edit);
}

/// Bare client with no cookies or CSRF.
fn bare() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(false)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Invite a new user, accept the invitation, and log in; returns `(user_id, authed_client)`.
async fn invite_and_login(
    app: &common::TestApp,
    admin: &reqwest::Client,
    email: &str,
    name: &str,
    password: &str,
    group_ids: Vec<i64>,
) -> (i64, reqwest::Client) {
    let invite_resp = admin
        .post(format!("{}/api/users", app.address))
        .json(&serde_json::json!({
            "name": name,
            "email": email,
            "groups": group_ids
        }))
        .send()
        .await
        .unwrap();
    let status = invite_resp.status().as_u16();
    let body_text = invite_resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "invite user: {body_text}");
    let invite_body: serde_json::Value = serde_json::from_str(&body_text)
        .unwrap_or_else(|e| panic!("invite body not JSON ({e}): {body_text}"));
    let user_id = invite_body["id"].as_i64().expect("invite must return id");

    let token = {
        use arcodash_core::entities::user;
        let u = user::Entity::find_by_id(user_id)
            .one(&app.db)
            .await
            .expect("db query failed")
            .expect("invited user must exist");
        u.details["invitation_token"]
            .as_str()
            .expect("invitation_token must be in details")
            .to_string()
    };

    let accept_client = bare();
    let sess = accept_client
        .get(format!("{}/api/session", app.address))
        .send()
        .await
        .unwrap();
    let csrf = common::extract_set_cookie(sess.headers(), "csrf_token").unwrap();

    let accept_resp = accept_client
        .post(format!("{}/api/users/invitation/accept", app.address))
        .header("Cookie", format!("csrf_token={csrf}"))
        .header("X-CSRF-Token", &csrf)
        .json(&serde_json::json!({
            "token": token,
            "name": name,
            "password": password
        }))
        .send()
        .await
        .unwrap();
    let status = accept_resp.status().as_u16();
    let body_text = accept_resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "accept invitation: {body_text}");

    let (session, csrf_login) = app.login(email, password).await;
    let client = common::authed_client(&app.address, &session, &csrf_login);

    (user_id, client)
}

/// Over real HTTP: a user in a restricted group granting `edit_data_source` (DS not on its
/// allowlist) plus an unrelated unrestricted group granting nothing is denied every edit-tier verb
/// (update, delete, test-by-id, pause, resume, refresh-schema) and sees `can_edit: false`. Once the
/// DS is on the granting group's allowlist, the verbs succeed.
#[tokio::test]
async fn mutation_verbs_blocked_when_capability_and_reach_on_different_groups() {
    let app = skip_if_none!(common::spawn_app().await);
    app.cleanup().await;

    let admin = app.admin_client().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();

    let grp_a_resp = admin
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": format!("ds-mut-capable-restricted-{suffix}") }))
        .send()
        .await
        .unwrap();
    assert_eq!(grp_a_resp.status().as_u16(), 200);
    let group_a_id = grp_a_resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();

    // Also `view_data_source` (view stays loose via the unrestricted Group B) so the DS shows up in
    // the list; the coupled gate under test is `edit_data_source`.
    let role_resp = admin
        .post(format!("{}/api/roles", app.address))
        .json(&serde_json::json!({
            "name": format!("ds-mut-role-{suffix}"),
            "permissions": ["view_data_source", "edit_data_source"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(role_resp.status().as_u16(), 200);
    let role_id = role_resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();
    let role_assign_resp = admin
        .post(format!("{}/api/groups/{group_a_id}/roles", app.address))
        .json(&serde_json::json!({ "role_id": role_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(role_assign_resp.status().as_u16(), 200);

    let restrict_resp = admin
        .put(format!(
            "{}/api/groups/{group_a_id}/data_source_access",
            app.address
        ))
        .json(&serde_json::json!({ "restricted": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(restrict_resp.status().as_u16(), 200);

    let grp_b_resp = admin
        .post(format!("{}/api/groups", app.address))
        .json(&serde_json::json!({ "name": format!("ds-mut-unrestricted-powerless-{suffix}") }))
        .send()
        .await
        .unwrap();
    assert_eq!(grp_b_resp.status().as_u16(), 200);
    let group_b_id = grp_b_resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();
    // New groups default to restricted; make Group B's unrestricted state explicit.
    let unrestrict_b_resp = admin
        .put(format!(
            "{}/api/groups/{group_b_id}/data_source_access",
            app.address
        ))
        .json(&serde_json::json!({ "restricted": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(unrestrict_b_resp.status().as_u16(), 200);

    let ds_resp = admin
        .post(format!("{}/api/data_sources", app.address))
        .json(&serde_json::json!({ "name": format!("ds-mut-target-{suffix}"), "type": "pg", "options": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(ds_resp.status().as_u16(), 200);
    let ds_id = ds_resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();

    let (uid, user_client) = invite_and_login(
        &app,
        &admin,
        &format!("ds-mutation-gap-{suffix}@test.local"),
        "DS Mutation Gap User",
        "P@ssw0rd!",
        vec![group_a_id, group_b_id],
    )
    .await;

    // Every edit-tier verb denied (404 — object-bound denials hide existence per `deny()`).
    let update_resp = user_client
        .post(format!("{}/api/data_sources/{ds_id}", app.address))
        .json(&serde_json::json!({ "name": "renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(update_resp.status().as_u16(), 404, "update must be denied");

    let test_resp = user_client
        .post(format!("{}/api/data_sources/test", app.address))
        .json(&serde_json::json!({ "id": ds_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        test_resp.status().as_u16(),
        404,
        "test-by-id must be denied"
    );

    let pause_resp = user_client
        .post(format!("{}/api/data_sources/{ds_id}/pause", app.address))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(pause_resp.status().as_u16(), 404, "pause must be denied");

    let resume_resp = user_client
        .post(format!("{}/api/data_sources/{ds_id}/resume", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resume_resp.status().as_u16(), 404, "resume must be denied");

    let refresh_resp = user_client
        .get(format!(
            "{}/api/data_sources/{ds_id}/schema?refresh=true",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        refresh_resp.status().as_u16(),
        404,
        "refresh-schema must be denied"
    );

    // A denied delete leaves the row intact, so it's safe to probe before the allowed-phase delete.
    let delete_resp = user_client
        .delete(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(delete_resp.status().as_u16(), 404, "delete must be denied");

    let list_resp = user_client
        .get(format!("{}/api/data_sources", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(list_resp.status().as_u16(), 200);
    let list_body: serde_json::Value = list_resp.json().await.unwrap();
    let row = list_body["data_sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"].as_i64() == Some(ds_id))
        .expect("data source must appear in list (unrestricted group B gives it reach)");
    assert_eq!(row["caller"]["can_edit"].as_bool(), Some(false));

    let get_resp = user_client
        .get(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(get_resp.status().as_u16(), 200);
    let get_body: serde_json::Value = get_resp.json().await.unwrap();
    assert_eq!(get_body["caller"]["can_edit"].as_bool(), Some(false));
    assert!(
        get_body.get("options").is_none() || get_body["options"].is_null(),
        "options must stay hidden from a caller who can't edit"
    );

    // Add the DS to Group A's own allowlist — capability and reach now land on the same group.
    let assign_resp = admin
        .post(format!(
            "{}/api/groups/{group_a_id}/data_sources",
            app.address
        ))
        .json(&serde_json::json!({ "data_source_id": ds_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(assign_resp.status().as_u16(), 200);

    // Test-by-id and refresh-schema still hit a real (unreachable) connector, so only assert authz
    // no longer blocks them (not 404).
    let update_resp = user_client
        .post(format!("{}/api/data_sources/{ds_id}", app.address))
        .json(&serde_json::json!({ "name": "renamed-ok" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        update_resp.status().as_u16(),
        200,
        "update must succeed once the DS is on Group A's allowlist"
    );

    let test_resp = user_client
        .post(format!("{}/api/data_sources/test", app.address))
        .json(&serde_json::json!({ "id": ds_id }))
        .send()
        .await
        .unwrap();
    assert_ne!(
        test_resp.status().as_u16(),
        404,
        "test-by-id must no longer be blocked by authz"
    );

    let pause_resp = user_client
        .post(format!("{}/api/data_sources/{ds_id}/pause", app.address))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(pause_resp.status().as_u16(), 200, "pause must succeed");

    let resume_resp = user_client
        .post(format!("{}/api/data_sources/{ds_id}/resume", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(resume_resp.status().as_u16(), 200, "resume must succeed");

    let refresh_resp = user_client
        .get(format!(
            "{}/api/data_sources/{ds_id}/schema?refresh=true",
            app.address
        ))
        .send()
        .await
        .unwrap();
    assert_ne!(
        refresh_resp.status().as_u16(),
        404,
        "refresh-schema must no longer be blocked by authz"
    );

    let list_resp = user_client
        .get(format!("{}/api/data_sources", app.address))
        .send()
        .await
        .unwrap();
    let list_body: serde_json::Value = list_resp.json().await.unwrap();
    let row = list_body["data_sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"].as_i64() == Some(ds_id))
        .expect("data source must still appear in list");
    assert_eq!(row["caller"]["can_edit"].as_bool(), Some(true));

    let delete_resp = user_client
        .delete(format!("{}/api/data_sources/{ds_id}", app.address))
        .send()
        .await
        .unwrap();
    assert_eq!(delete_resp.status().as_u16(), 204, "delete must succeed");

    // Cleanup: remove the user from the groups so they can be deleted.
    let _ = admin
        .delete(format!(
            "{}/api/groups/{group_a_id}/members/{uid}",
            app.address
        ))
        .send()
        .await;
    let _ = admin
        .delete(format!(
            "{}/api/groups/{group_b_id}/members/{uid}",
            app.address
        ))
        .send()
        .await;
    let _ = admin
        .delete(format!("{}/api/groups/{group_a_id}", app.address))
        .send()
        .await;
    let _ = admin
        .delete(format!("{}/api/groups/{group_b_id}", app.address))
        .send()
        .await;
}
