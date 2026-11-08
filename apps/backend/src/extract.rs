//! `axum::Json` and `axum::extract::Path` with this API's `{"message": ...}` error envelope
//! on a malformed/invalid request, instead of Axum's default bare `text/plain` rejection.

use axum::extract::{FromRequest, FromRequestParts, OptionalFromRequest, Request};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;

pub struct Json<T>(pub T);

impl<T, S> FromRequest<S> for Json<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        <axum::Json<T> as FromRequest<S>>::from_request(req, state)
            .await
            .map(|axum::Json(v)| Self(v))
            .map_err(|rej| ApiError::new(rej.status(), rej.body_text()))
    }
}

impl<T, S> OptionalFromRequest<S> for Json<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Option<Self>, Self::Rejection> {
        Option::<axum::Json<T>>::from_request(req, state)
            .await
            .map(|opt| opt.map(|axum::Json(v)| Self(v)))
            .map_err(|rej| ApiError::new(rej.status(), rej.body_text()))
    }
}

impl<T: serde::Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

pub struct Path<T>(pub T);

impl<T, S> FromRequestParts<S> for Path<T>
where
    T: serde::de::DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(v)| Self(v))
            .map_err(|rej| ApiError::new(rej.status(), rej.body_text()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{header, Request as HttpRequest, StatusCode};
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Foo {
        #[allow(dead_code)]
        value: i32,
    }

    fn build(body: Option<&'static str>) -> Request {
        let builder = HttpRequest::builder().method("POST").uri("/");
        match body {
            Some(b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(b))
                .unwrap(),
            None => builder.body(axum::body::Body::empty()).unwrap(),
        }
    }

    #[tokio::test]
    async fn optional_json_no_body_is_none() {
        let result = Option::<Json<Foo>>::from_request(build(None), &()).await;
        assert!(matches!(result, Ok(None)));
    }

    #[tokio::test]
    async fn optional_json_valid_body_is_some() {
        let result = Option::<Json<Foo>>::from_request(build(Some(r#"{"value": 1}"#)), &()).await;
        match result {
            Ok(Some(Json(foo))) => assert_eq!(foo.value, 1),
            other => panic!("expected Ok(Some(..)), got {}", other.is_ok()),
        }
    }

    #[tokio::test]
    async fn optional_json_malformed_body_is_err() {
        let result = Option::<Json<Foo>>::from_request(build(Some("not json")), &()).await;
        match result {
            Err(e) => assert_eq!(e.status, StatusCode::BAD_REQUEST),
            Ok(_) => panic!("expected Err for malformed body"),
        }
    }

    // `Path`'s deserializer only runs against params the router extension carries, which is a
    // private axum type — so driving it end to end needs a tiny router rather than a bare Parts.
    #[tokio::test]
    async fn path_non_numeric_segment_is_bad_request_json() {
        use axum::routing::get;
        use tower::ServiceExt;

        async fn handler(Path(id): Path<i64>) -> String {
            id.to_string()
        }

        let app = axum::Router::new().route("/items/{id}", get(handler));
        let req = HttpRequest::builder()
            .method("GET")
            .uri("/items/abc")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("message").is_some());
    }
}
