//! The first-setup wizard's backend and the settings it edits (plan P7.12).
//!
//! The wizard is a front for permanent endpoints: everything here stays useful
//! after setup, for an administrator who wants to repoint the watchfolder or
//! change the mailbox without editing config.json.
//!
//! Every handler here is Admin-only, and a session of the first-run `admin`
//! account is refused by `RequireAdmin` like everywhere else (P2.14). The one
//! exception is [`api_wizard_account`], which exists precisely for that session:
//! it swaps the default account for a real one.
//!
//! Each change reports `restart_required`: true when the running daemon read the
//! setting once at start-up and will use the saved value only after a restart.
//! Live: `setup_*`, `security.mcr_open_networks`, `security.trust_proxy_header`,
//! `security.trusted_proxies`, `metrics.allow_networks` (read per request).
//! Restart: watchfolder (the worker pool, recovery and the disk checks captured
//! the path), mail settings (the watcher is built at start), web port, TLS,
//! `security.allowed_hosts`, `metrics.enabled`, archive.

use std::path::Path;
use std::time::Duration;

use axum::extract::State;
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Duration as ChronoDuration;
use serde::Deserialize;

use omni_core::auth::generate_session_token;
use omni_core::config::AppConfig;
use omni_core::models::UserRole;
use omni_core::repository::DEFAULT_ADMIN_LOGIN;
use omni_core::secrets::keys;

use crate::auth::{
    build_session_cookie, internal_error, ApiError, LimiterIp, RequireAdmin, RequireAdminAllowDefault,
};
use crate::routes::validate_password;
use crate::state::AppState;

type JsonResult = Result<Json<serde_json::Value>, ApiError>;

/// The wizard modes, as stored in `setup_mode`.
const MODES: &[&str] = &["beginner", "intermediate", "it"];

/// A UNC path can hang for a long time on a dead server; the answer must not.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// At most one watchfolder probe thread alive.
static PROBE_SLOT: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)));

// ==========================================
// Step 1: the administrator account
// ==========================================

#[derive(Deserialize)]
pub struct WizardAccountPayload {
    full_name: String,
    login: String,
    password: String,
}

/// Replace the first-run `admin` / `Admin` account with a real administrator.
///
/// Order matters: the new account is created first and the default one is
/// deactivated only after that succeeded, so a failure anywhere before leaves
/// the default account live and the operator able to try again. The caller is
/// then signed in as the new administrator.
pub async fn api_wizard_account(
    RequireAdminAllowDefault(caller): RequireAdminAllowDefault,
    State(state): State<AppState>,
    LimiterIp(ip): LimiterIp,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    // Only the live default account may use this; an ordinary administrator
    // creates accounts through the users card.
    if !state.default_admin_locks(&caller) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "NOT_DEFAULT_ADMIN",
            "This step is only for the first-run account.",
        ));
    }
    // Parsed here, not by a `Json` extractor, so a caller who may not use this step is
    // refused with 403 whatever it sent.
    let payload: WizardAccountPayload =
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("Malformed request body."))?;
    let login = payload.login.trim().to_lowercase();
    let full_name = payload.full_name.trim().to_string();
    if full_name.is_empty() || full_name.chars().count() > 100 {
        return Err(ApiError::bad_request("Enter the administrator's full name (up to 100 characters)."));
    }
    if login.chars().count() < 3
        || login.chars().count() > 64
        || login.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ApiError::bad_request("The login must be 3 to 64 characters, without spaces."));
    }
    if login.eq_ignore_ascii_case(DEFAULT_ADMIN_LOGIN) {
        return Err(ApiError::bad_request("Choose a login other than \"admin\": that name is the first-run account."));
    }
    validate_password(&payload.password).map_err(ApiError::bad_request)?;
    let users = state.repo.list_users().map_err(internal_error("Could not read the accounts."))?;
    if users.iter().any(|u| u.email.eq_ignore_ascii_case(&login)) {
        return Err(ApiError::bad_request("That login is already in use."));
    }

    let lifetime = {
        let cfg = state.config.read().await;
        ChronoDuration::hours(cfg.security.session_hours_admin.max(1))
    };
    let token = generate_session_token();
    let ip_str = ip.map(|i| i.to_string());
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let default_id = caller.id;
    let caller_email = caller.email.clone();
    let (job_login, job_token) = (login.clone(), token.clone());
    let password = payload.password.clone();

    let outcome = state
        .argon(move |s| {
            let ops = RepoOps {
                repo: &s.repo,
                login: &job_login,
                password: &password,
                full_name: &full_name,
                token: &job_token,
                lifetime,
                ip: ip_str.as_deref(),
                user_agent: user_agent.as_deref(),
            };
            let r = swap_default_admin(&ops, default_id);
            if let Ok(o) = &r {
                let _ = s.repo.log_audit(
                    "WARN",
                    "ADMIN",
                    &format!(
                        "Administrator {job_login} created by the setup wizard (from {caller_email}); first-run account {DEFAULT_ADMIN_LOGIN} deactivated{}",
                        if matches!(o, SwapOutcome::SignedIn(_)) { "" } else { "; no session could be opened" }
                    ),
                );
            }
            // Whatever happened, the flag must follow the accounts as they now are.
            s.refresh_default_admin();
            r
        })
        .await
        .map_err(|msg| {
            tracing::error!(error = %msg, "The setup wizard could not replace the first-run account");
            ApiError::bad_request(msg)
        })?;

    let (new_id, signed_in) = match outcome {
        SwapOutcome::SignedIn(id) => (id, true),
        SwapOutcome::CreatedNotSignedIn(id) => (id, false),
    };
    if !signed_in {
        // The account exists and the first-run one is gone: sign in with the new login.
        return Ok(Json(serde_json::json!({
            "status": "ok", "user_id": new_id, "login": login, "signed_in": false,
            "message": "Your account was created; sign in with it."
        }))
        .into_response());
    }
    let cookie = build_session_cookie(&token, lifetime.num_seconds(), state.tls_enabled);
    let mut response = Json(serde_json::json!({ "status": "ok", "user_id": new_id, "login": login, "signed_in": true })).into_response();
    response.headers_mut().insert(SET_COOKIE, cookie);
    Ok(response)
}

/// The repository calls the account swap makes, so a test can make any one of them fail.
trait SwapOps {
    fn is_active(&self, user_id: i64) -> bool;
    fn create_admin(&self) -> anyhow::Result<i64>;
    fn set_active(&self, user_id: i64, active: bool) -> anyhow::Result<()>;
    fn end_sessions(&self, user_id: i64) -> anyhow::Result<()>;
    fn open_session(&self, user_id: i64) -> anyhow::Result<()>;
}

struct RepoOps<'a> {
    repo: &'a omni_core::repository::Repository,
    login: &'a str,
    password: &'a str,
    full_name: &'a str,
    token: &'a str,
    lifetime: ChronoDuration,
    ip: Option<&'a str>,
    user_agent: Option<&'a str>,
}

impl SwapOps for RepoOps<'_> {
    fn is_active(&self, user_id: i64) -> bool {
        self.repo.list_users().map(|u| u.iter().any(|x| x.id == user_id && x.is_active)).unwrap_or(false)
    }
    fn create_admin(&self) -> anyhow::Result<i64> {
        self.repo.create_user(self.login, self.password, UserRole::Admin, self.full_name, None)
    }
    fn set_active(&self, user_id: i64, active: bool) -> anyhow::Result<()> {
        self.repo.set_user_active_status(user_id, active)
    }
    fn end_sessions(&self, user_id: i64) -> anyhow::Result<()> {
        self.repo.delete_sessions_for_user(user_id).map(|_| ())
    }
    fn open_session(&self, user_id: i64) -> anyhow::Result<()> {
        self.repo.create_session_with_meta(user_id, self.token, self.lifetime, self.ip, self.user_agent)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SwapOutcome {
    /// New administrator created, first-run account off, a session opened.
    SignedIn(i64),
    /// New administrator created, first-run account off, but no session could be opened.
    CreatedNotSignedIn(i64),
}

/// One swap at a time: two concurrent requests must not each create an administrator.
static SWAP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Create the new administrator, then retire the first-run account.
///
/// At every point at least one working administrator exists. The default account is only
/// deactivated after the new one exists; if that deactivation fails the new account is
/// switched off and the default is switched back on. Ending the default's sessions is best
/// effort: a deactivated account has no usable session, because the session lookup requires
/// an active account. Errors are operator-facing text.
fn swap_default_admin(ops: &dyn SwapOps, default_id: i64) -> Result<SwapOutcome, String> {
    let _one_at_a_time = SWAP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Re-checked under the lock: a concurrent request may have finished the swap already.
    if !ops.is_active(default_id) {
        return Err("The first-run account was already replaced; reload the page and sign in.".into());
    }
    let new_id = ops.create_admin().map_err(|e| {
        tracing::error!(error = ?e, "Creating the wizard administrator failed");
        "Could not create the administrator account. The first-run account is unchanged; try again.".to_string()
    })?;
    if let Err(e) = ops.set_active(default_id, false) {
        tracing::error!(error = ?e, "Deactivating the first-run account failed");
        let _ = ops.set_active(new_id, false);
        // If the deactivation half-happened, put the way in back.
        let _ = ops.set_active(default_id, true);
        return Err("Could not replace the first-run account. It is unchanged; try again.".into());
    }
    if let Err(e) = ops.end_sessions(default_id) {
        tracing::warn!(error = ?e, "Could not end the first-run account's sessions; the account is off, so they no longer work");
    }
    Ok(match ops.open_session(new_id) {
        Ok(()) => SwapOutcome::SignedIn(new_id),
        Err(e) => {
            tracing::error!(error = ?e, "Opening the wizard administrator's session failed");
            SwapOutcome::CreatedNotSignedIn(new_id)
        }
    })
}

// ==========================================
// Reading the settings
// ==========================================

fn settings_json(cfg: &AppConfig) -> serde_json::Value {
    serde_json::json!({
        "setup_completed": cfg.setup_completed,
        "setup_mode": cfg.setup_mode,
        "watchfolder_path": cfg.watchfolder_path,
        // Where a relative watchfolder lands, so the wizard can test it as it stands.
        "watchfolder_resolved": cfg.resolve_path(&cfg.watchfolder_path).to_string_lossy(),
        "web_port": cfg.web_port,
        "mcr_open_networks": cfg.security.mcr_open_networks,
        "archive": { "enabled": cfg.archive.enabled, "retention_days": cfg.archive.retention_days },
        "tls": { "cert_path": cfg.tls.cert_path },
        "trust_proxy_header": cfg.security.trust_proxy_header,
        "trusted_proxies": cfg.security.trusted_proxies,
        "allowed_hosts": cfg.security.allowed_hosts,
        "metrics": { "enabled": cfg.metrics.enabled, "allow_networks": cfg.metrics.allow_networks },
        "graph": {
            "tenant_id": cfg.graph.tenant_id,
            "client_id": cfg.graph.client_id,
            "mailbox": cfg.graph.mailbox,
            // Whether, never what.
            "secret_set": !cfg.graph.client_secret.is_empty(),
        },
        "llm": { "mode": cfg.llm.mode, "provider": cfg.llm.provider, "endpoint": cfg.ollama_endpoint, "model": cfg.ollama_model },
    })
}

pub async fn api_admin_get_settings(RequireAdmin(_): RequireAdmin, State(state): State<AppState>) -> JsonResult {
    Ok(Json(settings_json(&*state.config.read().await)))
}

/// The chosen settings as a file for an unattended install. No secret is in it:
/// the mailbox secret and the LLM key are entered again on the new machine.
pub async fn api_admin_answer_file(RequireAdmin(_): RequireAdmin, State(state): State<AppState>) -> Response {
    let mut doc = settings_json(&*state.config.read().await);
    if let Some(graph) = doc.get_mut("graph").and_then(|g| g.as_object_mut()) {
        graph.remove("secret_set");
    }
    doc["answer_file_version"] = serde_json::json!(1);
    let mut response = Json(doc).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_static("attachment; filename=\"omni-answers.json\""),
    );
    response
}

// ==========================================
// Watchfolder
// ==========================================

#[derive(Deserialize)]
pub struct WatchfolderPayload {
    path: String,
}

/// The account this process runs as, for the operator to grant access to.
fn process_account() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    match (domain.is_empty(), user.is_empty()) {
        (_, true) => "the account the OmniIngest service runs as".to_string(),
        (true, false) => user,
        (false, false) => format!("{domain}\\{user}"),
    }
}

/// Create the folder if missing, then write, rename and remove one probe file,
/// in this process (so the answer is about the service's own account). The
/// probe name is unique and dot-prefixed; nothing else in the folder is touched.
fn probe_watchfolder_blocking(path: &Path) -> Result<(), String> {
    let account = process_account();
    let denied = |what: &str, e: &std::io::Error| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!(
                "Access denied while {what} {}. Grant {account} Modify permission on the share (Sharing tab) \
                 and on the folder (Security tab), then test again.",
                path.display()
            )
        } else {
            format!("Could not {what} {}: {e}", path.display())
        }
    };
    if !path.exists() {
        std::fs::create_dir_all(path).map_err(|e| denied("create", &e))?;
    }
    if !path.is_dir() {
        return Err(format!("{} is not a folder.", path.display()));
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stem = format!(".omni-probe-{}-{nonce}", std::process::id());
    let tmp = path.join(format!("{stem}.tmp"));
    let done = path.join(format!("{stem}.ok"));
    std::fs::write(&tmp, b"omni").map_err(|e| denied("write in", &e))?;
    if let Err(e) = std::fs::rename(&tmp, &done) {
        let _ = std::fs::remove_file(&tmp);
        return Err(denied("rename a file in", &e));
    }
    std::fs::remove_file(&done).map_err(|e| denied("remove a file from", &e))?;
    Ok(())
}

/// The trimmed path as the rules of `/api/setup` read it: judged after it is
/// resolved against the install folder, so the default `watchfolder` passes.
async fn judged_watchfolder(state: &AppState, raw: &str) -> Result<(String, std::path::PathBuf), String> {
    let watch = raw.trim().to_string();
    let resolved = if watch.is_empty() {
        std::path::PathBuf::new()
    } else {
        state.config.read().await.resolve_path(&watch)
    };
    match omni_core::paths::watchfolder_problem(&resolved.to_string_lossy()) {
        Some(problem) => Err(problem),
        None => Ok((watch, resolved)),
    }
}

/// `Ok(trimmed path)` or the operator-facing reason.
async fn test_watchfolder(state: &AppState, raw: &str) -> Result<String, String> {
    let (watch, resolved) = judged_watchfolder(state, raw).await?;
    let shown = resolved.clone();
    // One probe at a time: a probe on a dead share keeps its thread until the OS gives up, so
    // another one is refused instead of piling threads up. The permit travels with the thread.
    // A short wait covers a probe that is just finishing; a stuck one is not waited for.
    let permit = tokio::time::timeout(Duration::from_secs(2), PROBE_SLOT.clone().acquire_owned())
        .await
        .ok()
        .and_then(|p| p.ok())
        .ok_or_else(|| "A watchfolder test is still running (a slow share can take a while). Wait for it, then test again.".to_string())?;
    let probe = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        probe_watchfolder_blocking(&resolved)
    });
    match tokio::time::timeout(PROBE_TIMEOUT, probe).await {
        Err(_) => Err(format!(
            "{} did not answer within {} seconds. Check that the server is up and the share name is right.",
            shown.display(),
            PROBE_TIMEOUT.as_secs()
        )),
        Ok(Err(e)) => Err(format!("The folder test failed: {e}")),
        Ok(Ok(Err(msg))) => Err(msg),
        Ok(Ok(Ok(()))) => Ok(watch),
    }
}

pub async fn api_admin_test_watchfolder(
    RequireAdmin(_): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<WatchfolderPayload>,
) -> JsonResult {
    let account = process_account();
    Ok(Json(match test_watchfolder(&state, &payload.path).await {
        Ok(_) => serde_json::json!({
            "ok": true,
            "message": format!("OK: this service ({account}) can create, rename and delete files there."),
            "account": account,
        }),
        Err(message) => serde_json::json!({ "ok": false, "message": message, "account": account }),
    }))
}

/// Save only a path that passes the test just run again. Restart needed: the
/// delivery path, crash recovery and disk checks hold the path they started with.
pub async fn api_admin_save_watchfolder(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<WatchfolderPayload>,
) -> JsonResult {
    let watch = test_watchfolder(&state, &payload.path).await.map_err(ApiError::bad_request)?;
    let restart = {
        let mut cfg = state.config.write().await;
        let changed = cfg.watchfolder_path != watch;
        let mut next = cfg.clone();
        next.watchfolder_path = watch.clone();
        next.save_to_file(&state.config_path).map_err(internal_error("Could not save the configuration."))?;
        *cfg = next;
        changed
    };
    let _ = state.repo.log_audit(
        "WARN",
        "ADMIN",
        &format!("Watchfolder set to {watch} by {} (settings)", admin.email),
    );
    Ok(Json(serde_json::json!({ "status": "ok", "restart_required": restart })))
}

// ==========================================
// Mail (Microsoft 365)
// ==========================================

#[derive(Deserialize)]
pub struct MailPayload {
    #[serde(default)]
    tenant_id: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    mailbox: String,
    /// Blank keeps the stored secret.
    #[serde(default)]
    client_secret: Option<String>,
}

pub async fn api_admin_save_mail(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<MailPayload>,
) -> JsonResult {
    let (tenant, client, mailbox) = (
        payload.tenant_id.trim().to_string(),
        payload.client_id.trim().to_string(),
        payload.mailbox.trim().to_string(),
    );
    let any = !tenant.is_empty() || !client.is_empty() || !mailbox.is_empty();
    if any && (tenant.is_empty() || client.is_empty() || mailbox.is_empty()) {
        return Err(ApiError::bad_request("Fill in tenant, client id and mailbox, or leave all three blank to skip mail."));
    }
    if !mailbox.is_empty() && !mailbox.contains('@') {
        return Err(ApiError::bad_request("The mailbox must be an email address."));
    }
    let secret = payload.client_secret.as_deref().map(str::trim).unwrap_or("");
    let mut cfg = state.config.write().await;
    let mut next = cfg.clone();
    next.graph.tenant_id = tenant;
    next.graph.client_id = client;
    next.graph.mailbox = mailbox;
    // The secret first: if it cannot be stored nothing changes. Never in config.json.
    if !secret.is_empty() {
        state
            .secrets
            .set(keys::GRAPH_CLIENT_SECRET, secret)
            .map_err(internal_error("Could not store the client secret."))?;
        next.graph.client_secret = secret.to_string();
    }
    let changed = next.graph.tenant_id != cfg.graph.tenant_id
        || next.graph.client_id != cfg.graph.client_id
        || next.graph.mailbox != cfg.graph.mailbox
        || !secret.is_empty();
    next.save_to_file(&state.config_path).map_err(internal_error("Could not save the configuration."))?;
    let secret_set = !next.graph.client_secret.is_empty();
    *cfg = next;
    drop(cfg);
    let _ = state.repo.log_audit("WARN", "ADMIN", &format!("Mail settings changed by {} (settings)", admin.email));
    Ok(Json(serde_json::json!({
        "status": "ok",
        "secret_set": secret_set,
        "restart_required": changed,
    })))
}

// ==========================================
// Intermediate and IT settings
// ==========================================

#[derive(Deserialize)]
pub struct AdvancedPayload {
    web_port: Option<i64>,
    mcr_open_networks: Option<Vec<String>>,
    archive_enabled: Option<bool>,
    archive_retention_days: Option<i64>,
    /// Empty text turns TLS off.
    tls_cert_path: Option<String>,
    /// The `.pfx` passphrase; blank keeps the stored one. Never echoed.
    tls_password: Option<String>,
    trust_proxy_header: Option<bool>,
    trusted_proxies: Option<Vec<String>>,
    allowed_hosts: Option<Vec<String>>,
    metrics_enabled: Option<bool>,
    metrics_allow_networks: Option<Vec<String>>,
}

/// Warnings for trusted proxies wider than a /24 (IPv4) or /64 (IPv6): every client in such a
/// range is believed when it speaks for another address.
fn wide_proxy_warnings(entries: &[String]) -> Vec<String> {
    let Ok(set) = omni_core::net::CidrSet::parse(entries) else { return Vec::new() };
    set.blocks()
        .iter()
        .zip(entries)
        .filter(|(c, _)| c.prefix_len() < if c.network().is_ipv4() { 24 } else { 64 })
        .map(|(_, raw)| {
            format!(
                "Trusted proxy {raw} is a wide range: every machine in it may speak for any client address. Name only the proxy itself (for example x.x.x.x/32)."
            )
        })
        .collect()
}

/// Open `path` as a PKCS#12 bundle with `password`, off the runtime, as the listener will.
async fn check_pkcs12(path: std::path::PathBuf, password: String) -> Result<(), String> {
    let shown = path.display().to_string();
    let opened = tokio::task::spawn_blocking(move || {
        if !path.is_file() {
            return Err(format!(
                "The TLS certificate file {} does not exist. Enter the full path of the .pfx file.",
                path.display()
            ));
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("The TLS certificate {} cannot be read: {e}", path.display()))?;
        native_tls::Identity::from_pkcs12(&bytes, &password).map(|_| ()).map_err(|_| {
            format!(
                "{} does not open as a PKCS#12 certificate with that passphrase. It must be a .pfx/.p12 file                  holding the certificate and its private key (not a .cer or .pem), and the passphrase must be right.                  Nothing was saved.",
                path.display()
            )
        })
    })
    .await;
    match opened {
        Ok(r) => r,
        Err(_) => Err(format!("The TLS certificate {shown} could not be checked.")),
    }
}

/// Trim, drop blanks, and require every entry to parse as a CIDR.
fn clean_cidrs(what: &str, raw: &[String]) -> Result<Vec<String>, ApiError> {
    let list: Vec<String> = raw.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if let Err(bad) = omni_core::net::CidrSet::parse(&list) {
        let first = bad.first().map(|(entry, _)| entry.as_str()).unwrap_or("");
        return Err(ApiError::bad_request(format!(
            "{what}: \"{first}\" is not a network. Use the form 10.20.0.0/16 (or 10.20.0.5/32 for one machine)."
        )));
    }
    Ok(list)
}

pub async fn api_admin_save_advanced(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(p): Json<AdvancedPayload>,
) -> JsonResult {
    let new_tls_password = p.tls_password.as_deref().map(str::trim).unwrap_or("");
    // A certificate that does not open with its passphrase would stop the web listener at
    // the next start, and the daemon would then run with no panel: open it now, or refuse.
    // Done from a copy of the config, before the write lock: the file may be on a share
    // that hangs, and a held lock would stall every request.
    // The certificate path the check above was made against; another save between the
    // check and the write lock would pair this passphrase with a file it never opened.
    let mut tls_checked_against: Option<Option<String>> = None;
    if p.tls_cert_path.is_some() || !new_tls_password.is_empty() {
        let current = state.config.read().await.clone();
        tls_checked_against = Some(current.tls.cert_path.clone());
        let cert = match p.tls_cert_path.as_deref().map(str::trim) {
            Some("") => None,
            Some(c) => Some(c.to_string()),
            None => current.tls.cert_path.clone(),
        };
        if let Some(cert) = cert {
            let password = if new_tls_password.is_empty() {
                state.secrets.get_lossy(keys::TLS_PASSWORD).unwrap_or_default()
            } else {
                new_tls_password.to_string()
            };
            check_pkcs12(current.resolve_path(&cert), password).await.map_err(ApiError::bad_request)?;
        }
    }

    // The lock is taken only to apply and save.
    let mut cfg = state.config.write().await;
    if let Some(checked) = tls_checked_against {
        if cfg.tls.cert_path != checked {
            return Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "TLS_CHANGED",
                "The TLS settings were changed by someone else meanwhile; reload and try again.",
            ));
        }
    }
    let mut next = cfg.clone();
    // Restart-needing and live changes, counted separately.
    let mut restart = false;

    if let Some(port) = p.web_port {
        if !(1..=65535).contains(&port) {
            return Err(ApiError::bad_request("The web port must be between 1 and 65535."));
        }
        next.web_port = port as u16;
        restart |= next.web_port != cfg.web_port;
    }
    if let Some(list) = &p.mcr_open_networks {
        next.security.mcr_open_networks = clean_cidrs("MCR open networks", list)?;
    }
    if let Some(on) = p.archive_enabled {
        next.archive.enabled = on;
        restart |= on != cfg.archive.enabled;
    }
    if let Some(days) = p.archive_retention_days {
        if !(1..=3650).contains(&days) {
            return Err(ApiError::bad_request("Archive retention must be between 1 and 3650 days."));
        }
        next.archive.retention_days = days;
        restart |= days != cfg.archive.retention_days;
    }
    if let Some(cert) = &p.tls_cert_path {
        let cert = cert.trim();
        if cert.is_empty() {
            next.tls.cert_path = None;
        } else {
            next.tls.cert_path = Some(cert.to_string());
        }
        restart |= next.tls.cert_path != cfg.tls.cert_path;
    }
    if let Some(on) = p.trust_proxy_header {
        next.security.trust_proxy_header = on;
    }
    if let Some(list) = &p.trusted_proxies {
        next.security.trusted_proxies = clean_cidrs("Trusted proxies", list)?;
    }
    if let Some(list) = &p.allowed_hosts {
        let hosts: Vec<String> = list.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if let Some(bad) = hosts.iter().find(|h| h.contains("://") || h.contains('/') || h.contains(char::is_whitespace)) {
            return Err(ApiError::bad_request(format!(
                "Allowed hosts: \"{bad}\" must be a bare name such as newsroom-ingest.local, without http:// or a path."
            )));
        }
        restart |= hosts != cfg.security.allowed_hosts;
        next.security.allowed_hosts = hosts;
    }
    if let Some(on) = p.metrics_enabled {
        next.metrics.enabled = on;
        restart |= on != cfg.metrics.enabled;
    }
    if let Some(list) = &p.metrics_allow_networks {
        next.metrics.allow_networks = clean_cidrs("Metrics networks", list)?;
    }
    // A proxy header is believed only from a trusted proxy: trusting it with no
    // proxy named would do nothing, so say so rather than save a dead switch.
    if next.security.trust_proxy_header && next.security.trusted_proxies.is_empty() {
        return Err(ApiError::bad_request("Trusting the proxy header needs at least one trusted proxy network."));
    }
    if !new_tls_password.is_empty() {
        state
            .secrets
            .set(keys::TLS_PASSWORD, new_tls_password)
            .map_err(internal_error("Could not store the certificate passphrase."))?;
        restart = true;
    }
    next.save_to_file(&state.config_path).map_err(internal_error("Could not save the configuration."))?;
    let saved_proxies = next.security.trusted_proxies.clone();
    *cfg = next;
    drop(cfg);
    let _ = state.repo.log_audit("WARN", "ADMIN", &format!("Advanced settings changed by {} (settings)", admin.email));
    let warnings = wide_proxy_warnings(&saved_proxies);
    Ok(Json(serde_json::json!({ "status": "ok", "restart_required": restart, "warnings": warnings })))
}

// ==========================================
// Finish
// ==========================================

#[derive(Deserialize)]
pub struct CompletePayload {
    mode: String,
}

pub async fn api_admin_complete_setup(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(p): Json<CompletePayload>,
) -> JsonResult {
    let mode = p.mode.trim().to_lowercase();
    if !MODES.contains(&mode.as_str()) {
        return Err(ApiError::bad_request("The mode must be beginner, intermediate or it."));
    }
    let mut cfg = state.config.write().await;
    let mut next = cfg.clone();
    next.setup_completed = true;
    next.setup_mode = mode.clone();
    next.save_to_file(&state.config_path).map_err(internal_error("Could not save the configuration."))?;
    *cfg = next;
    drop(cfg);
    let _ = state.repo.log_audit("WARN", "ADMIN", &format!("Setup wizard completed ({mode}) by {}", admin.email));
    Ok(Json(serde_json::json!({ "status": "ok", "restart_required": false })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_lists_are_trimmed_and_checked() {
        let ok = clean_cidrs("x", &[" 10.0.0.0/8 ".into(), "".into()]).unwrap();
        assert_eq!(ok, vec!["10.0.0.0/8"]);
        assert!(clean_cidrs("x", &["banana".into()]).is_err());
    }

    use std::cell::RefCell;

    /// The real repository with one step made to fail, to prove an administrator always remains.
    struct Flaky<'a> {
        inner: RepoOps<'a>,
        fail: &'static str,
        log: RefCell<Vec<String>>,
    }

    impl SwapOps for Flaky<'_> {
        fn is_active(&self, id: i64) -> bool {
            self.inner.is_active(id)
        }
        fn create_admin(&self) -> anyhow::Result<i64> {
            if self.fail == "create" { anyhow::bail!("injected") }
            self.inner.create_admin()
        }
        fn set_active(&self, id: i64, active: bool) -> anyhow::Result<()> {
            self.log.borrow_mut().push(format!("active({id},{active})"));
            // Only the first deactivation of the default fails; the rollback must still work.
            if self.fail == "deactivate" && !active && self.log.borrow().len() == 1 {
                // The worst case: the write lands and the call still reports failure.
                self.inner.set_active(id, active)?;
                anyhow::bail!("injected")
            }
            self.inner.set_active(id, active)
        }
        fn end_sessions(&self, id: i64) -> anyhow::Result<()> {
            if self.fail == "end_sessions" { anyhow::bail!("injected") }
            self.inner.end_sessions(id)
        }
        fn open_session(&self, id: i64) -> anyhow::Result<()> {
            if self.fail == "open_session" { anyhow::bail!("injected") }
            self.inner.open_session(id)
        }
    }

    /// Run the swap with a step failing; return the outcome, the admins that can sign in, and the log.
    fn swap_with(fail: &'static str) -> (Result<SwapOutcome, String>, Vec<String>, Vec<String>) {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = omni_core::repository::Repository::new(dir.path().join("omni.db")).unwrap();
        let default_id = repo.create_user("admin", "Admin", UserRole::Admin, "First-run", None).unwrap();
        let ops = Flaky {
            inner: RepoOps {
                repo: &repo, login: "new.admin", password: "a-long-passphrase-1", full_name: "N",
                token: "tok", lifetime: ChronoDuration::hours(1), ip: None, user_agent: None,
            },
            fail,
            log: RefCell::new(Vec::new()),
        };
        let r = swap_default_admin(&ops, default_id);
        let working: Vec<String> = repo
            .list_users()
            .unwrap()
            .into_iter()
            .filter(|u| u.is_active && u.role == UserRole::Admin)
            .map(|u| u.email)
            .collect();
        (r, working, ops.log.into_inner())
    }

    #[test]
    fn an_administrator_can_sign_in_after_a_failure_at_every_step() {
        let (r, working, _) = swap_with("none");
        assert!(matches!(r, Ok(SwapOutcome::SignedIn(_))));
        assert_eq!(working, vec!["new.admin"]);

        // Create fails: the default is untouched.
        let (r, working, _) = swap_with("create");
        assert!(r.is_err());
        assert_eq!(working, vec!["admin"]);

        // Deactivating the default fails: the new one is off again and the default is on.
        let (r, working, log) = swap_with("deactivate");
        assert!(r.unwrap_err().contains("unchanged"));
        assert_eq!(working, vec!["admin"], "{log:?}");

        // Ending sessions fails: best effort, the swap still completes with the new admin.
        let (r, working, _) = swap_with("end_sessions");
        assert!(matches!(r, Ok(SwapOutcome::SignedIn(_))));
        assert_eq!(working, vec!["new.admin"]);

        // Opening the session fails: the account exists, one admin works.
        let (r, working, _) = swap_with("open_session");
        assert!(matches!(r, Ok(SwapOutcome::CreatedNotSignedIn(_))));
        assert_eq!(working, vec!["new.admin"]);
    }

    #[test]
    fn a_second_swap_finds_the_default_already_replaced() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = omni_core::repository::Repository::new(dir.path().join("omni.db")).unwrap();
        let default_id = repo.create_user("admin", "Admin", UserRole::Admin, "First-run", None).unwrap();
        let mk = |login: &'static str| Flaky {
            inner: RepoOps {
                repo: &repo, login, password: "a-long-passphrase-1", full_name: "N",
                token: login, lifetime: ChronoDuration::hours(1), ip: None, user_agent: None,
            },
            fail: "none",
            log: RefCell::new(Vec::new()),
        };
        assert!(swap_default_admin(&mk("one.admin"), default_id).is_ok());
        assert!(swap_default_admin(&mk("two.admin"), default_id).is_err());
        assert!(repo.get_user_by_email("two.admin").unwrap().is_none());
    }

    #[test]
    fn proxies_wider_than_a_24_or_a_64_get_a_warning() {
        let w = wide_proxy_warnings(&["10.0.0.0/8".into(), "10.1.1.0/24".into(), "10.1.1.5/32".into(), "fd00::/48".into(), "fd00::/64".into()]);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("10.0.0.0/8") && w[1].contains("fd00::/48"));
        assert!(wide_proxy_warnings(&["10.1.1.5/32".into()]).is_empty());
    }

    #[tokio::test]
    async fn a_second_watchfolder_test_is_refused_while_one_is_still_running() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = omni_core::repository::Repository::new(dir.path().join("omni.db")).unwrap();
        let state = AppState::new(repo, AppConfig::default(), dir.path().join("config.json"));
        let target = dir.path().join("wf");
        let held = PROBE_SLOT.clone().try_acquire_owned().unwrap();
        let busy = test_watchfolder(&state, &target.to_string_lossy()).await;
        assert!(busy.unwrap_err().contains("still running"));
        assert!(!target.exists(), "no probe may start while another is alive");
        drop(held);
        assert!(test_watchfolder(&state, &target.to_string_lossy()).await.is_ok());
    }
}
