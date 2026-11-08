//! Defense-in-depth guards for the capability-ceiling authorization model: every gate must be
//! `allow = is_admin OR (has_capability(key) AND object_access >= required)`, with object access
//! only narrowing within the capability ceiling.
//!
//! Two guards: (1) a grep-gate asserting no function in `apps/backend/src/services/*.rs` uses a
//! banned half-gate instead of `authz::authorize::authorize`; (2) the invariant `allow ⟹
//! has_capability(key)` for every non-admin path, exercised as capability × reach tables plus
//! dynamic add/remove demonstrations.

mod common;
use common::{make_org, user_with_role};

use arcodash_backend::authz::authorize::{
    authorize, Action, DashboardVerb, DataSourceVerb, MonitorVerb, QueryVerb,
};
use arcodash_backend::services::permission_service;
use arcodash_core::entities::{
    dashboard, data_source, data_source_group, group, monitor, query, user,
};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
use std::fs;
use std::path::Path;

/// Half-gate call patterns that must never appear in `services/*.rs`. `require_edit_role` is
/// excluded: it's a documented pass-through checked separately.
const BANNED_CALL_PATTERNS: &[&str] = &[
    "object_role_for(",
    "require_owner_or_admin(",
    "require_owner_or_edit_monitor(",
];

/// Capability keys that gate a mutation on a specific object; a direct `user_has_permission(...,
/// "edit_query")` is the capability-only half-gate `authorize` exists to prevent. View-tier and
/// create-tier keys are excluded: they're legitimate list-wide/non-object uses.
const OBJECT_MUTATION_KEYS: &[&str] = &[
    "edit_query",
    "edit_dashboard",
    "edit_monitor",
    "edit_data_source",
];

/// Line-by-line scan for a reintroduced half-gate, skipping comment lines; also run on fixture
/// strings to prove the detection fires.
fn scan_for_violations(file_name: &str, contents: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (i, line) in contents.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for pat in BANNED_CALL_PATTERNS {
            if line.contains(pat) {
                violations.push(format!(
                    "{file_name}:{}: reintroduced half-gate call `{pat}` — route through authz::authorize::authorize instead",
                    i + 1
                ));
            }
        }
        if line.contains("user_has_permission(") {
            for key in OBJECT_MUTATION_KEYS {
                if line.contains(&format!("\"{key}\"")) {
                    violations.push(format!(
                        "{file_name}:{}: bare user_has_permission(...) check on object-mutation key \"{key}\" — must route through authorize()",
                        i + 1
                    ));
                }
            }
        }
    }
    violations
}

/// The actual gate: scans every file under `src/services/` for banned half-gates and confirms the
/// deleted helper names (`require_owner_or_admin`, `require_owner_or_edit_monitor`) don't exist.
#[test]
fn no_half_gates_in_services() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/services");
    let mut violations = Vec::new();
    let mut scanned = 0;
    for entry in fs::read_dir(&dir).expect("services dir must exist") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let contents = fs::read_to_string(&path).expect("read service file");
        assert!(
            !contents.contains("fn require_owner_or_admin"),
            "{}: require_owner_or_admin must stay deleted",
            path.display()
        );
        assert!(
            !contents.contains("fn require_owner_or_edit_monitor"),
            "{}: require_owner_or_edit_monitor must stay deleted",
            path.display()
        );
        scanned += 1;
        violations.extend(scan_for_violations(&path.display().to_string(), &contents));
    }
    assert!(
        scanned > 5,
        "sanity: expected to scan several service files, only found {scanned} — did the path change?"
    );
    assert!(
        violations.is_empty(),
        "half-gate regression(s) found in services/ — route through authz::authorize instead:\n{}",
        violations.join("\n")
    );
}

/// Sanity check on the detection logic: each banned pattern fires on a fixture, a comment mention
/// and legitimate view-/create-tier checks do not.
#[test]
fn grep_gate_detects_known_bad_patterns() {
    let bad_object_role_for =
        "async fn sneaky(db: &impl ConnectionTrait) { let role = object_role_for(db, user, \"query\", id).await?; }";
    assert!(
        !scan_for_violations("fixture.rs", bad_object_role_for).is_empty(),
        "must flag a reintroduced object_role_for call"
    );

    let bad_require_owner_or_admin = "require_owner_or_admin(db, user, &d).await?;";
    assert!(
        !scan_for_violations("fixture.rs", bad_require_owner_or_admin).is_empty(),
        "must flag a reintroduced require_owner_or_admin call"
    );

    let bad_require_owner_or_edit_monitor = "require_owner_or_edit_monitor(db, user, &m).await?;";
    assert!(
        !scan_for_violations("fixture.rs", bad_require_owner_or_edit_monitor).is_empty(),
        "must flag a reintroduced require_owner_or_edit_monitor call"
    );

    let bad_bare_permission =
        "if !user_has_permission(user, &groups, \"edit_query\") { return Err(x); }";
    assert!(
        !scan_for_violations("fixture.rs", bad_bare_permission).is_empty(),
        "must flag a bare capability-only check on edit_query"
    );

    // A comment merely mentioning the name must not be flagged.
    let comment_only = "// this calls object_role_for( internally, see authorize.rs";
    assert!(
        scan_for_violations("fixture.rs", comment_only).is_empty(),
        "a comment mentioning the name must not be flagged"
    );

    // view_/create_ capability checks are legitimate and must not be flagged.
    let legit =
        "let can_view_type = admin || user_has_permission(user, &groups, \"view_dashboard\");";
    assert!(
        scan_for_violations("fixture.rs", legit).is_empty(),
        "view_/create_ capability checks are legitimate and must not be flagged"
    );
}

/// `require_edit_role` is the one half-gate name allowed to survive, as a thin wrapper over
/// `authorize::authorize`; pins that its body is a pure delegate.
#[test]
fn require_edit_role_is_a_pure_authorize_delegate() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/services/query_service.rs");
    let contents = fs::read_to_string(&path).expect("read query_service.rs");
    let fn_start = contents
        .find("fn require_edit_role")
        .expect("require_edit_role must still exist as a function in query_service.rs");
    let body_start = contents[fn_start..]
        .find('{')
        .map(|i| fn_start + i)
        .expect("require_edit_role must have a body");

    let mut depth = 0i32;
    let mut body_end = body_start;
    for (i, ch) in contents[body_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    body_end = body_start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    let body = &contents[body_start..=body_end];

    assert!(
        body.contains("authorize::authorize("),
        "require_edit_role must delegate fully to authz::authorize::authorize: {body}"
    );
    assert!(
        !body.contains("object_role_for("),
        "require_edit_role must not call object_role_for directly — that would reintroduce a \
         reach-only half-gate: {body}"
    );
}

async fn make_query_owned_by(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    owner_id: i64,
) -> query::Model {
    let ds_id = data_source::Entity::insert(arcodash_core::test_support::new_data_source(org_id))
        .exec(db)
        .await
        .expect("data source insert failed")
        .last_insert_id;
    let mut active = arcodash_core::test_support::new_query(org_id, ds_id, owner_id);
    active.api_key = Set(uuid::Uuid::new_v4().simple().to_string());
    active.visibility = Set("restricted".to_string());
    active.insert(db).await.expect("query insert failed")
}

async fn make_dashboard_owned_by(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    owner_id: i64,
) -> dashboard::Model {
    let mut active = arcodash_core::test_support::new_dashboard(org_id, owner_id);
    active.slug = Set(format!(
        "removal-invariant-{}",
        uuid::Uuid::new_v4().simple()
    ));
    active.visibility = Set("restricted".to_string());
    active.insert(db).await.expect("dashboard insert failed")
}

async fn make_monitor_owned_by(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    owner_id: i64,
    query_id: i64,
) -> monitor::Model {
    monitor::ActiveModel {
        org_id: Set(org_id),
        user_id: Set(owner_id),
        query_id: Set(query_id),
        name: Set(format!("m-{}", uuid::Uuid::new_v4().simple())),
        options: Set(serde_json::json!({ "op": ">", "value": 1 })),
        state: Set("unknown".to_string()),
        rearm: Set(None),
        last_triggered_at: Set(None),
        visibility: Set("restricted".to_string()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("monitor insert failed")
}

/// Grant `access_type` on `(object_type, object_id)` to `grantee`, sharing as `owner`; returns the
/// grant id.
async fn grant(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    owner: &user::Model,
    object_type: &str,
    object_id: i64,
    grantee: &user::Model,
    access_type: &str,
) -> i64 {
    permission_service::grant(
        db,
        org_id,
        owner,
        object_type,
        object_id,
        Some(grantee.id),
        None,
        access_type,
    )
    .await
    .expect("grant failed")
    .id
}

/// Capability × reach matrix for queries: a capability-less user is denied edit at every reach
/// level (including ownership); a holder is denied until reach clears Editor, then allowed at
/// Editor and Owner.
#[tokio::test]
async fn removal_invariant_matrix_query() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    let (no_cap, _) = user_with_role(&app.db, org_id, &[]).await;
    let (has_cap, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;

    let obj_none = make_query_owned_by(&app.db, org_id, owner.id).await;
    let obj_viewer = make_query_owned_by(&app.db, org_id, owner.id).await;
    grant(
        &app.db,
        org_id,
        &owner,
        "query",
        obj_viewer.id,
        &no_cap,
        "viewer",
    )
    .await;
    grant(
        &app.db,
        org_id,
        &owner,
        "query",
        obj_viewer.id,
        &has_cap,
        "viewer",
    )
    .await;
    let obj_editor = make_query_owned_by(&app.db, org_id, owner.id).await;
    grant(
        &app.db,
        org_id,
        &owner,
        "query",
        obj_editor.id,
        &no_cap,
        "editor",
    )
    .await;
    grant(
        &app.db,
        org_id,
        &owner,
        "query",
        obj_editor.id,
        &has_cap,
        "editor",
    )
    .await;
    let obj_owned_no_cap = make_query_owned_by(&app.db, org_id, no_cap.id).await;
    let obj_owned_has_cap = make_query_owned_by(&app.db, org_id, has_cap.id).await;

    let action = |id: i64| Action::Query {
        verb: QueryVerb::Edit,
        id,
    };

    for (level, obj_id) in [
        ("no reach", obj_none.id),
        ("viewer reach", obj_viewer.id),
        ("editor reach", obj_editor.id),
        ("owner reach", obj_owned_no_cap.id),
    ] {
        let r = authorize(&app.db, &no_cap, action(obj_id)).await;
        assert!(r.is_err(), "query/no-capability/{level}: must be denied (removal invariant: object-access alone never grants beyond the capability ceiling)");
    }

    for (level, obj_id, expect_allow) in [
        ("no reach", obj_none.id, false),
        ("viewer reach", obj_viewer.id, false),
        ("editor reach", obj_editor.id, true),
        ("owner reach", obj_owned_has_cap.id, true),
    ] {
        let r = authorize(&app.db, &has_cap, action(obj_id)).await;
        assert_eq!(
            r.is_ok(),
            expect_allow,
            "query/has-capability/{level}: expected allow={expect_allow}, got {:?}",
            r.err()
        );
    }
}

/// REQUIRED capability × reach matrix for dashboards — same shape as the query matrix above.
#[tokio::test]
async fn removal_invariant_matrix_dashboard() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_dashboard"]).await;
    let (no_cap, _) = user_with_role(&app.db, org_id, &[]).await;
    let (has_cap, _) = user_with_role(&app.db, org_id, &["edit_dashboard"]).await;

    let obj_none = make_dashboard_owned_by(&app.db, org_id, owner.id).await;
    let obj_viewer = make_dashboard_owned_by(&app.db, org_id, owner.id).await;
    grant(
        &app.db,
        org_id,
        &owner,
        "dashboard",
        obj_viewer.id,
        &no_cap,
        "viewer",
    )
    .await;
    grant(
        &app.db,
        org_id,
        &owner,
        "dashboard",
        obj_viewer.id,
        &has_cap,
        "viewer",
    )
    .await;
    let obj_editor = make_dashboard_owned_by(&app.db, org_id, owner.id).await;
    grant(
        &app.db,
        org_id,
        &owner,
        "dashboard",
        obj_editor.id,
        &no_cap,
        "editor",
    )
    .await;
    grant(
        &app.db,
        org_id,
        &owner,
        "dashboard",
        obj_editor.id,
        &has_cap,
        "editor",
    )
    .await;
    let obj_owned_no_cap = make_dashboard_owned_by(&app.db, org_id, no_cap.id).await;
    let obj_owned_has_cap = make_dashboard_owned_by(&app.db, org_id, has_cap.id).await;

    let action = |id: i64| Action::Dashboard {
        verb: DashboardVerb::Edit,
        id,
    };

    for (level, obj_id) in [
        ("no reach", obj_none.id),
        ("viewer reach", obj_viewer.id),
        ("editor reach", obj_editor.id),
        ("owner reach", obj_owned_no_cap.id),
    ] {
        let r = authorize(&app.db, &no_cap, action(obj_id)).await;
        assert!(
            r.is_err(),
            "dashboard/no-capability/{level}: must be denied"
        );
    }

    for (level, obj_id, expect_allow) in [
        ("no reach", obj_none.id, false),
        ("viewer reach", obj_viewer.id, false),
        ("editor reach", obj_editor.id, true),
        ("owner reach", obj_owned_has_cap.id, true),
    ] {
        let r = authorize(&app.db, &has_cap, action(obj_id)).await;
        assert_eq!(
            r.is_ok(),
            expect_allow,
            "dashboard/has-capability/{level}: expected allow={expect_allow}, got {:?}",
            r.err()
        );
    }
}

/// Capability × reach matrix for monitors — same shape, plus an explicit AND-not-OR check:
/// ownership alone must never bypass capability, and capability alone must never bypass reach.
#[tokio::test]
async fn removal_invariant_matrix_monitor() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_monitor"]).await;
    let (no_cap, _) = user_with_role(&app.db, org_id, &[]).await;
    let (has_cap, _) = user_with_role(&app.db, org_id, &["edit_monitor"]).await;

    let backing_query = make_query_owned_by(&app.db, org_id, owner.id).await;

    let obj_none = make_monitor_owned_by(&app.db, org_id, owner.id, backing_query.id).await;
    let obj_viewer = make_monitor_owned_by(&app.db, org_id, owner.id, backing_query.id).await;
    grant(
        &app.db,
        org_id,
        &owner,
        "monitor",
        obj_viewer.id,
        &no_cap,
        "viewer",
    )
    .await;
    grant(
        &app.db,
        org_id,
        &owner,
        "monitor",
        obj_viewer.id,
        &has_cap,
        "viewer",
    )
    .await;
    let obj_editor = make_monitor_owned_by(&app.db, org_id, owner.id, backing_query.id).await;
    grant(
        &app.db,
        org_id,
        &owner,
        "monitor",
        obj_editor.id,
        &no_cap,
        "editor",
    )
    .await;
    grant(
        &app.db,
        org_id,
        &owner,
        "monitor",
        obj_editor.id,
        &has_cap,
        "editor",
    )
    .await;
    let obj_owned_no_cap =
        make_monitor_owned_by(&app.db, org_id, no_cap.id, backing_query.id).await;
    let obj_owned_has_cap =
        make_monitor_owned_by(&app.db, org_id, has_cap.id, backing_query.id).await;

    let action = |id: i64| Action::Monitor {
        verb: MonitorVerb::Edit,
        id,
    };

    for (level, obj_id) in [
        ("no reach", obj_none.id),
        ("viewer reach", obj_viewer.id),
        ("editor reach", obj_editor.id),
        ("owner reach", obj_owned_no_cap.id),
    ] {
        let r = authorize(&app.db, &no_cap, action(obj_id)).await;
        assert!(
            r.is_err(),
            "monitor/no-capability/{level}: must be denied — ownership alone must not bypass capability (AND not OR)"
        );
    }

    for (level, obj_id, expect_allow) in [
        ("no reach", obj_none.id, false),
        ("viewer reach", obj_viewer.id, false),
        ("editor reach", obj_editor.id, true),
        ("owner reach", obj_owned_has_cap.id, true),
    ] {
        let r = authorize(&app.db, &has_cap, action(obj_id)).await;
        assert_eq!(
            r.is_ok(),
            expect_allow,
            "monitor/has-capability/{level}: expected allow={expect_allow}, got {:?}",
            r.err()
        );
    }
}

/// Capability × reach matrix for data sources. Reach is binary under the restricted toggle (no
/// owner concept): out of scope vs in scope.
#[tokio::test]
async fn removal_invariant_matrix_data_source() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let ds_out_of_scope = arcodash_core::test_support::new_data_source(org_id)
        .insert(&app.db)
        .await
        .expect("data source insert failed");
    let ds_in_scope = arcodash_core::test_support::new_data_source(org_id)
        .insert(&app.db)
        .await
        .expect("data source insert failed");

    let (no_cap, no_cap_role_group) = user_with_role(&app.db, org_id, &[]).await;
    let (has_cap, has_cap_role_group) =
        user_with_role(&app.db, org_id, &["edit_data_source"]).await;
    let _ = (no_cap_role_group, has_cap_role_group);

    async fn assign(db: &sea_orm::DatabaseConnection, ds_id: i64, group_id: i64) {
        data_source_group::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            data_source_id: Set(ds_id),
            group_id: Set(group_id),
            created_at: sea_orm::ActiveValue::NotSet,
            updated_at: sea_orm::ActiveValue::NotSet,
        }
        .insert(db)
        .await
        .expect("data_source_group insert failed");
    }

    async fn restrict(db: &sea_orm::DatabaseConnection, group_id: i64) {
        let g = group::Entity::find_by_id(group_id)
            .one(db)
            .await
            .unwrap()
            .expect("group not found");
        let mut active: group::ActiveModel = g.into();
        active.data_source_access_restricted = Set(true);
        active.update(db).await.expect("group restrict failed");
    }

    // Groups default unrestricted: restrict them, then allowlist only `ds_in_scope`.
    restrict(&app.db, no_cap.groups[0]).await;
    restrict(&app.db, has_cap.groups[0]).await;
    assign(&app.db, ds_in_scope.id, no_cap.groups[0]).await;
    assign(&app.db, ds_in_scope.id, has_cap.groups[0]).await;

    let action = |id: i64| Action::DataSource {
        verb: DataSourceVerb::Edit,
        id,
    };

    for (level, ds_id) in [
        ("out of scope", ds_out_of_scope.id),
        ("in scope", ds_in_scope.id),
    ] {
        let r = authorize(&app.db, &no_cap, action(ds_id)).await;
        assert!(
            r.is_err(),
            "data_source/no-capability/{level}: must be denied"
        );
    }

    for (level, ds_id, expect_allow) in [
        ("out of scope", ds_out_of_scope.id, false),
        ("in scope", ds_in_scope.id, true),
    ] {
        let r = authorize(&app.db, &has_cap, action(ds_id)).await;
        assert_eq!(
            r.is_ok(),
            expect_allow,
            "data_source/has-capability/{level}: expected allow={expect_allow}, got {:?}",
            r.err()
        );
    }
}

/// Dynamic half of the removal invariant for `access_permission` rows: granting Editor allows edit;
/// revoking that grant narrows access back to what capability alone denies.
#[tokio::test]
async fn revoking_access_permission_narrows_back_to_capability_alone() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let (owner, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    let (has_cap, _) = user_with_role(&app.db, org_id, &["edit_query"]).await;
    let q = make_query_owned_by(&app.db, org_id, owner.id).await;

    let action = Action::Query {
        verb: QueryVerb::Edit,
        id: q.id,
    };
    assert!(
        authorize(&app.db, &has_cap, action).await.is_err(),
        "no reach yet: capability alone must not be sufficient"
    );

    let grant_id = grant(&app.db, org_id, &owner, "query", q.id, &has_cap, "editor").await;
    assert!(
        authorize(&app.db, &has_cap, action).await.is_ok(),
        "adding an Editor grant within the capability ceiling must allow"
    );

    permission_service::revoke(&app.db, org_id, &owner, "query", q.id, grant_id)
        .await
        .expect("revoke failed");
    assert!(
        authorize(&app.db, &has_cap, action).await.is_err(),
        "removing the grant must narrow back to exactly the capability-alone denial, never below it"
    );
}

/// Dynamic half of the removal invariant for `data_source_groups` rows, the second grant mechanism.
#[tokio::test]
async fn revoking_data_source_group_narrows_back_to_capability_alone() {
    let app = skip_if_none!(common::spawn_app().await);
    let org_id = make_org(&app.db).await;

    let ds = arcodash_core::test_support::new_data_source(org_id)
        .insert(&app.db)
        .await
        .expect("data source insert failed");
    let (has_cap, _) = user_with_role(&app.db, org_id, &["edit_data_source"]).await;
    let group_id = has_cap.groups[0];

    // Groups default unrestricted: restrict this one first so the allowlist row's effect is
    // isolated.
    let g = group::Entity::find_by_id(group_id)
        .one(&app.db)
        .await
        .unwrap()
        .expect("group not found");
    let mut active: group::ActiveModel = g.into();
    active.data_source_access_restricted = Set(true);
    active.update(&app.db).await.expect("group restrict failed");

    let action = Action::DataSource {
        verb: DataSourceVerb::Edit,
        id: ds.id,
    };
    assert!(
        authorize(&app.db, &has_cap, action).await.is_err(),
        "no allowlist row yet: capability alone must not be sufficient"
    );

    let dsg = data_source_group::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        data_source_id: Set(ds.id),
        group_id: Set(group_id),
        created_at: sea_orm::ActiveValue::NotSet,
        updated_at: sea_orm::ActiveValue::NotSet,
    }
    .insert(&app.db)
    .await
    .expect("data_source_group insert failed");
    assert!(
        authorize(&app.db, &has_cap, action).await.is_ok(),
        "adding a data_source_groups allowlist row within the capability ceiling must allow"
    );

    data_source_group::Entity::delete_by_id(dsg.id)
        .exec(&app.db)
        .await
        .expect("delete failed");
    assert!(
        authorize(&app.db, &has_cap, action).await.is_err(),
        "removing the data_source_groups row must narrow back to the capability-alone denial"
    );
}
