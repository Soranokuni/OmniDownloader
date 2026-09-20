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
        mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    ) -> Result<()> {
        let app = Self::build_router(state);
        let addr: SocketAddr = format!("{}:{}", host, port)
            .parse()
            .with_context(|| format!("Invalid socket address {}:{}", host, port))?;

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
}
