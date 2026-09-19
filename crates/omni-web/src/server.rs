use anyhow::{Context, Result};
use axum::routing::{get, post};
use axum::Router;
use std::net::SocketAddr;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::routes::*;
use crate::state::AppState;

pub struct WebServer;

impl WebServer {
    pub fn build_router(state: AppState) -> Router {
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);

        Router::new()
            // HTML Pages
            .route("/", get(handle_root))
            .route("/login", get(view_login))
            .route("/user", get(view_user))
            .route("/mcr", get(view_mcr))
            .route("/admin", get(view_admin))
            .route("/setup", get(view_setup))
            // Auth API
            .route("/api/auth/login", post(api_login))
            .route("/api/auth/logout", post(api_logout))
            .route("/api/auth/me", get(api_me))
            // Jobs API
            .route("/api/jobs", get(api_get_jobs).post(api_create_job))
            .route("/api/jobs/:id/override", post(api_override_job))
            .route("/api/jobs/:id/retry", post(api_retry_job))
            .route("/api/jobs/:id/discard", post(api_discard_job))
            // Journalists API
            .route("/api/journalists", get(api_get_journalists).post(api_save_journalist))
            .route("/api/journalists/:surname", post(api_delete_journalist))
            // Admin API
            .route("/api/admin/users", get(api_admin_list_users).post(api_admin_create_user))
            .route("/api/admin/users/:id/password", post(api_admin_update_password))
            .route("/api/admin/purge", post(api_admin_purge))
            .route("/api/admin/vacuum", post(api_admin_vacuum))
            .route("/api/admin/dependencies", get(api_admin_dependencies))
            .route("/api/admin/update-ytdl", post(api_admin_update_ytdl))
            // System API
            .route("/api/system/status", get(api_system_status))
            .route("/api/system/logs", get(api_system_logs))
            .route("/api/system/test-email", post(api_test_email))
            .route("/api/system/test-llm", post(api_test_llm))
            .route("/api/setup", post(api_setup))
            // Real-time Events (SSE)
            .route("/api/events", get(api_events))
            .layer(cors)
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

        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.recv().await;
                info!("OmniDownloader Web Server gracefully shutting down.");
            })
            .await
            .context("Error running Axum server")?;

        Ok(())
    }
}
