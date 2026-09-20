use anyhow::{Context, Result};
use axum::routing::{get, post};
use axum::Router;
use std::net::SocketAddr;
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::assets::serve_static;
use crate::middleware::{csrf_guard, security_headers};
use crate::routes::*;
use crate::state::AppState;

pub struct WebServer;

impl WebServer {
    /// The route table.
    ///
    /// The access policy lives in each handler's extractor
    /// (`RequireAdmin` / `RequireMcr` / `RequireUser`), not here, so that a
    /// handler cannot be reached without one — a route added to this list with
    /// no extractor is public, and `tests/auth_matrix_tests.rs` walks the whole
    /// table to catch exactly that.
    ///
    /// There is deliberately **no CORS layer**. This is a same-origin app; the
    /// previous `allow_origin(Any)` made every read API scriptable from any
    /// page on the internet that an operator happened to have open.
    pub fn build_router(state: AppState) -> Router {
        Router::new()
            // ---- Public ----
            .route("/", get(handle_root))
            .route("/login", get(view_login))
            .route("/api/health", get(api_health))
            .route("/api/auth/login", post(api_login))
            .route("/api/auth/logout", post(api_logout))
            .route("/api/setup/state", get(api_setup_state))
            // ---- First run only (loopback, while no admin exists) ----
            .route("/setup", get(view_setup))
            .route("/api/setup", post(api_setup))
            // ---- Logged in (any role) ----
            .route("/user", get(view_user))
            .route("/api/auth/me", get(api_me))
            .route("/api/auth/logout-all", post(api_logout_all))
            .route("/api/auth/password", post(api_change_password))
            .route("/api/jobs/mine", get(api_get_my_jobs))
            // ---- MCR: admin, an MCR account, or an allowlisted client ----
            .route("/mcr", get(view_mcr))
            .route("/api/jobs", get(api_get_jobs).post(api_create_job))
            .route("/api/jobs/:id", get(api_get_job))
            .route("/api/jobs/:id/override", post(api_override_job))
            .route("/api/jobs/:id/retry", post(api_retry_job))
            .route("/api/jobs/:id/discard", post(api_discard_job))
            .route(
                "/api/journalists",
                get(api_get_journalists).post(api_save_journalist),
            )
            .route("/api/journalists/:surname", post(api_delete_journalist))
            .route("/api/system/status", get(api_system_status))
            .route("/api/events", get(api_events))
            // ---- Admin ----
            .route("/admin", get(view_admin))
            .route(
                "/api/admin/users",
                get(api_admin_list_users).post(api_admin_create_user),
            )
            .route("/api/admin/users/:id/password", post(api_admin_update_password))
            .route("/api/admin/purge", post(api_admin_purge))
            .route("/api/admin/vacuum", post(api_admin_vacuum))
            .route("/api/admin/dependencies", get(api_admin_dependencies))
            .route("/api/admin/update-ytdl", post(api_admin_update_ytdl))
            .route("/api/secrets", get(api_secrets_status).post(api_secrets_set))
            .route("/api/system/logs", get(api_system_logs))
            .route("/api/system/test-email", post(api_test_email))
            .route("/api/system/test-llm", post(api_test_llm))
            // ---- Static assets (public; they contain no data) ----
            .route("/static/*file", get(serve_static))
            .layer(axum::middleware::from_fn(csrf_guard))
            .layer(axum::middleware::from_fn(security_headers))
            .layer(TraceLayer::new_for_http())
            .with_state(state)
    }

    pub async fn run(
        state: AppState,
        host: &str,
        port: u16,
        shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    ) -> Result<()> {
        let addr: SocketAddr = format!("{}:{}", host, port)
            .parse()
            .with_context(|| format!("Invalid socket address {}:{}", host, port))?;

        // Resolved before the router is built, because the answer decides
        // whether the session cookie is marked `Secure`.
        let tls_cert = {
            let cfg = state.config.read().await;
            let paths = omni_core::paths::AppPaths::discover("config.json");
            cfg.tls.resolve(&paths)?
        };

        match tls_cert {
            Some(cert) => {
                // An empty passphrase is legitimate for a `.pfx` exported
                // without one, so "not set" is not an error here.
                let password = state
                    .secrets
                    .get_lossy(omni_core::secrets::keys::TLS_PASSWORD)
                    .unwrap_or_default();
                Self::run_tls(state.with_tls(true), addr, cert, password, shutdown_rx).await
            }
            None => Self::run_plain(state.with_tls(false), addr, shutdown_rx).await,
        }
    }

    async fn run_plain(
        state: AppState,
        addr: SocketAddr,
        mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    ) -> Result<()> {
        let app = Self::build_router(state);
        info!("OmniDownloader Web Server listening on http://{}", addr);

        let listener = tokio::net::TcpListener::bind(&addr).await?;

        // `into_make_service_with_connect_info` is what puts the peer address
        // in the request extensions. Without it every request looks like it has
        // no origin, and the MCR allowlist would never match — losing the
        // feature, not opening a hole, but losing it silently.
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
            info!("OmniDownloader Web Server gracefully shutting down.");
        })
        .await
        .context("Error running Axum server")?;

        Ok(())
    }

    /// Serve HTTPS (plan P2.7).
    ///
    /// Optional and off by default: the deployment is a single machine on a
    /// newsroom LAN, where the realistic choice is an IT-issued certificate or
    /// a reverse proxy, not a self-signed one that trains operators to click
    /// through warnings. When it is on, the session cookie gains `Secure`.
    ///
    /// The TLS implementation is **SChannel**, through `native-tls` — the same
    /// stack the HTTP client already uses. The alternative, rustls, needs a C
    /// toolchain at build time for whichever crypto provider it uses (`ring`
    /// wants clang on aarch64-windows; `aws-lc` wants cmake). This project
    /// builds with `cargo build --release` and nothing else, and a TLS option
    /// that breaks the build on the machine that has to produce the binary is
    /// not an option. SChannel is already on every Windows box, and a `.pfx`
    /// is what Windows IT issues anyway.
    async fn run_tls(
        state: AppState,
        addr: SocketAddr,
        cert_path: std::path::PathBuf,
        password: String,
        mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    ) -> Result<()> {
        use hyper_util::rt::{TokioExecutor, TokioIo};
        use hyper_util::server::conn::auto::Builder as ConnBuilder;
        use tower::Service;

        let pkcs12 = tokio::fs::read(&cert_path)
            .await
            .with_context(|| format!("Failed reading the TLS certificate {cert_path:?}"))?;

        let identity = native_tls::Identity::from_pkcs12(&pkcs12, &password).with_context(|| {
            format!(
                "Failed opening {cert_path:?} as a PKCS#12 certificate. It must be a .pfx/.p12 \
                 containing the certificate and its private key; the passphrase comes from the \
                 secret store (`omni-ingest secrets set web.tls_password`)."
            )
        })?;

        let acceptor = tokio_native_tls::TlsAcceptor::from(
            native_tls::TlsAcceptor::new(identity).context("Failed building the TLS acceptor")?,
        );

        let listener = tokio::net::TcpListener::bind(&addr).await?;
        info!("OmniDownloader Web Server listening on https://{}", addr);

        let app = Self::build_router(state);

        loop {
            let (stream, peer) = tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        // One refused connection is not a reason to stop
                        // serving the newsroom.
                        tracing::warn!(error = %e, "Failed accepting a TLS connection");
                        continue;
                    }
                },
                _ = shutdown_rx.recv() => {
                    info!("OmniDownloader Web Server gracefully shutting down.");
                    return Ok(());
                }
            };

            let acceptor = acceptor.clone();
            let app = app.clone();

            tokio::spawn(async move {
                let stream = match acceptor.accept(stream).await {
                    Ok(s) => s,
                    Err(e) => {
                        // A browser that rejected the certificate, a port
                        // scanner, a health check speaking plain HTTP. Common
                        // and uninteresting: debug, not warn.
                        tracing::debug!(error = %e, %peer, "TLS handshake failed");
                        return;
                    }
                };

                // The same `ConnectInfo` the plaintext path gets from
                // `into_make_service_with_connect_info`. Without it the MCR
                // allowlist would never match over HTTPS — and would fail
                // closed and silently, which is the worst way to fail.
                let service = hyper::service::service_fn(move |mut req: axum::http::Request<hyper::body::Incoming>| {
                    req.extensions_mut()
                        .insert(axum::extract::ConnectInfo(peer));
                    app.clone().call(req)
                });

                if let Err(e) = ConnBuilder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(TokioIo::new(stream), service)
                    .await
                {
                    tracing::debug!(error = %e, %peer, "HTTPS connection ended");
                }
            });
        }
    }
}
