//! Shared graceful-shutdown signal wait, used by every HTTP composition binary (`apps/backend` and
//! any embedding binary) as the future passed to `axum::serve(..).with_graceful_shutdown(..)`.

/// Resolves on SIGTERM (how an orchestrator/container runtime asks a process to stop) or Ctrl-C.
///
/// Ctrl-C covers local dev. Either begins axum's graceful drain of in-flight requests before the
/// listener closes, instead of a bare `.await?` cutting them mid-request.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received, draining in-flight requests");
}

#[cfg(test)]
mod tests {
    use super::shutdown_signal;
    use std::time::Duration;

    #[tokio::test]
    async fn does_not_resolve_without_a_signal() {
        assert!(
            tokio::time::timeout(Duration::from_millis(100), shutdown_signal())
                .await
                .is_err(),
            "shutdown_signal must only resolve on Ctrl-C/SIGTERM, not spuriously"
        );
    }
}
