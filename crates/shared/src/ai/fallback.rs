//! Provider fallback loop shared by the backend and the worker. Per-attempt bookkeeping (status,
//! usage, metrics) is the caller's, injected through [`AttemptSink`].

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use super::settings::{provider_free_tier, provider_status_str, ResolvedProvider};
use super::{AiError, AiProvider, AiProviderKind};

/// Whether a generation failure against one provider in the chain should trigger a fallback attempt
/// at the next entry.
///
/// A bad prompt or a transient network blip should not hop providers — only an error that means
/// "this provider itself won't work right now" (bad key, quota, overloaded) does.
#[must_use]
pub const fn should_attempt_fallback(err: &AiError) -> bool {
    matches!(
        err,
        AiError::InvalidKey
            | AiError::QuotaExceeded { .. }
            | AiError::RateLimited { .. }
            | AiError::Unavailable(_)
    )
}

/// A transient failure hops to the next provider only while the chain is younger than this, so a
/// slow late 5xx can't stack a second full provider timeout past a reverse proxy's request limit.
pub const TRANSIENT_FALLBACK_WINDOW: Duration = Duration::from_secs(30);

/// Hard ceiling for a whole fallback chain, kept under a typical ~100s proxy request limit.
pub const TOTAL_CALL_CEILING: Duration = Duration::from_secs(95);

/// A hop needs at least this much of the ceiling left to be worth attempting.
pub const MIN_HOP_BUDGET: Duration = Duration::from_secs(5);

/// Time left for a hopped attempt, or `None` when too little remains to try one.
#[must_use]
pub fn hop_budget(chain_elapsed: Duration) -> Option<Duration> {
    let remaining = TOTAL_CALL_CEILING.saturating_sub(chain_elapsed);
    (remaining >= MIN_HOP_BUDGET).then_some(remaining)
}

#[must_use]
pub fn may_fallback(err: &AiError, chain_elapsed: Duration) -> bool {
    should_attempt_fallback(err)
        && (!matches!(err, AiError::Unavailable(_)) || chain_elapsed < TRANSIENT_FALLBACK_WINDOW)
        && hop_budget(chain_elapsed).is_some()
}

/// Runs a hopped attempt under `budget`; a timeout is a plain network error, which never hops
/// again.
pub async fn bounded_hop<T>(
    fut: impl Future<Output = Result<T, AiError>>,
    budget: Duration,
) -> Result<T, AiError> {
    tokio::time::timeout(budget, fut).await.unwrap_or_else(|_| {
        Err(AiError::Network(
            "timed out waiting for the fallback provider".to_string(),
        ))
    })
}

/// Boxed async result future.
///
/// Matches the shape an `async_trait`-generated trait method already returns, so a `call` closure
/// passed to [`run_fallback_chain`] can just point at the trait method itself (e.g. `|p, http, key,
/// model, prompt| p.generate_sql(http, key, model, prompt)`) rather than wrapping it in `async
/// move`.
pub type GenFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AiError>> + Send + 'a>>;

/// One finished provider call, handed to the [`AttemptSink`].
pub struct Attempt<'a> {
    pub kind: AiProviderKind,
    pub model: &'a str,
    pub feature: &'a str,
    pub status: &'static str,
    pub free_tier: bool,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
}

/// Receives every attempt of a chain, successful or not. Implementations are best-effort: they
/// must not fail the generation.
#[async_trait::async_trait]
pub trait AttemptSink: Send + Sync {
    async fn record(&self, attempt: Attempt<'_>);

    /// Pre-flight spend-cap check, run once before the first provider call. An `Err` aborts the
    /// chain with no provider call and no recorded attempt.
    async fn check_budget(&self) -> Result<(), AiError>;
}

/// Tries each entry in `chain` in priority order until one succeeds, reporting every attempt to
/// `sink`.
///
/// Continues to the next entry only when `retry_enabled` and [`may_fallback`] allow it; otherwise
/// (or once the chain is exhausted) returns the last attempt's outcome. Generic over the generation
/// result `T`: `call` invokes the trait method, `token_accessor` extracts `(tokens_in, tokens_out)`
/// from a success. `provider_lookup` stands in for `provider_for` so tests can inject a fake.
#[allow(clippy::too_many_arguments)]
pub async fn run_fallback_chain<T, F, C, TokFn>(
    http: &reqwest::Client,
    chain: &[ResolvedProvider],
    retry_enabled: bool,
    feature: &str,
    prompt: &str,
    provider_lookup: F,
    call: C,
    token_accessor: TokFn,
    sink: &dyn AttemptSink,
) -> (AiProviderKind, Result<T, AiError>)
where
    F: Fn(AiProviderKind, Option<String>) -> Box<dyn AiProvider>,
    C: for<'a> Fn(
        &'a dyn AiProvider,
        &'a reqwest::Client,
        &'a str,
        &'a str,
        &'a str,
    ) -> GenFuture<'a, T>,
    TokFn: Fn(&T) -> (Option<i64>, Option<i64>),
{
    if let Err(e) = sink.check_budget().await {
        return (chain[0].kind, Err(e));
    }
    let chain_start = Instant::now();
    let mut iter = chain.iter().peekable();
    let mut hopped = false;
    loop {
        let entry = iter.next().expect("chain has at least one entry");
        let model = entry.model.as_deref().unwrap_or_default();
        let provider = provider_lookup(entry.kind, entry.base_url.clone());
        let attempt = call(provider.as_ref(), http, &entry.api_key, model, prompt);
        let outcome = if hopped {
            let budget = hop_budget(chain_start.elapsed()).unwrap_or(MIN_HOP_BUDGET);
            bounded_hop(attempt, budget).await
        } else {
            attempt.await
        };
        hopped = true;

        let status = provider_status_str(&outcome);
        let free_tier = provider_free_tier(&outcome);
        let (tokens_in, tokens_out) = outcome.as_ref().map_or((None, None), &token_accessor);
        sink.record(Attempt {
            kind: entry.kind,
            model,
            feature,
            status,
            free_tier,
            tokens_in,
            tokens_out,
        })
        .await;

        let retry = retry_enabled
            && matches!(&outcome, Err(e) if may_fallback(e, chain_start.elapsed()))
            && iter.peek().is_some();
        if !retry {
            return (entry.kind, outcome);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `should_attempt_fallback` is the whole gate on whether a chain retry happens — locks its
    /// truth table for every `AiError` variant.
    #[test]
    fn should_attempt_fallback_for_provider_level_failures() {
        assert!(should_attempt_fallback(&AiError::InvalidKey));
        assert!(should_attempt_fallback(&AiError::QuotaExceeded {
            free_tier: false
        }));
        assert!(should_attempt_fallback(&AiError::RateLimited {
            retry_after_secs: 10,
            free_tier: false
        }));
        assert!(should_attempt_fallback(&AiError::Unavailable(
            "503".to_string()
        )));
        assert!(!should_attempt_fallback(&AiError::BudgetExceeded {
            cap: 1.0,
            spent: 2.0,
            resets_at: chrono::Utc::now()
        }));
        assert!(!should_attempt_fallback(&AiError::Network(
            "timeout".to_string()
        )));
        assert!(!should_attempt_fallback(&AiError::GenerationFailed(
            "bad json".to_string()
        )));
    }

    #[test]
    fn may_fallback_bounds_transient_errors_by_elapsed() {
        let busy = AiError::Unavailable("504".to_string());
        assert!(may_fallback(&busy, Duration::from_secs(5)));
        assert!(!may_fallback(&busy, Duration::from_secs(45)));
        assert!(may_fallback(&AiError::InvalidKey, Duration::from_secs(45)));
        assert!(may_fallback(
            &AiError::QuotaExceeded { free_tier: false },
            Duration::from_secs(45)
        ));
        assert!(may_fallback(
            &AiError::RateLimited {
                retry_after_secs: 10,
                free_tier: false
            },
            Duration::from_secs(45)
        ));
        assert!(!may_fallback(
            &AiError::Network("x".to_string()),
            Duration::from_secs(5)
        ));
    }

    #[test]
    fn hop_budget_skips_when_under_minimum() {
        assert_eq!(
            hop_budget(Duration::from_secs(29)),
            Some(Duration::from_secs(66))
        );
        assert_eq!(
            hop_budget(Duration::from_secs(90)),
            Some(Duration::from_secs(5))
        );
        assert_eq!(hop_budget(Duration::from_secs(91)), None);
        assert!(!may_fallback(&AiError::InvalidKey, Duration::from_secs(92)));
    }

    #[tokio::test]
    async fn bounded_hop_cuts_off_slow_fallback_without_rehopping() {
        let slow = async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok::<(), AiError>(())
        };
        let started = Instant::now();
        let r = bounded_hop(slow, Duration::from_millis(50)).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        let err = r.unwrap_err();
        assert!(matches!(err, AiError::Network(_)));
        assert!(!should_attempt_fallback(&err));
    }

    struct DeniedSink(std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl AttemptSink for DeniedSink {
        async fn record(&self, _attempt: Attempt<'_>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        async fn check_budget(&self) -> Result<(), AiError> {
            Err(AiError::BudgetExceeded {
                cap: 1.0,
                spent: 2.0,
                resets_at: chrono::Utc::now(),
            })
        }
    }

    #[tokio::test]
    async fn budget_denial_skips_every_provider_and_records_nothing() {
        let chain = vec![ResolvedProvider {
            kind: AiProviderKind::Gemini,
            api_key: "k".into(),
            model: None,
            base_url: None,
        }];
        let sink = DeniedSink(Default::default());
        let (_, out) = run_fallback_chain::<(), _, _, _>(
            &reqwest::Client::new(),
            &chain,
            true,
            "sql_generation",
            "p",
            |_, _| panic!("provider looked up despite exhausted budget"),
            |_, _, _, _, _| panic!("provider called despite exhausted budget"),
            |_: &()| (None, None),
            &sink,
        )
        .await;
        assert!(matches!(out, Err(AiError::BudgetExceeded { .. })));
        assert_eq!(sink.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
