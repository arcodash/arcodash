use axum::http::{header, HeaderValue, Method};
use tower_http::cors::CorsLayer;

/// The single CORS policy for every binary that serves this API.
///
/// Covers `apps/backend` and any embedding binary or test harness. `frontend_origin` is the one
/// allowed cross-origin caller (the SPA, on its own origin per each binary's deploy).
pub fn cors(frontend_origin: HeaderValue) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(frontend_origin)
        .allow_credentials(true)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            header::HeaderName::from_static("x-csrf-token"),
        ])
        // Cross-origin responses hide every header from JS by default unless it's explicitly
        // exposed — without this, `fetch`'s `Headers.get("x-total-count")` always reads null in
        // the browser (reqwest-based tests never notice, since that same-origin restriction is a
        // browser-only behavior).
        .expose_headers([header::HeaderName::from_static("x-total-count")])
}
