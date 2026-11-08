//! A failed invite/password-reset email delivery must never be silent: the request still
//! succeeds (the row is already persisted), but a real SMTP failure is logged and recorded as
//! an `events` row so an operator can find it. Distinguishes three states: SMTP unconfigured
//! (informational only, no failure event), SMTP configured but unreachable (failure event), and
//! (in `integration_users.rs`, against real Mailpit) SMTP that actually works.

mod common;
use common::default_org_id;
use common::{spawn_app, spawn_app_with_real_mailer, spawn_app_with_unreachable_mailer};

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde_json::Value;

async fn latest_event_for_action(
    db: &sea_orm::DatabaseConnection,
    org_id: i64,
    action: &str,
) -> Option<arcodash_core::entities::event::Model> {
    use arcodash_core::entities::event;
    event::Entity::find()
        .filter(event::Column::OrgId.eq(org_id))
        .filter(event::Column::Action.eq(action))
        .order_by_desc(event::Column::Id)
        .one(db)
        .await
        .unwrap()
}

/// `POST /api/users` against an SMTP host nothing is listening on must still return 200 (the
/// invite row is real; a mail outage is not the caller's problem) and record a
/// `user.invite_email_failed` event an operator can find.
#[tokio::test]
async fn invite_with_unreachable_smtp_still_succeeds_and_records_a_failure_event() {
    let app = skip_if_none!(spawn_app_with_unreachable_mailer().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let email = format!(
        "unreachable-smtp-{}@test.local",
        uuid::Uuid::new_v4().simple()
    );
    let resp = admin
        .post(format!("{}/api/users", app.address))
        .json(&serde_json::json!({ "name": "Doomed Invite", "email": email, "groups": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "invite must succeed even though the mail transport is unreachable"
    );

    let event = latest_event_for_action(&app.db, org_id, "user.invite_email_failed")
        .await
        .expect("an unreachable-SMTP invite must record a user.invite_email_failed event");
    assert_eq!(event.additional_properties["email"], Value::String(email));
}

/// `POST /api/forgot_password` against an unreachable SMTP host must still return 204 (the
/// no-enumeration contract is unconditional) and record a `user.reset_email_failed` event.
#[tokio::test]
async fn forgot_password_with_unreachable_smtp_still_returns_204_and_records_a_failure_event() {
    let app = skip_if_none!(spawn_app_with_unreachable_mailer().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let resp = admin
        .post(format!("{}/api/forgot_password", app.address))
        .json(&serde_json::json!({ "email": common::ADMIN_EMAIL }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "forgot_password must always return 204"
    );

    let event = latest_event_for_action(&app.db, org_id, "user.reset_email_failed")
        .await
        .expect("an unreachable-SMTP reset must record a user.reset_email_failed event");
    assert_eq!(
        event.additional_properties["email"],
        Value::String(common::ADMIN_EMAIL.to_string())
    );
}

/// SMTP unconfigured (the default test harness's `spawn_app()`, `mailer: None`) is not a
/// delivery failure — it's an intentional no-mail-transport mode with its own info-log path.
/// Neither invite nor forgot_password should record a failure event for it.
#[tokio::test]
async fn unconfigured_smtp_does_not_record_a_failure_event() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let admin = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;

    let email = format!(
        "unconfigured-smtp-{}@test.local",
        uuid::Uuid::new_v4().simple()
    );
    let resp = admin
        .post(format!("{}/api/users", app.address))
        .json(&serde_json::json!({ "name": "No SMTP Invite", "email": email, "groups": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        latest_event_for_action(&app.db, org_id, "user.invite_email_failed")
            .await
            .is_none(),
        "unconfigured SMTP must never record an invite_email_failed event"
    );

    let resp = admin
        .post(format!("{}/api/forgot_password", app.address))
        .json(&serde_json::json!({ "email": common::ADMIN_EMAIL }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    assert!(
        latest_event_for_action(&app.db, org_id, "user.reset_email_failed")
            .await
            .is_none(),
        "unconfigured SMTP must never record a reset_email_failed event"
    );
}

/// A real (not mocked) SMTP send against Mailpit must never record a failure event: a working
/// transport is the one case the failure-event bookkeeping must stay silent for.
/// `invite_email_lands_in_mailpit_with_html_text_and_org_inviter_named` (`integration_users.rs`)
/// already proves the mail itself arrives; this proves delivery success leaves no
/// `user.invite_email_failed` event behind. Skips cleanly if Mailpit isn't reachable.
#[tokio::test]
async fn real_mailpit_send_records_no_failure_event() {
    let app = skip_if_none!(spawn_app_with_real_mailer().await);
    app.cleanup().await;

    let mailpit_base =
        std::env::var("MAILPIT_API_BASE").unwrap_or_else(|_| "http://localhost:8026".to_string());
    let client = reqwest::Client::new();
    if client
        .get(format!("{mailpit_base}/api/v1/info"))
        .send()
        .await
        .is_err()
    {
        eprintln!("skipping: Mailpit not reachable at {mailpit_base}");
        return;
    }

    let admin = app.admin_client().await;
    let org_id = default_org_id(&app.db).await;
    let email = format!("real-mailpit-{}@test.local", uuid::Uuid::new_v4().simple());

    let resp = admin
        .post(format!("{}/api/users", app.address))
        .json(&serde_json::json!({ "name": "Real Mailpit Invite", "email": email, "groups": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Give the async send a moment to land (or fail) before checking for an event.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        latest_event_for_action(&app.db, org_id, "user.invite_email_failed")
            .await
            .is_none(),
        "a real, working Mailpit send must never record a invite_email_failed event"
    );
}
