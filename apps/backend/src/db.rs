//! `Db`: the connection a handler/service runs queries on: the pooled connection in OSS mode, or a
//! per-request transaction begun by a composition binary's [`RequestTxnHook`].
//!
//! Code is written against `Db` so a composition binary can swap every request onto its own
//! transaction without call-site changes; OSS never sets the hook, so [`request_db`] always
//! resolves to `Db::Pool`.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use sea_orm::{
    AccessMode, ActiveModelBehavior, ActiveModelTrait, ConnectionTrait, DatabaseConnection,
    DatabaseTransaction, DbBackend, DbErr, EntityTrait, ExecResult, IntoActiveModel,
    IsolationLevel, QueryResult, Statement, TransactionError, TransactionOptions, TransactionTrait,
};

use arcodash_core::entities::organization;

use crate::auth::BoxFuture;
use crate::error::ApiError;
use crate::state::AppState;

/// A job registered via [`Db::after_commit`], run once the enclosing per-request transaction
/// commits.
pub type PostCommitJob = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Jobs queued by [`Db::after_commit`] inside a per-request transaction; shared by every `Db::Txn`
/// of the request and drained by the owning middleware's [`RequestTxn`] after commit.
#[derive(Clone, Default)]
pub struct PostCommitJobs(Arc<Mutex<Vec<PostCommitJob>>>);

impl PostCommitJobs {
    fn push(&self, job: PostCommitJob) {
        self.0.lock().unwrap().push(job);
    }

    /// Runs and clears every registered job in order; call only after the transaction they were
    /// deferred against has committed.
    pub async fn run_all(&self) {
        let jobs = std::mem::take(&mut *self.0.lock().unwrap());
        for job in jobs {
            job.await;
        }
    }
}

/// The connection a handler runs on: the pooled connection (OSS) or a per-request transaction from
/// a composition binary's [`RequestTxnHook`].
#[derive(Clone)]
pub enum Db {
    Pool(DatabaseConnection),
    Txn(Arc<DatabaseTransaction>, PostCommitJobs),
}

impl Db {
    /// Runs `job` once any enclosing transaction has committed: immediately on a pooled connection,
    /// queued for `middleware::org::inject_current_org` on `Db::Txn`. Without this, a job handing
    /// work to another connection (e.g. a worker enqueue) could run before this request's writes
    /// are visible.
    pub async fn after_commit(&self, job: PostCommitJob) {
        match self {
            Self::Pool(_) => job.await,
            Self::Txn(_, jobs) => jobs.push(job),
        }
    }

    /// Fallible counterpart to [`after_commit`](Db::after_commit). In `Db::Pool` mode the `Result`
    /// is returned; in `Db::Txn` mode the job runs after the response is built, so this returns
    /// `Ok(())` immediately and a failure is only logged.
    pub async fn after_commit_fallible(
        &self,
        job: Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send>>,
    ) -> Result<(), ApiError> {
        match self {
            Self::Pool(_) => job.await,
            Self::Txn(_, jobs) => {
                jobs.push(Box::pin(async move {
                    if let Err(e) = job.await {
                        tracing::error!(
                            error = %e.message,
                            status = %e.status,
                            "post-commit job failed after its response had already been built"
                        );
                    }
                }));
                Ok(())
            }
        }
    }
}

#[async_trait::async_trait]
impl ConnectionTrait for Db {
    fn get_database_backend(&self) -> DbBackend {
        match self {
            Self::Pool(db) => db.get_database_backend(),
            Self::Txn(txn, _) => txn.get_database_backend(),
        }
    }

    async fn execute_raw(&self, stmt: Statement) -> Result<ExecResult, DbErr> {
        match self {
            Self::Pool(db) => db.execute_raw(stmt).await,
            Self::Txn(txn, _) => txn.execute_raw(stmt).await,
        }
    }

    async fn execute_unprepared(&self, sql: &str) -> Result<ExecResult, DbErr> {
        match self {
            Self::Pool(db) => db.execute_unprepared(sql).await,
            Self::Txn(txn, _) => txn.execute_unprepared(sql).await,
        }
    }

    async fn query_one_raw(&self, stmt: Statement) -> Result<Option<QueryResult>, DbErr> {
        match self {
            Self::Pool(db) => db.query_one_raw(stmt).await,
            Self::Txn(txn, _) => txn.query_one_raw(stmt).await,
        }
    }

    async fn query_all_raw(&self, stmt: Statement) -> Result<Vec<QueryResult>, DbErr> {
        match self {
            Self::Pool(db) => db.query_all_raw(stmt).await,
            Self::Txn(txn, _) => txn.query_all_raw(stmt).await,
        }
    }

    fn support_returning(&self) -> bool {
        match self {
            Self::Pool(db) => db.support_returning(),
            Self::Txn(txn, _) => txn.support_returning(),
        }
    }

    fn is_mock_connection(&self) -> bool {
        match self {
            Self::Pool(db) => db.is_mock_connection(),
            Self::Txn(txn, _) => txn.is_mock_connection(),
        }
    }
}

/// `DatabaseConnection` and `DatabaseTransaction` (via a nested savepoint) both implement
/// `TransactionTrait`, so `Db` just dispatches and services get a real `.begin()`/commit path in
/// production.
#[async_trait::async_trait]
impl TransactionTrait for Db {
    type Transaction = DatabaseTransaction;

    async fn begin(&self) -> Result<DatabaseTransaction, DbErr> {
        match self {
            Self::Pool(db) => db.begin().await,
            Self::Txn(txn, _) => txn.begin().await,
        }
    }

    async fn begin_with_config(
        &self,
        isolation_level: Option<IsolationLevel>,
        access_mode: Option<AccessMode>,
    ) -> Result<DatabaseTransaction, DbErr> {
        match self {
            Self::Pool(db) => db.begin_with_config(isolation_level, access_mode).await,
            Self::Txn(txn, _) => txn.begin_with_config(isolation_level, access_mode).await,
        }
    }

    async fn begin_with_options(
        &self,
        options: TransactionOptions,
    ) -> Result<DatabaseTransaction, DbErr> {
        match self {
            Self::Pool(db) => db.begin_with_options(options).await,
            Self::Txn(txn, _) => txn.begin_with_options(options).await,
        }
    }

    async fn transaction<F, T, E>(&self, callback: F) -> Result<T, TransactionError<E>>
    where
        F: for<'c> FnOnce(
                &'c DatabaseTransaction,
            ) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'c>>
            + Send,
        T: Send,
        E: std::fmt::Display + std::fmt::Debug + Send,
    {
        match self {
            Self::Pool(db) => db.transaction(callback).await,
            Self::Txn(txn, _) => txn.transaction(callback).await,
        }
    }

    async fn transaction_with_config<F, T, E>(
        &self,
        callback: F,
        isolation_level: Option<IsolationLevel>,
        access_mode: Option<AccessMode>,
    ) -> Result<T, TransactionError<E>>
    where
        F: for<'c> FnOnce(
                &'c DatabaseTransaction,
            ) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'c>>
            + Send,
        T: Send,
        E: std::fmt::Display + std::fmt::Debug + Send,
    {
        match self {
            Self::Pool(db) => {
                db.transaction_with_config(callback, isolation_level, access_mode)
                    .await
            }
            Self::Txn(txn, _) => {
                txn.transaction_with_config(callback, isolation_level, access_mode)
                    .await
            }
        }
    }
}

/// Inserts inside a nested scope so a handled constraint violation leaves the enclosing transaction
/// usable (Postgres aborts a transaction on any failed statement).
///
/// Returns the raw `DbErr` so `error::is_unique_violation` works as on a direct insert.
pub async fn insert_scoped<A, C>(
    db: &C,
    model: A,
) -> Result<<A::Entity as EntityTrait>::Model, DbErr>
where
    C: ConnectionTrait + TransactionTrait<Transaction = DatabaseTransaction>,
    A: ActiveModelTrait + ActiveModelBehavior + Send + 'static,
    <A::Entity as EntityTrait>::Model: IntoActiveModel<A> + Send,
{
    db.transaction(|txn| Box::pin(async move { model.insert(txn).await }))
        .await
        .map_err(|e| match e {
            TransactionError::Transaction(e) | TransactionError::Connection(e) => e,
        })
}

/// The per-request transaction, held in one place so a handler can end it early
/// ([`RequestScope::detach`]); `slot.take()` returning `None` at commit time tells the middleware
/// to skip its commit.
#[derive(Clone)]
pub struct RequestTxnSlot(Arc<Mutex<Option<Arc<DatabaseTransaction>>>>);

impl RequestTxnSlot {
    #[must_use]
    pub fn new(txn: Arc<DatabaseTransaction>) -> Self {
        Self(Arc::new(Mutex::new(Some(txn))))
    }

    fn get(&self) -> Option<Arc<DatabaseTransaction>> {
        self.0.lock().unwrap().clone()
    }

    pub(crate) fn take(&self) -> Option<Arc<DatabaseTransaction>> {
        self.0.lock().unwrap().take()
    }
}

/// Inserted into request extensions by `middleware::org::inject_current_org` once a
/// `RequestTxnHook` begins a transaction (never in OSS).
///
/// The second field is the [`PostCommitJobs`] shared by every `Db::Txn` of the request.
#[derive(Clone)]
pub struct RequestTxn(pub RequestTxnSlot, pub PostCommitJobs);

impl<S> FromRequestParts<S> for Db
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    #[allow(
        clippy::unused_async_trait_impl,
        reason = "keeps the async-fn form every extractor impl here uses; the body just has nothing to await"
    )]
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);
        Ok(request_db(parts, &app_state))
    }
}

/// Non-extractor equivalent of `Db`'s `FromRequestParts`, for code holding `Parts` directly.
///
/// Used by e.g. `auth::mod`. After [`RequestScope::detach`] the slot is empty and this falls to
/// `Db::Pool`, which under RLS sees zero rows; calling it after a detach is a caller bug.
pub fn request_db(parts: &Parts, state: &AppState) -> Db {
    parts
        .extensions
        .get::<RequestTxn>()
        .and_then(|RequestTxn(slot, jobs)| Some(Db::Txn(slot.get()?, jobs.clone())))
        .unwrap_or_else(|| Db::Pool(state.db.clone()))
}

/// Lets a handler end the per-request transaction around a slow external call.
///
/// End it before awaiting a slow external system (LLM, customer warehouse), then re-open a short
/// org-scoped one, so the request doesn't hold an idle-in-transaction pooled connection.
#[derive(Clone)]
pub struct RequestScope {
    pool: DatabaseConnection,
    hook: Option<Arc<dyn RequestTxnHook>>,
    txn: Option<(RequestTxnSlot, PostCommitJobs)>,
}

impl RequestScope {
    /// A scope with no per-request transaction to release (pooled; tests and OSS paths).
    #[must_use]
    pub fn pooled(db: DatabaseConnection) -> Self {
        Self {
            pool: db,
            hook: None,
            txn: None,
        }
    }

    /// Commits and releases the per-request transaction. Takes `db` by value so nothing can use it
    /// afterwards. A no-op without a transaction (OSS or [`Self::pooled`]) and idempotent.
    pub async fn detach(&self, db: Db) -> Result<(), ApiError> {
        drop(db);
        let Some((slot, jobs)) = &self.txn else {
            return Ok(());
        };
        let Some(txn) = slot.take() else {
            return Ok(());
        };
        if let Ok(txn) = Arc::try_unwrap(txn) {
            txn.commit().await.map_err(|e| {
                tracing::error!(error = %e, "failed to commit per-request transaction on early detach");
                ApiError::internal("internal server error")
            })?;
            // Post-commit jobs must fire here: the middleware will find the slot empty and skip
            // its run.
            jobs.run_all().await;
            Ok(())
        } else {
            // Something cloned the `Arc<DatabaseTransaction>` past the `drop(db)`; dropping it
            // would silently roll back a transaction the caller thinks succeeded, so fail
            // loudly.
            tracing::error!(
                "per-request transaction was still referenced at detach time; refusing to \
                 drop it silently"
            );
            Err(ApiError::internal("internal server error"))
        }
    }

    /// Opens a transaction scoped to `org` for one short unit of work via the same `RequestTxnHook`
    /// (plain pooled connection, no transaction, in OSS).
    pub async fn begin(&self, org: &organization::Model) -> Result<ScopedTxn, ApiError> {
        let Some(hook) = &self.hook else {
            return Ok(ScopedTxn {
                db: Db::Pool(self.pool.clone()),
                txn: None,
                jobs: PostCommitJobs::default(),
            });
        };
        let txn = Arc::new(hook.begin(&self.pool, org).await?);
        let jobs = PostCommitJobs::default();
        Ok(ScopedTxn {
            db: Db::Txn(txn.clone(), jobs.clone()),
            txn: Some(txn),
            jobs,
        })
    }
}

impl<S> FromRequestParts<S> for RequestScope
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    #[allow(
        clippy::unused_async_trait_impl,
        reason = "keeps the async-fn form every extractor impl here uses; the body just has nothing to await"
    )]
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);
        let txn = parts
            .extensions
            .get::<RequestTxn>()
            .map(|RequestTxn(slot, jobs)| (slot.clone(), jobs.clone()));
        Ok(Self {
            pool: app_state.db.clone(),
            hook: app_state.request_txn_hook.clone(),
            txn,
        })
    }
}

/// A short transaction from [`RequestScope::begin`]. Dropping it without [`Self::commit`] rolls it
/// back, the right outcome for an early `?` return.
pub struct ScopedTxn {
    db: Db,
    txn: Option<Arc<DatabaseTransaction>>,
    jobs: PostCommitJobs,
}

impl ScopedTxn {
    #[must_use]
    pub const fn db(&self) -> &Db {
        &self.db
    }

    pub async fn commit(self) -> Result<(), ApiError> {
        let Self { db, txn, jobs } = self;
        drop(db);
        let Some(txn) = txn else {
            return Ok(());
        };
        if let Ok(txn) = Arc::try_unwrap(txn) {
            txn.commit().await.map_err(|e| {
                tracing::error!(error = %e, "failed to commit scoped transaction");
                ApiError::internal("internal server error")
            })?;
            jobs.run_all().await;
            Ok(())
        } else {
            tracing::error!(
                "scoped transaction was still referenced at commit time; refusing to drop \
                 it silently"
            );
            Err(ApiError::internal("internal server error"))
        }
    }
}

/// Lets a deployment plug in how the per-request transaction begins; `None` by default, so requests
/// run on the pooled connection.
pub trait RequestTxnHook: Send + Sync + 'static {
    fn begin<'a>(
        &'a self,
        db: &'a DatabaseConnection,
        org: &'a organization::Model,
    ) -> BoxFuture<'a, Result<DatabaseTransaction, ApiError>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn post_commit_job_does_not_run_until_run_all_is_called() {
        let jobs = PostCommitJobs::default();
        let ran = Arc::new(AtomicUsize::new(0));

        let ran_clone = ran.clone();
        jobs.push(Box::pin(async move {
            ran_clone.fetch_add(1, Ordering::SeqCst);
        }));

        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "a queued job must not run before run_all is called"
        );
        jobs.run_all().await;
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "run_all must run the queued job exactly once"
        );
    }

    #[tokio::test]
    async fn post_commit_jobs_run_in_registration_order() {
        let jobs = PostCommitJobs::default();
        let order = Arc::new(Mutex::new(Vec::new()));

        for i in 0..3 {
            let order = order.clone();
            jobs.push(Box::pin(async move {
                order.lock().unwrap().push(i);
            }));
        }

        jobs.run_all().await;
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn run_all_clears_the_queue_so_jobs_never_run_twice() {
        let jobs = PostCommitJobs::default();
        let ran = Arc::new(AtomicUsize::new(0));
        let ran_clone = ran.clone();
        jobs.push(Box::pin(async move {
            ran_clone.fetch_add(1, Ordering::SeqCst);
        }));

        jobs.run_all().await;
        jobs.run_all().await;

        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pooled_scope_detach_is_a_noop() {
        let scope = RequestScope::pooled(DatabaseConnection::default());
        let db = Db::Pool(DatabaseConnection::default());
        assert!(scope.detach(db).await.is_ok());
    }

    #[tokio::test]
    async fn pooled_scope_begin_yields_pool_and_commits_cleanly() {
        let scope = RequestScope::pooled(DatabaseConnection::default());
        let org = organization::Model {
            id: 1,
            name: "test".to_string(),
            slug: "test".to_string(),
            settings: serde_json::json!({}),
            created_at: chrono::Utc::now().into(),
            updated_at: chrono::Utc::now().into(),
        };
        let scoped = scope.begin(&org).await.expect("begin must not fail");
        assert!(matches!(scoped.db(), Db::Pool(_)));
        assert!(scoped.commit().await.is_ok());
    }

    /// Seeds the slot with a real transaction so `take()` clearing it is actually exercised. Needs
    /// `TEST_DATABASE_URL`.
    #[tokio::test]
    async fn request_txn_slot_take_returns_none_on_second_call() {
        let Ok(db_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return;
        };
        let db = arcodash_core::db::connect(&db_url).await.unwrap();
        let txn = Arc::new(db.begin().await.expect("begin failed"));
        let slot = RequestTxnSlot::new(txn);

        let taken = slot.take();
        assert!(
            taken.is_some(),
            "first take() must return the seeded transaction"
        );
        assert!(
            slot.take().is_none(),
            "second take() must find the slot already empty"
        );

        Arc::try_unwrap(taken.unwrap())
            .unwrap_or_else(|_| panic!("txn still shared"))
            .rollback()
            .await
            .expect("rollback failed");
    }
}
