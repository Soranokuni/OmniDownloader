//! The admin's toolchain card routes (plan P9.3): the listing, the install
//! start, and the one-at-a-time rule. The download is a fake that waits on
//! command; no network and no tools in `bin/`. Who may call is in
//! `auth_matrix_tests.rs`.

use anyhow::Result;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;
use tower::ServiceExt;

use omni_core::config::AppConfig;
use omni_core::dependencies::{FetchFuture, Fetcher};
use omni_core::models::UserRole;
use omni_core::repository::Repository;
use omni_web::server::WebServer;
use omni_web::state::AppState;

const PEER: &str = "127.0.0.1:50000";

/// Says it started, waits to be released, then delivers nothing.
struct HeldFetcher {
    started: Notify,
    release: Notify,
}

impl Fetcher for HeldFetcher {
    fn fetch<'a>(&'a self, _url: &'a str, _sink: &'a mut (dyn FnMut(&[u8]) -> Result<()> + Send)) -> FetchFuture<'a> {
        Box::pin(async move {
            self.started.notify_one();
            self.release.notified().await;
            Ok(())
        })
    }
}

struct App {
    router: axum::Router,
    admin: String,
    bin: std::path::PathBuf,
    fetcher: Arc<HeldFetcher>,
    _dir: TempDir,
}

impl App {
    fn new() -> Result<Self> {
        let dir = TempDir::new()?;
        let repo = Repository::new(dir.path().join("omni.db"))?;
        let id = repo.create_user("it@station.gr", "correct-horse-battery", UserRole::Admin, "Administrator", None)?;
        let admin = omni_core::auth::generate_session_token();
        repo.create_session(id, &admin, 1)?;
        let bin = dir.path().join("bin");
        let fetcher = Arc::new(HeldFetcher { started: Notify::new(), release: Notify::new() });
        let state = AppState::new(repo, AppConfig::default(), dir.path().join("config.json"))
            .with_tool_dirs(bin.clone(), vec![])
            .with_tool_fetcher(fetcher.clone());
        Ok(Self { router: WebServer::build_router(state), admin, bin, fetcher, _dir: dir })
    }

    async fn send(&self, method: &str, uri: &str, marker: bool) -> Result<(StatusCode, Value)> {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", format!("omni_session={}", self.admin))
            .header("host", "mcr.local")
            .header("origin", "http://mcr.local");
        if marker {
            b = b.header("x-omni-request", "1");
        }
        let res = self.router.clone().layer(MockConnectInfo(PEER.parse::<SocketAddr>().unwrap())).oneshot(b.body(Body::empty())?).await?;
        let status = res.status();
        let bytes = res.into_body().collect().await?.to_bytes();
        Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
    }
}

fn row<'a>(listing: &'a Value, name: &str) -> &'a Value {
    listing["tools"].as_array().unwrap().iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("no row for {name}"))
}

#[tokio::test]
async fn the_listing_says_what_each_tool_needs() -> Result<()> {
    let app = App::new()?;
    // Present but not the pinned build, and not something that runs.
    std::fs::create_dir_all(&app.bin)?;
    std::fs::write(app.bin.join("ffmpeg.exe"), b"not a program")?;

    let (status, listing) = app.send("GET", "/api/admin/tools", true).await?;
    assert_eq!(status, StatusCode::OK);

    let ffmpeg = row(&listing, "ffmpeg");
    assert_eq!(ffmpeg["installed"], true);
    assert_eq!(ffmpeg["matches_pin"], false);
    // The literal per architecture, so a pin edit or the wrong list shows up here.
    let want = match omni_core::toolpins::host_arch() {
        omni_core::toolpins::Arch::X64 => "9.0.2",
        omni_core::toolpins::Arch::Arm64 => "9.0.2-39-g7adfbe40dc",
    };
    assert_eq!(ffmpeg["pinned_version"], want);
    assert_eq!(ffmpeg["action"], "repair");
    assert!(ffmpeg["version"].is_null());

    let ffprobe = row(&listing, "ffprobe");
    assert_eq!((ffprobe["installed"].clone(), ffprobe["action"].clone()), (Value::Bool(false), Value::from("install")));

    assert_eq!(row(&listing, "bmxtranswrap")["pinned_version"], "1.6.0");
    for name in ["yt-dlp", "deno"] {
        let r = row(&listing, name);
        assert!(r["pinned_version"].is_null() && r["matches_pin"].is_null(), "{name} is not pinned");
        assert_eq!(r["action"], "install");
    }
    assert!(listing["running_tool"].is_null());
    Ok(())
}

#[tokio::test]
async fn an_install_needs_the_panel_marker_and_a_known_tool() -> Result<()> {
    let app = App::new()?;
    let (status, _) = app.send("POST", "/api/admin/tools/ffmpeg/install", false).await?;
    assert_eq!(status, StatusCode::FORBIDDEN, "no CSRF marker");
    let (status, _) = app.send("POST", "/api/admin/tools/rm-rf/install", true).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, listing) = app.send("GET", "/api/admin/tools", true).await?;
    assert!(listing["running_tool"].is_null(), "neither request may start an install");
    Ok(())
}

#[tokio::test]
async fn a_second_install_is_refused_while_one_runs_and_the_slot_frees_after() -> Result<()> {
    let app = App::new()?;

    let (status, body) = app.send("POST", "/api/admin/tools/ffmpeg/install", true).await?;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["tool"], "ffmpeg");
    tokio::time::timeout(Duration::from_secs(10), app.fetcher.started.notified()).await?;

    // ffprobe comes with ffmpeg's archive, so it is the same install.
    for other in ["bmxtranswrap", "ffprobe", "deno"] {
        let (status, body) = app.send("POST", &format!("/api/admin/tools/{other}/install"), true).await?;
        assert_eq!(status, StatusCode::CONFLICT, "{other}");
        assert!(body["error"].to_string().contains("ffmpeg") || body.to_string().contains("ffmpeg"), "{body}");
    }

    let (_, listing) = app.send("GET", "/api/admin/tools", true).await?;
    assert_eq!(listing["running_tool"], "ffmpeg");
    assert_eq!(row(&listing, "ffmpeg")["running"]["stage"], "downloading");
    assert_eq!(row(&listing, "ffprobe")["running"]["stage"], "downloading");
    assert!(row(&listing, "bmxtranswrap")["running"].is_null());

    // The fake delivers nothing: the install fails on the size check and says so.
    app.fetcher.release.notify_one();
    let mut listing = Value::Null;
    for _ in 0..100 {
        listing = app.send("GET", "/api/admin/tools", true).await?.1;
        if listing["running_tool"].is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(listing["running_tool"].is_null(), "the install never ended");
    let last = &row(&listing, "ffmpeg")["last_result"];
    assert_eq!(last["ok"], false);
    assert!(last["message"].as_str().unwrap().contains("pinned build is"), "{last}");
    assert!(!app.bin.join("ffmpeg.staged").exists(), "the staged folder is removed");
    Ok(())
}

#[tokio::test]
async fn a_yt_dlp_or_deno_swap_waits_for_the_canary_and_for_other_swappers() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = Repository::new(dir.path().join("omni.db"))?;
    let canary = Arc::new(tokio::sync::Mutex::new(()));
    let state = AppState::new(repo, AppConfig::default(), dir.path().join("config.json")).with_canary_lock(canary.clone());

    // A canary run holds its lock: the swap does not start.
    let canary_guard = canary.clone().lock_owned().await;
    let s = state.clone();
    let first = tokio::spawn(async move { omni_web::admin_tools::lock_swaps(&s).await });
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(!first.is_finished(), "the swap ran under a canary run");
    drop(canary_guard);
    let mut first = first.await?;
    assert!(first.swap.drain(Duration::from_secs(1)).await);
    assert!(state.update_gate.is_paused());

    // A second swapper waits for the first, and its release does not reopen the gate under it.
    let s = state.clone();
    let second = tokio::spawn(async move { omni_web::admin_tools::lock_swaps(&s).await });
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(!second.is_finished(), "two swappers at once");
    drop(first);
    let mut second = second.await?;
    assert!(second.swap.drain(Duration::from_secs(1)).await);
    assert!(state.update_gate.is_paused());
    drop(second);
    assert!(!state.update_gate.is_paused(), "the last release reopens the gate");
    assert!(canary.try_lock().is_ok(), "the canary lock is released too");
    Ok(())
}
