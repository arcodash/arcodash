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
            header::HeaderName::from_static("x-reauth-token"),
        ])
        // Cross-origin responses hide every header from JS by default unless it's explicitly
        // exposed — without this, `fetch`'s `Headers.get("x-total-count")` always reads null in
        // the browser (reqwest-based tests never notice, since that same-origin restriction is a
        // browser-only behavior).
        .expose_headers([header::HeaderName::from_static("x-total-count")])
}

#[cfg(test)]
mod tests {
    use super::cors;
    use axum::{body::Body, http::Request, routing::get, Router};
    use tower::ServiceExt;

    #[tokio::test]
    async fn preflight_allows_reauth_token() {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(cors("http://localhost:3000".parse().unwrap()));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/")
                    .header("origin", "http://localhost:3000")
                    .header("access-control-request-method", "POST")
                    .header("access-control-request-headers", "x-reauth-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let allowed = resp.headers()["access-control-allow-headers"]
            .to_str()
            .unwrap();
        assert!(allowed.contains("x-reauth-token"), "got: {allowed}");
    }
}
