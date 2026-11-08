//! Proves the OSS CORS layer exposes `x-total-count` to cross-origin JS — the header
//! `AuditLogPage` (`apps/frontend/src/router.tsx`) reads via `apps/frontend/src/lib/api.ts` to
//! compute pagination. Without `.expose_headers(...)` on the `CorsLayer`, a browser's
//! `Headers.get("x-total-count")` always reads `null` on a cross-origin response even though the
//! header is on the wire — `reqwest` doesn't enforce CORS, so this checks the response header
//! Tower actually sets, which is what a real browser reads. Exercises the shared
//! construction (`arcodash_backend::middleware::cors::cors`) that embedding binaries delegate to.

mod common;

use common::spawn_app;

#[tokio::test]
async fn cross_origin_response_exposes_x_total_count() {
    let app = skip_if_none!(spawn_app().await);

    let resp = reqwest::Client::new()
        .get(format!("{}/api/session", app.address))
        .header("Origin", "http://localhost:3000")
        .send()
        .await
        .unwrap();

    let exposed = resp
        .headers()
        .get("access-control-expose-headers")
        .expect("access-control-expose-headers must be present on a cross-origin response")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        exposed.contains("x-total-count"),
        "x-total-count must be in the exposed-headers list, got: {exposed}"
    );
}
