use std::sync::Arc;
use std::time::Duration;

use axum::extract::FromRef;
use redis::aio::ConnectionManager;
use sea_orm::DatabaseConnection;

use arcodash_core::connectors::registry::ConnectorRegistry;
use arcodash_core::entitlements::OrgEntitlements;
use arcodash_core::queue::Queue;
use arcodash_core::storage::ObjectStore;
use arcodash_core::Config;

use crate::auth::OrgResolver;
use crate::authz::org_access::OrgAccessCheck;
use crate::db::RequestTxnHook;
use crate::services::mailer::Mailer;

/// Who may view a dashboard via its public share token (`dashboards::public_get`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicDashboardAccess {
    /// Anyone with the token, no auth required (single-org self-host default; still gated by
    /// `DISABLE_PUBLIC_URLS`).
    Anonymous,
    /// A signed-in member of the dashboard's own org (multi-tenant). Unauthenticated requests get
    /// 401 unless the link is `anon_authorized`; a member of a different org also gets 401.
    OrgMemberOnly,
}

/// Shared application state handed to every Axum handler.
#[derive(Clone)]
pub struct AppState {
    pub db: DatabaseConnection,
    /// The connection conveyor's own tables (`conveyor_tasks`/`conveyor_schedules`) use: a
    /// separate, more-privileged handle when `db` is privilege-restricted (those tables aren't RLS
    /// org-scoped). Identical to `db` in the OSS binary. Also used for the one lookup with no org
    /// yet (`embed_service::org_for_public_token`).
    pub conveyor_db: DatabaseConnection,
    pub redis: ConnectionManager,
    pub config: Config,
    /// `type` → `QueryRunner` lookup (each connector registers here); `Arc`-wrapped to keep
    /// `AppState` cheap to clone.
    pub connectors: Arc<ConnectorRegistry>,
    /// Backs the upload data-source endpoint directly; `connectors` holds its own clone for the
    /// csv/sqlite connectors.
    pub store: Arc<ObjectStore>,
    /// Job producer, used by `services::query_service::execute` to enqueue `ExecuteQuery` jobs.
    pub queue: Arc<dyn Queue>,
    /// Producer-only conveyor handle for `execute_query` (the worker owns handlers); `None` when
    /// `CONVEYOR_EXECUTE_QUERY_ENABLED` is unset, so execution takes the `queue` path.
    pub conveyor: Option<Arc<arcodash_core::Conveyor>>,
    /// Optional SMTP mailer for backend-originated emails (invitations, password resets); `None`
    /// when `SMTP_HOST` is unset.
    pub mailer: Option<Arc<Mailer>>,
    /// Shared HTTP client for outbound OAuth / third-party API calls.
    pub http: reqwest::Client,
    /// Org-resolution hook (`auth::OrgResolver`); `None` in the OSS binary, which falls back to the
    /// seeded `default` org.
    pub org_resolver: Option<Arc<dyn OrgResolver>>,
    /// Per-request transaction hook (`crate::db::RequestTxnHook`); `None` in the OSS binary, where
    /// requests use the plain pooled connection.
    pub request_txn_hook: Option<Arc<dyn RequestTxnHook>>,
    /// Access policy for `dashboards::public_get`'s share-token route: `Anonymous` by default,
    /// `OrgMemberOnly` for multi-tenant deployments.
    pub public_dashboard_access: PublicDashboardAccess,
    /// Default TTL applied by `embed_service::create_embed` when a caller doesn't specify
    /// `expires_at`; `None` means links never auto-expire.
    pub embed_default_expiry: Option<Duration>,
    /// Optional per-request org access gate (`authz::org_access::OrgAccessCheck`), called in
    /// `middleware::org::inject_current_org` after the org resolves; an `Err` short-circuits the
    /// request.
    pub org_access_check: Option<Arc<dyn OrgAccessCheck>>,
    /// What each org may do (`arcodash_core::entitlements`); `Unrestricted` unless
    /// `AppExtensions::entitlements` supplies one.
    pub entitlements: Arc<dyn OrgEntitlements>,
    /// Org-settings keys clients may never write through the generic organization endpoint, in
    /// addition to the built-in ones. Filled from `AppExtensions::reserved_org_settings_keys`.
    pub reserved_org_settings_keys: Arc<Vec<&'static str>>,
    /// The embedding deployment's login system, if it replaces the built-in one. Filled from
    /// `AppExtensions::external_auth`.
    pub external_auth: Option<Arc<crate::ExternalAuth>>,
}

impl FromRef<AppState> for Config {
    fn from_ref(state: &AppState) -> Self {
        state.config.clone()
    }
}

impl AppState {
    /// `provider` claim this deployment mints and accepts on session cookies.
    #[must_use]
    pub fn session_provider(&self) -> &'static str {
        self.external_auth
            .as_ref()
            .map_or(crate::auth::session::INTERNAL_PROVIDER, |e| {
                e.session_provider
            })
    }

    pub async fn publish_wake(&self) {
        if let Err(e) = arcodash_core::redis_util::publish_sched_wake(&self.redis).await {
            tracing::warn!(error = %e, "scheduler wake publish failed; the loop falls back to its poll cap");
        }
    }
}
