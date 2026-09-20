//! Front-end guarantees, enforced as tests (plan P2.3/W-03, P2.5/W-08).
//!
//! Both defects are the kind that come back. `innerHTML = \`…${x}\`` is the
//! obvious way to build a row, and a CDN `<script>` is the obvious way to get
//! an icon set, so a reviewer noticing them once is not a fix. These tests
//! read the shipped assets and fail the build instead.

use std::collections::BTreeMap;

use anyhow::Result;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::net::SocketAddr;
use tempfile::TempDir;
use tower::ServiceExt;

use omni_core::config::AppConfig;
use omni_core::models::{JobStatus, UserRole};
use omni_core::repository::Repository;
use omni_web::assets::WebAssets;
use omni_web::server::WebServer;
use omni_web::state::AppState;

/// A comment line in HTML, CSS or JS.
///
/// These checks read source text rather than parse it, so the prose that
/// explains *why* a thing is banned would otherwise trip the ban.
fn is_comment(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("//")
        || t.starts_with('*')
        || t.starts_with("/*")
        || t.starts_with("<!--")
        || t.starts_with("--")
}

/// Every shipped asset, as `path -> contents`.
fn all_assets() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for path in WebAssets::iter() {
        let path = path.to_string();
        if let Some(file) = WebAssets::get(&path) {
            out.insert(path, String::from_utf8_lossy(&file.data).into_owned());
        }
    }
    out
}

// ==========================================
// W-03 — stored XSS
// ==========================================

#[test]
fn no_asset_writes_html_from_a_string() {
    // `innerHTML`, `outerHTML`, `document.write` and `insertAdjacentHTML` all
    // parse their argument as markup. Job URLs and error messages arrive from
    // email, so any of them is a path from "someone mailed the newsroom" to
    // "script ran in an operator's browser".
    const BANNED: &[&str] = &[
        "innerHTML",
        "outerHTML",
        "document.write",
        "insertAdjacentHTML",
        // `new Function` and `eval` are the same problem one level up.
        "eval(",
    ];

    let mut offences = Vec::new();
    for (path, body) in all_assets() {
        if !(path.ends_with(".html") || path.ends_with(".js")) {
            continue;
        }
        for (line_no, line) in body.lines().enumerate() {
            if is_comment(line) {
                continue;
            }
            for banned in BANNED {
                if line.contains(banned) {
                    offences.push(format!("{path}:{} contains `{banned}`", line_no + 1));
                }
            }
        }
    }

    assert!(
        offences.is_empty(),
        "build DOM nodes with `el()`/`textContent` instead:\n  {}",
        offences.join("\n  ")
    );
}

#[test]
fn no_asset_carries_inline_event_handlers_or_inline_script() {
    // The CSP is `script-src 'self'` with no `'unsafe-inline'`, so an inline
    // handler would not run anyway — it would just silently stop working. And
    // `onclick="…${value}"` is the string-built-markup problem wearing a hat.
    let mut offences = Vec::new();
    for (path, body) in all_assets() {
        if !path.ends_with(".html") {
            continue;
        }
        for (line_no, line) in body.lines().enumerate() {
            for attr in ["onclick=", "onsubmit=", "onchange=", "oninput=", "onload=", "onerror="] {
                if line.contains(attr) {
                    offences.push(format!("{path}:{} has inline `{attr}`", line_no + 1));
                }
            }
            // An inline `<script>` body (as opposed to `<script src=...>`).
            let trimmed = line.trim();
            if trimmed.starts_with("<script") && !trimmed.contains("src=") {
                offences.push(format!("{path}:{} has an inline <script>", line_no + 1));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "attach listeners in the module instead:\n  {}",
        offences.join("\n  ")
    );
}

#[tokio::test]
async fn a_hostile_job_url_is_returned_as_data_and_never_as_markup() -> Result<()> {
    // The attack: a journalist's automated mail, or anyone who can write to
    // the ingest address, includes this as a link. It reaches the queue, and
    // the MCR panel renders it on the operator's screen.
    const PAYLOAD: &str = "https://evil.example/\"><img src=x onerror=alert(1)>";

    let dir = TempDir::new()?;
    let repo = Repository::new(dir.path().join("omni.db"))?;
    let token = omni_core::auth::generate_session_token();
    let mcr = repo.create_user(
        "desk@station.gr",
        "correct-horse-battery",
        UserRole::OpenMcr,
        "MCR Desk",
        None,
    )?;
    repo.create_session(mcr, &token, 1)?;

    // Straight into the database: this models a job that arrived by email,
    // which is not subject to the submission API's URL check.
    repo.add_job(
        PAYLOAD,
        "1_MCR_XSS",
        "MCR",
        "XSS",
        "1",
        0,
        JobStatus::RequiresReview,
        None,
        Some("<script>alert('notes')</script>"),
        None,
    )?;
    repo.update_job_status(
        1,
        JobStatus::RequiresReview,
        Some("<img src=x onerror=alert('err')>"),
        None,
        None,
    )?;

    let state = AppState::new(repo.clone(), AppConfig::default(), dir.path().join("cfg.json"));
    let router = WebServer::build_router(state);

    let res = router
        .clone()
        .layer(MockConnectInfo("127.0.0.1:1".parse::<SocketAddr>()?))
        .oneshot(
            Request::builder()
                .uri("/api/jobs")
                .header("cookie", format!("omni_session={token}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);

    let content_type = res
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()?
        .to_string();
    // JSON, not HTML: a browser never parses this response as markup.
    assert!(content_type.starts_with("application/json"), "{content_type}");

    let bytes = res.into_body().collect().await?.to_bytes();
    let body = String::from_utf8_lossy(&bytes).into_owned();

    // The payload round-trips intact (the operator must see the real URL to
    // judge it) but as a JSON string value, with the quote escaped.
    let json: serde_json::Value = serde_json::from_slice(&bytes)?;
    let job = &json["jobs"][0];
    assert_eq!(job["url"].as_str().unwrap(), PAYLOAD);
    assert!(
        body.contains(r#"\"><img"#),
        "the quote must be escaped in the JSON encoding: {body}"
    );

    // And the served HTML does not contain it at all -- the panels are static
    // documents that fetch their data, so there is no server-side template to
    // inject into in the first place.
    let page = router
        .clone()
        .layer(MockConnectInfo("127.0.0.1:1".parse::<SocketAddr>()?))
        .oneshot(
            Request::builder()
                .uri("/mcr")
                .header("cookie", format!("omni_session={token}"))
                .body(Body::empty())?,
        )
        .await?;
    let page_body = page.into_body().collect().await?.to_bytes();
    let page_text = String::from_utf8_lossy(&page_body);
    assert!(!page_text.contains("onerror"), "the page must not embed job data");

    Ok(())
}

// ==========================================
// W-08 — offline panels
// ==========================================

#[test]
fn no_asset_references_an_external_origin() {
    // MCR workstations sit on a restricted VLAN. With the CDN tags, the panels
    // rendered as unstyled text at exactly the moment they were needed -- and
    // a CDN script is an unsigned third party with full DOM access on a
    // machine that can write to the playout share.
    // A URL in a `placeholder` or a default form value is text the operator
    // reads, not something the browser fetches. What matters is a *load*: an
    // element source, a stylesheet link, a CSS import, or a module import.
    const LOADERS: &[&str] = &["src=", "href=", "@import", "url(", "from '", "from \"", "import("];
    // These never appear for a legitimate reason, wherever they are.
    const NEVER: &[&str] = &["cdn.tailwindcss", "unpkg.com", "fonts.googleapis", "fonts.gstatic"];

    let mut offences = Vec::new();
    for (path, body) in all_assets() {
        for (line_no, line) in body.lines().enumerate() {
            // Comments and doc text may name a URL; only live references count.
            if is_comment(line) {
                continue;
            }
            let trimmed = line.trim();

            for marker in NEVER {
                if line.contains(marker) {
                    offences.push(format!("{path}:{}: {}", line_no + 1, trimmed));
                }
            }

            let absolute = line.contains("http://") || line.contains("https://");
            let loads = LOADERS.iter().any(|m| line.contains(m));
            if absolute && loads {
                // The SVG namespace is an identifier, not a fetch.
                if line.contains("www.w3.org") {
                    continue;
                }
                // A link to a job's own URL is the operator following the
                // story; it is opened on click, not loaded with the page.
                if line.contains("noopener") {
                    continue;
                }
                offences.push(format!("{path}:{}: {}", line_no + 1, trimmed));
            }
        }
    }

    assert!(
        offences.is_empty(),
        "panels must load nothing from outside this server:\n  {}",
        offences.join("\n  ")
    );
}

#[test]
fn every_asset_referenced_by_a_page_is_actually_shipped() {
    // A stylesheet that 404s is a panel that renders as unstyled text, and it
    // would otherwise only be noticed by opening each page by hand.
    let assets = all_assets();
    let mut missing = Vec::new();

    for (path, body) in &assets {
        if !path.ends_with(".html") && !path.ends_with(".js") {
            continue;
        }
        for reference in body.split(['"', '\'']) {
            let Some(rest) = reference.strip_prefix("/static/") else {
                continue;
            };
            let file = rest.split(['?', '#']).next().unwrap_or(rest);
            if file.is_empty() {
                continue;
            }
            let key = format!("static/{file}");
            if !assets.contains_key(&key) {
                missing.push(format!("{path} references /static/{file}, which is not shipped"));
            }
        }
    }

    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

#[tokio::test]
async fn static_assets_are_served_and_cacheable() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = Repository::new(dir.path().join("omni.db"))?;
    let state = AppState::new(repo, AppConfig::default(), dir.path().join("cfg.json"));
    let router = WebServer::build_router(state);

    for (path, mime) in [
        ("/static/app.css", "text/css"),
        ("/static/app.js", "javascript"),
        ("/static/icons.svg", "image/svg"),
    ] {
        let res = router
            .clone()
            .layer(MockConnectInfo("127.0.0.1:1".parse::<SocketAddr>()?))
            .oneshot(Request::builder().uri(path).body(Body::empty())?)
            .await?;
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        let content_type = res.headers().get("content-type").unwrap().to_str()?;
        assert!(content_type.contains(mime), "{path} served as {content_type}");
        assert!(res
            .headers()
            .get("cache-control")
            .unwrap()
            .to_str()?
            .contains("max-age"));
    }

    // Traversal is refused rather than relied upon to miss.
    let res = router
        .clone()
        .layer(MockConnectInfo("127.0.0.1:1".parse::<SocketAddr>()?))
        .oneshot(
            Request::builder()
                .uri("/static/..%2f..%2fmcr.html")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    Ok(())
}
