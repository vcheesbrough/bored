//! Binding the listener, serving, and stopping cleanly.
//!
//! Before card #415 `main` called `axum::serve(..).await.unwrap()` and had no
//! shutdown path at all: `docker stop` sent SIGTERM, nothing handled it, and
//! ten seconds later Docker's SIGKILL ended the process mid-whatever. Nothing
//! could be flushed, because nothing ran after the serve call.
//!
//! Now: on SIGTERM or Ctrl-C the server raises [`Draining`] (which ends every
//! SSE stream — they never end on their own), stops accepting, gives in-flight
//! requests up to [`DRAIN_TIMEOUT`] to finish, and returns — so `main` can
//! flush telemetry, including the spans of the streams it just closed. The
//! drain is **bounded** as well, so no single slow request can hold shutdown
//! past Docker's stop timeout.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum_server::tls_rustls::RustlsConfig; // TLS support using rustls (pure-Rust TLS)

use crate::config::ServerConfig;

/// "The server is draining": raised once, when the shutdown signal arrives,
/// and observable by any number of waiters.
///
/// Long-lived responses watch it and end themselves — the SSE stream above
/// all, which otherwise never ends. A `watch` channel rather than a `Notify`
/// because a waiter that starts *after* the signal must still see it.
/// Cloning shares the one channel (it lives behind an `Arc`).
#[derive(Clone)]
pub(crate) struct Draining(std::sync::Arc<tokio::sync::watch::Sender<bool>>);

impl Draining {
    pub(crate) fn new() -> Self {
        // `watch::channel` returns (Sender, Receiver); receivers are made on
        // demand by `subscribe`, so the initial one is dropped.
        let (sender, _) = tokio::sync::watch::channel(false);
        Self(std::sync::Arc::new(sender))
    }

    /// Raise the signal. Idempotent.
    pub(crate) fn start(&self) {
        self.0.send_replace(true);
    }

    /// Resolves once the signal has been raised (at once if it already was).
    pub(crate) fn started(&self) -> impl Future<Output = ()> + Send + use<> {
        let mut receiver = self.0.subscribe();
        async move {
            // `wait_for` checks the current value first, then waits for
            // changes. It errs only once every `Draining` clone (and so the
            // sender) is gone — the state that owned it has been dropped —
            // and nothing will ever raise it then, so wait forever rather
            // than treat that as a shutdown.
            if receiver.wait_for(|draining| *draining).await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

/// How long open connections get to finish after the shutdown signal. With
/// `observability::FLUSH_TIMEOUT` and the runtime's own bound this stays inside
/// the compose file's `stop_grace_period`.
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_secs(4);

/// Serve `app` according to `server`, until `shutdown` completes.
///
/// A TLS pair present ⇒ HTTPS on :443 (the deployed shape). Otherwise plain
/// HTTP on `server.http-port` (dev mode, e2e). When `shutdown` completes,
/// `draining` is raised first — so SSE streams end and their connections can
/// close — then the listener stops accepting and open requests get
/// [`DRAIN_TIMEOUT`] to finish.
pub(crate) async fn serve(
    server: &ServerConfig,
    app: Router,
    draining: Draining,
    shutdown: impl Future<Output = ()> + Send + 'static,
) {
    match server.tls_pair() {
        Some((cert, key)) => {
            let tls_config = RustlsConfig::from_pem_file(cert, key)
                .await
                .expect("failed to load TLS config");
            // `[0, 0, 0, 0]` means bind to all network interfaces (0.0.0.0).
            let addr = SocketAddr::from(([0, 0, 0, 0], 443));
            tracing::info!(%addr, "bored backend listening (TLS)");
            // axum-server's `Handle` is how a running server is told to stop:
            // `graceful_shutdown(Some(d))` stops accepting at once and closes
            // whatever is still open after `d`.
            let handle = axum_server::Handle::new();
            let stopper = handle.clone();
            tokio::spawn(async move {
                shutdown.await;
                tracing::info!("shutdown signal received, draining connections");
                draining.start();
                stopper.graceful_shutdown(Some(DRAIN_TIMEOUT));
            });
            if let Err(error) = axum_server::bind_rustls(addr, tls_config)
                .handle(handle)
                .serve(app.into_make_service())
                .await
            {
                tracing::error!(error = %error, "server stopped with an error");
            }
        }
        None => {
            let addr = SocketAddr::from(([0, 0, 0, 0], server.http_port));
            // `tokio::net::TcpListener` is the async equivalent of the standard
            // library's `TcpListener` — it doesn't block the thread while waiting.
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .expect("failed to bind the HTTP listener");
            tracing::info!(%addr, "bored backend listening (plain HTTP)");
            serve_plain(listener, app, draining, shutdown).await;
        }
    }
    tracing::info!("server stopped");
}

/// Plain-HTTP serving with a bounded drain. Split out so a test can run it on
/// an ephemeral port and a shutdown signal it controls.
pub(crate) async fn serve_plain(
    listener: tokio::net::TcpListener,
    app: Router,
    draining: Draining,
    shutdown: impl Future<Output = ()> + Send + 'static,
) {
    // axum's graceful shutdown waits on this future: once `draining` is
    // raised it stops accepting and waits for open connections to finish.
    let server = axum::serve(listener, app).with_graceful_shutdown(draining.started());
    // Run the server as its own task so this function can stop waiting for it
    // after the drain timeout, rather than for as long as the slowest client.
    let mut task = tokio::spawn(async move { server.await });

    tokio::select! {
        // The server ended by itself (it only does on an I/O error).
        result = &mut task => {
            report(result);
            return;
        }
        () = shutdown => {
            tracing::info!("shutdown signal received, draining connections");
        }
    }
    // Raising the signal also ends every SSE stream (events.rs), so an open
    // board tab no longer holds the drain open.
    draining.start();
    match tokio::time::timeout(DRAIN_TIMEOUT, &mut task).await {
        Ok(result) => report(result),
        Err(_) => {
            tracing::warn!(
                timeout_secs = DRAIN_TIMEOUT.as_secs(),
                "connections still open after the drain timeout; giving up on them"
            );
            // Stop waiting. `abort` cancels the serve loop; the connections it
            // had spawned run as tasks of their own and are dropped when the
            // runtime shuts down (`main`'s bounded `shutdown_timeout`), after
            // the telemetry flush. Only a request that ignores the drain signal
            // for the whole timeout gets here.
            task.abort();
        }
    }
}

/// Log how the server task ended, if it ended badly.
fn report(result: Result<std::io::Result<()>, tokio::task::JoinError>) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(error = %error, "server stopped with an error"),
        Err(error) => tracing::error!(error = %error, "server task failed"),
    }
}

/// Resolves on SIGTERM (what `docker stop` sends; tini forwards it to us) or
/// Ctrl-C (SIGINT, a local run).
pub(crate) async fn shutdown_signal() {
    let ctrl_c = async {
        // An error here means no handler could be installed; waiting forever
        // then just leaves SIGTERM (or SIGKILL) as the way out.
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
