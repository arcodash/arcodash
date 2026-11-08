//! The `OrgResolver` extension point: how the request's organization gets resolved, without
//! this crate needing to know anything about who implements it.
//!
//! OSS never implements this trait (`AppState::org_resolver` stays `None`; `resolve_org` falls
//! back to the seeded `default` org). A deployment can inject its own resolution
//! strategy by setting `AppState.org_resolver` to `Some(Arc::new(impl OrgResolver))`.

use std::future::Future;
use std::pin::Pin;

use axum::http::request::Parts;
use sea_orm::DatabaseConnection;

use arcodash_core::entities::organization;

use crate::error::ApiError;

/// A boxed, `Send` future — needed because `OrgResolver::resolve` must be usable as `dyn
/// OrgResolver` (trait objects can't have `async fn` methods directly).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A deployment can plug in its own org resolution (e.g. by `Host` or session). See module docs.
pub trait OrgResolver: Send + Sync + 'static {
    fn resolve<'a>(
        &'a self,
        db: &'a DatabaseConnection,
        parts: &'a Parts,
    ) -> BoxFuture<'a, Result<organization::Model, ApiError>>;
}
