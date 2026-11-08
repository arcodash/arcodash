//! Library target: exposes `build_app` and `AppState` so integration tests can run the server
//! in-process; all logic lives in submodules.

#![allow(clippy::result_large_err)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::doc_lazy_continuation)]

pub mod api;
pub mod auth;
pub mod authz;
pub mod bootstrap;
pub mod db;
pub mod error;
pub mod extract;
pub mod metrics;
pub mod middleware;
pub mod services;
pub mod state;

use std::sync::Arc;

use axum::extract::{MatchedPath, Request};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info_span;

pub use state::{AppState, PublicDashboardAccess};

/// A login system owned by the embedding deployment that replaces the built-in one. When set,
/// `/api/login`, SAML, LDAP, password reset/change and invitation verify/accept are not mounted.
#[derive(Clone)]
pub struct ExternalAuth {
    /// `provider` claim its sessions are minted and verified under; never `session::INTERNAL_PROVIDER`.
    pub session_provider: &'static str,
    /// Merged by key into `/api/session`'s `client_config`; keys must not collide with built-in ones.
    pub client_config: serde_json::Map<String, serde_json::Value>,
}

/// Extension point for an embedding deployment: extra `/api` routes (e.g. OAuth) and the
/// CSRF/org-exempt paths they need. Empty for the stock binary.
pub struct AppExtensions {
    /// Extra routes merged into `/api`, with the same middleware stack as built-in routes.
    pub api_router: Router<AppState>,
    /// Paths that bypass CSRF in addition to `CSRF_EXEMPT_PATHS` (e.g. an OAuth callback).
    pub csrf_exempt_paths: Vec<&'static str>,
    /// Paths that skip `inject_current_org` entirely, for inherently org-agnostic routes (org
    /// resolution would otherwise 404 them).
    pub org_exempt_paths: Vec<&'static str>,
    /// Paths where a resolution failure degrades to "no org" instead of erroring, while a
    /// successful resolution still opens the RLS transaction. `GET /api/session` bootstraps the
    /// login UI, so it must answer 200 for a logged-out visitor on an unknown host; defaults to
    /// `/api/session`.
    pub org_optional_paths: Vec<&'static str>,
    pub entitlements: Option<Arc<dyn arcodash_core::entitlements::OrgEntitlements>>,
    /// Org-settings keys the organization endpoint must never let a client write, beyond the
    /// built-in ones.
    pub reserved_org_settings_keys: Vec<&'static str>,
    /// Replaces the built-in login system; `None` keeps it.
    pub external_auth: Option<ExternalAuth>,
}

/// `GET /api/session` is the one org-optional path every binary needs; shared so embedders don't
/// repeat the string.
pub const DEFAULT_ORG_OPTIONAL_PATHS: &[&str] = &["/api/session"];

impl Default for AppExtensions {
    fn default() -> Self {
        Self {
            api_router: Router::new(),
            csrf_exempt_paths: Vec::new(),
            org_exempt_paths: Vec::new(),
            org_optional_paths: DEFAULT_ORG_OPTIONAL_PATHS.to_vec(),
            entitlements: None,
            reserved_org_settings_keys: Vec::new(),
            external_auth: None,
        }
    }
}

/// Build the Axum [`Router`] with all middleware applied; extracted so tests can bind to port 0.
pub fn build_app(mut state: AppState, cors: CorsLayer, ext: AppExtensions) -> Router {
    if let Some(entitlements) = ext.entitlements {
        state.entitlements = entitlements;
    }
    state.reserved_org_settings_keys = Arc::new(ext.reserved_org_settings_keys);
    if let Some(ext_auth) = &ext.external_auth {
        assert_ne!(
            ext_auth.session_provider,
            auth::session::INTERNAL_PROVIDER,
            "ExternalAuth.session_provider must differ from the built-in provider"
        );
    }
    state.external_auth = ext.external_auth.map(Arc::new);
    let csrf_exempt_paths = Arc::new(ext.csrf_exempt_paths);
    let org_exempt_paths = Arc::new(ext.org_exempt_paths);
    let org_optional_paths = Arc::new(ext.org_optional_paths);
    Router::new()
        .route("/health", get(health))
        .route("/ping", get(ping))
        .route("/metrics", get(metrics::metrics))
        .nest("/api", api::router::router(&state).merge(ext.api_router))
        // Tower applies later `.layer()` calls as outer wrappers, so org resolution runs BEFORE
        // CSRF validation. Deliberate: reordering changes the failure mode for
        // org-exempt/org-optional paths, and org-first is what enables org-aware CSRF rejection
        // logs and the RLS transaction hook.
        .layer(axum::middleware::from_fn_with_state(
            csrf_exempt_paths,
            middleware::csrf::validate_csrf,
        ))
        .layer(axum::middleware::from_fn_with_state(
            middleware::org::OrgMiddlewareState {
                app_state: state.clone(),
                org_exempt_paths,
                org_optional_paths,
            },
            middleware::org::inject_current_org,
        ))
        // Built with `.make_span_with` (not `new_for_http()`, which logs the full URI) so query
        // strings such as `?api_key=...` never reach logs; only the path template is recorded.
        .layer(TraceLayer::new_for_http().make_span_with(|req: &Request| {
            let path = req
                .extensions()
                .get::<MatchedPath>()
                .map_or_else(|| req.uri().path(), MatchedPath::as_str);
            info_span!("request", method = %req.method(), path)
        }))
        // Wraps org/CSRF (so their rejections are counted); labeled by route template, never the
        // raw path.
        .layer(axum::middleware::from_fn(record_http_metrics))
        .layer(CompressionLayer::new())
        .layer(cors)
        // Outermost application layer, so a panic anywhere below (including CSRF/org middleware)
        // still gets a response.
        .layer(CatchPanicLayer::custom(handle_panic))
        .with_state(state)
}

/// Counts every HTTP response as `arcodash_http_requests_total{method,route,status}`; `route` is
/// the `MatchedPath` template, keeping cardinality bounded.
async fn record_http_metrics(req: Request, next: axum::middleware::Next) -> Response {
    let method = match req.method().as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        _ => "OTHER",
    };
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", MatchedPath::as_str)
        .to_string();

    let response = next.run(req).await;
    metrics::record_http_request(method, route, metrics::status_class(response.status()));
    response
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

async fn ping() -> impl IntoResponse {
    "pong"
}

/// `CatchPanicLayer`'s handler: logs the panic payload and returns a plain 500.
#[allow(
    clippy::needless_pass_by_value,
    reason = "signature fixed by tower-http's CatchPanicLayer::custom, which hands over the payload by value"
)]
fn handle_panic(err: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let payload = err
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| err.downcast_ref::<&str>().copied())
        .unwrap_or("unknown panic payload");
    tracing::error!(panic = %payload, "request handler panicked");
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "message": "internal server error" })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use tower::ServiceExt;

    async fn always_panics() -> &'static str {
        panic!("boom")
    }

    /// A panicking handler must answer with a generic 500, and the service must remain usable
    /// afterward.
    #[tokio::test]
    async fn catch_panic_layer_survives_a_handler_panic_and_stays_usable() {
        let app: Router = Router::new()
            .route("/panic", get(always_panics))
            .route("/ok", get(|| async { "fine" }))
            .layer(CatchPanicLayer::custom(handle_panic));

        let panic_resp = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/panic")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(panic_resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // A follow-up request must still succeed; the panic must not poison the service.
        let ok_resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/ok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok_resp.status(), StatusCode::OK);
    }

    /// A slow in-flight request completes before `serve` returns when graceful shutdown fires.
    /// Drives the shutdown future directly rather than a real SIGTERM.
    #[tokio::test]
    async fn graceful_shutdown_lets_an_in_flight_request_finish() {
        let app: Router = Router::new().route(
            "/slow",
            get(|| async {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                "done"
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let serve_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = reqwest::Client::new();
        let request = client.get(format!("http://{addr}/slow")).send();
        let slow_request = tokio::spawn(request);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = shutdown_tx.send(());

        tokio::time::timeout(std::time::Duration::from_secs(2), serve_task)
            .await
            .expect("serve must return promptly once the in-flight request drains")
            .unwrap();

        let response = slow_request
            .await
            .unwrap()
            .expect("the in-flight request must complete, not be cut by shutdown");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "done");
    }
}
