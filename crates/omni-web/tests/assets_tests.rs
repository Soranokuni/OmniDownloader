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

/// Regression (2026-09-30): the shared `api()` helper answered every 401
/// with "Session expired." before reading the response, so a sign-in with an
/// address that has no account said "Session expired." — and sent the
/// operator looking for a session problem. On the login page the server's
/// own message must be shown.
#[test]
fn a_failed_sign_in_shows_the_servers_message_not_session_expired() {
    let assets = all_assets();
    let app = assets.get("static/app.js").expect("app.js shipped");
    let code: Vec<&str> = app.lines().filter(|l| !is_comment(l)).collect();
    let body_read = code.iter().position(|l| l.contains("await response.text()")).expect("body read");
    let unauthorized = code.iter().position(|l| l.contains("response.status === 401")).expect("401 branch");
    assert!(body_read < unauthorized, "the 401 branch runs before the response body is read");
    let branch = code[unauthorized..].iter().take(12).copied().collect::<Vec<_>>().join("\n");
    assert!(branch.contains("serverMessage"), "the 401 branch ignores the server's message:\n{branch}");

    // And every page imports the same app.js: two versions of one module
    // would load twice, with two copies of its state.
    let versions: std::collections::BTreeSet<&str> = assets
        .values()
        .flat_map(|s| s.match_indices("/static/app.js?v=").map(move |(i, _)| &s[i..i + 19]))
        .collect();
    assert_eq!(versions.len(), 1, "pages import different app.js versions: {versions:?}");
}

/// P1.10: the desk's live job events. One URL per ES module (two versions of a
/// module load twice), and the desk no longer refetches everything per event.
#[test]
fn desk_modules_have_one_url_each_and_mcr_handles_job_events_in_place() {
    let assets = all_assets();
    for (module, version) in [("desk.js", 14), ("inbox.js", 20), ("approval.js", 9), ("notify.js", 11), ("mcr.js", 46)] {
        let needle = format!("/static/{module}?v=");
        let mut found = std::collections::BTreeSet::new();
        for s in assets.values() {
            for (i, _) in s.match_indices(needle.as_str()) {
                let rest = &s[i + needle.len()..];
                let n = rest.chars().take_while(|c| c.is_ascii_digit()).count();
                found.insert(rest[..n].to_string());
            }
        }
        assert_eq!(found, [version.to_string()].into(), "{module} is imported under different versions");
    }
    let mcr = assets.get("static/mcr.js").expect("mcr.js shipped");
    assert!(mcr.contains("applyJobEvent"), "mcr.js does not apply job events in place");
    assert!(mcr.contains("new EventSource('/api/events')"), "mcr.js opens no stream of its own");
    assert!(!mcr.contains("live(() =>"), "mcr.js still refetches everything per event");
    // Events are an accelerator: the 5 s poll must not depend on the stream, or new, requeued
    // and reaped jobs and health changes (which send no event) would lag.
    assert!(mcr.contains("setInterval(reloadAll, 5000)"), "the unconditional 5 s poll is gone");
    assert!(!mcr.contains("readyState"), "the poll is gated on the stream state");
    // The module ends with the top-level poll and nothing after it: a leftover
    // `}, 5000);` from an old wrapper is a SyntaxError that kills the whole desk.
    let tail: Vec<&str> = mcr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    assert_eq!(
        tail[tail.len() - 2..],
        ["setInterval(reloadAll, 5000);", "reloadAll();"],
        "mcr.js must end with the unconditional poll"
    );
}

/// One app.css URL across every page, so a bump cannot miss one.
#[test]
fn every_page_links_the_same_app_css_version() {
    let versions: std::collections::BTreeSet<String> = all_assets()
        .values()
        .flat_map(|s| {
            s.match_indices("/static/app.css?v=").map(move |(i, m)| {
                let rest = &s[i + m.len()..];
                rest.chars().take_while(|c| c.is_ascii_digit()).collect::<String>()
            })
        })
        .collect();
    assert_eq!(versions.len(), 1, "pages link different app.css versions: {versions:?}");
}

/// P11.3b: the late strip is loaded on every refresh and reconciled by job id, the chime reads
/// `queue.late` (and is not part of the needs-a-person count), and a video that waits for the
/// watchfolder says so.
#[test]
fn the_late_strip_is_loaded_every_cycle_and_reconciled_in_place() {
    let assets = all_assets();
    let mcr = assets.get("static/mcr.js").expect("mcr.js shipped");
    assert!(mcr.contains("'/api/jobs?view=late'"), "mcr.js does not load the late list");
    assert!(
        mcr.contains("const reloadAll = () => { refresh(); loadStatus(); loadLate(); };"),
        "the late strip is not part of the 5 s cycle on every tab"
    );
    assert!(mcr.contains("syncKeyedList(lateList,"), "the late strip is not reconciled by job id");
    assert!(mcr.contains("/now`, { method: 'POST' }"), "no «Τώρα» action");
    // P11.7: «Τώρα» for a back-off and for a stream still live waiting for its next check.
    let row = mcr.split("function lateRow(job)").nth(1).and_then(|s| s.split("\n}").next()).expect("mcr.js has lateRow");
    let button = row.split("el('button'").next().unwrap();
    assert!(
        button.contains("job.wait_reason === 'retry' || job.wait_reason === 'live'"),
        "the late strip does not offer «Τώρα» for both 'retry' and 'live':\n{row}"
    );
    assert!(row.contains("runNow(job, ev.currentTarget)"), "the late row's button does not run the job now");
    assert!(mcr.contains("και άλλα"), "the strip does not cap its rows");
    let page = assets.get("mcr.html").expect("mcr.html shipped");
    let strip = page.find("id=\"late-strip\"").expect("the strip is in mcr.html");
    assert!(strip < page.find("class=\"tabs\"").unwrap(), "the strip is not above the tab bar");

    let notify = assets.get("static/notify.js").expect("notify.js shipped");
    assert!(notify.contains("workers: {"), "CHECK_INFO has no entry for the workers check");
    assert!(notify.contains("queue.late"), "notify.js does not read queue.late");
    let need = notify.split("function needCount").nth(1).and_then(|s| s.split('}').next()).unwrap();
    assert!(!need.contains("late"), "late videos must not count as needing a person");

    let desk = assets.get("static/desk.js").expect("desk.js shipped");
    assert!(desk.contains("Περιμένει τον φάκελο παράδοσης"), "desk.js has no watchfolder wait text");
    assert!(desk.contains("export function lateReason"), "desk.js has no lateReason");

    // P11.7: a stream still live says so, with its next check's time, in the late strip.
    let late = desk.split("export function lateReason").nth(1).and_then(|s| s.split("\nexport function").next()).unwrap();
    let live = late.split("case 'live':").nth(1).expect("lateReason has no case for wait_reason 'live'");
    let live = live.split("case '").next().unwrap();
    assert!(live.contains("Ζωντανή μετάδοση"), "the live case does not say «Ζωντανή μετάδοση»:\n{live}");
    assert!(live.contains("επόμενος έλεγχος ${at}") && live.contains("clock(job.next_try_at)"), "the live case does not show the next check's time:\n{live}");
}

/// P12.3: the desk's lists are updated in place. A full `render(list, every card)` on the 5 s
/// refresh throws away an operator's typed address, the focus and the open panels.
#[test]
fn mcr_lists_are_reconciled_not_rebuilt_every_refresh() {
    let assets = all_assets();
    let mcr = assets.get("static/mcr.js").expect("mcr.js shipped");
    for old in [
        "render('review-cards', withDayHeadings",
        "render('queue-cards', cards)",
        "render('completed-body', jobs.map(completedRow))",
    ] {
        assert!(!mcr.contains(old), "mcr.js rebuilds a whole list again: {old}");
    }
    for new in [
        "syncKeyedList(reviewList,",
        "syncKeyedList(queueList,",
        "syncKeyedList(completedList,",
        "carryControls(",
    ] {
        assert!(mcr.contains(new), "mcr.js no longer reconciles its lists: {new}");
    }
}

#[test]
fn the_user_panel_posts_to_its_own_endpoint_and_is_imported_at_v7() {
    let assets = all_assets();
    let page = assets.get("user.html").expect("user.html shipped");
    assert!(page.contains("/static/user.js?v=7"), "user.html does not import user.js?v=7");
    assert!(!page.contains("/static/user.js?v=6"), "user.html still imports the old user.js");

    let js = assets.get("static/user.js").expect("user.js shipped");
    let code: Vec<&str> = js.lines().filter(|l| !is_comment(l)).collect();
    let code = code.join("\n");
    // The MCR route answers 403 to a plain user; the panel must not post there.
    assert!(!code.contains("'/api/jobs'"), "user.js posts to the MCR-only /api/jobs");
    assert!(code.contains("/api/user/jobs"));
    assert!(code.contains("/api/user/prefs"));
    assert!(!code.contains("innerHTML"));
}

#[test]
fn the_first_run_password_card_ships_with_admin_v33_and_login_v7() {
    let assets = all_assets();
    let admin = assets.get("admin.html").expect("admin.html shipped");
    assert!(admin.contains("/static/admin.js?v=33"), "admin.html does not import admin.js?v=33");
    assert!(!admin.contains("/static/admin.js?v=32"), "admin.html still imports the old admin.js");
    let login = assets.get("login.html").expect("login.html shipped");
    assert!(login.contains("/static/login.js?v=7"), "login.html does not import login.js?v=7");
    assert!(!login.contains("/static/login.js?v=6"), "login.html still imports the old login.js");

    let js = assets.get("static/admin.js").expect("admin.js shipped");
    assert!(js.contains("password_change_required"), "admin.js ignores the first-run flag");
    let login_js = assets.get("static/login.js").expect("login.js shipped");
    assert!(login_js.contains("password_change_required"), "login.js ignores the first-run flag");
}

/// The Autonomy card (P11.0) opens the right-hand column and its loader asks
/// the admin endpoint, building nodes rather than writing HTML.
#[test]
fn the_autonomy_card_is_first_in_the_right_column_and_loads_the_report() {
    let assets = all_assets();
    let admin = assets.get("admin.html").expect("admin.html shipped");
    let right = admin.find(r#"<div class="stack" style="grid-column:span 2">"#).expect("the right-hand column");
    let first = admin[right..].find("<section").map(|i| right + i).expect("a card in it");
    assert!(admin[first..].starts_with(r#"<section class="card stack" id="autonomy">"#), "the Autonomy card is not first");
    assert!(admin.contains("A video later marked as wrong counts as touched."));
    for id in ["autonomy-days", "autonomy-refresh", "autonomy-figures", "autonomy-tables"] {
        assert!(admin.contains(&format!(r#"id="{id}""#)), "{id} missing");
    }
    let js = assets.get("static/admin.js").expect("admin.js shipped");
    let code: Vec<&str> = js.lines().filter(|l| !is_comment(l)).collect();
    let code = code.join("\n");
    assert!(code.contains("/api/admin/autonomy-report?days="), "admin.js does not load the autonomy report");
    assert!(code.contains("Delivered untouched"));
    assert!(code.contains("No finished jobs in this period yet."));
    assert!(code.contains("Queued by MCR from an offer or a skipped link"));
    assert!(!code.contains("innerHTML"));
}

/// P13.2: the restart button asks in the page (no browser dialog), calls the
/// restart route through `api()`, and shows the pending watchfolder.
#[test]
fn the_restart_button_asks_in_the_page_and_the_pending_watchfolder_is_shown() {
    let assets = all_assets();
    let html = assets.get("admin.html").expect("admin.html shipped");
    for id in ["restart-box", "restart-warn", "restart-btn", "restart-result", "watchfolder-pending"] {
        assert!(html.contains(&format!(r#"id="{id}""#)), "{id} missing");
    }
    assert!(html.contains("Επανεκκίνηση υπηρεσίας"));
    let js = assets.get("static/admin.js").expect("admin.js shipped");
    let code: String = js.lines().filter(|l| !is_comment(l)).collect::<Vec<_>>().join("\n");
    // The restart block runs up to the next section heading of the raw file.
    let raw = &js[js.find("restart_available").expect("the restart block")..];
    let raw = &raw[..raw.find("/* ----").expect("the next section")];
    let block: String = raw.lines().filter(|l| !is_comment(l)).collect::<Vec<_>>().join("\n");
    assert!(block.contains("api('/api/admin/restart-service'"), "the button calls the route");
    assert!(block.contains("warn.hidden"), "the first click only asks");
    assert!(!block.contains("confirm(") && !block.contains("innerHTML"));
    assert!(code.contains("watchfolder_pending"));
}

/// P13.3: the wizard is the one setup path. No first-run page ships, and nothing
/// points at one or at the endpoints it used.
#[test]
fn no_first_run_page_ships_and_nothing_points_at_it() {
    let assets = all_assets();
    assert!(!assets.contains_key("setup.html") && !assets.contains_key("static/setup.js"));
    for (name, text) in &assets {
        for gone in ["/setup\"", "/setup'", "<strong>/setup</strong>", "/api/setup/state", "'/api/setup'", "setup-hint"] {
            assert!(!text.contains(gone), "{name} still mentions {gone}");
        }
    }
}

/// P11.11c/d: the wizard and the admin's Mailbox card both ask for the secret's
/// expiry date, and the card saves the mailbox (used from the next poll).
#[test]
fn the_wizard_and_the_mailbox_card_ask_for_the_secret_expiry() {
    let assets = all_assets();
    let code = |name: &str| -> String {
        let src = assets.get(name).unwrap_or_else(|| panic!("{name} shipped"));
        src.lines().filter(|l| !is_comment(l)).collect::<Vec<_>>().join("\n")
    };
    let wizard = code("static/wizard.js");
    assert!(wizard.contains("client_secret_expires: expires.value"), "the wizard does not send the expiry");
    let admin = code("static/admin.js");
    assert!(admin.contains("'/api/admin/settings/mail'"), "the Mailbox card does not save");
    assert!(admin.contains("client_secret_expires: document.getElementById('test-mail-expires').value"));
    assert!(!admin.contains("/api/setup/state"), "the Mailbox card must not read the first-run endpoint");
    let html = assets.get("admin.html").expect("admin.html shipped");
    assert!(html.contains(r#"<input type="date" id="test-mail-expires">"#));
    assert!(html.contains(r#"id="save-mail-btn""#));
}

/// P7.12: the wizard is one module under one URL, built with el() only, and the
/// admin page still hands the first-run session to it.
#[test]
fn the_setup_wizard_is_one_module_with_no_innerhtml_and_no_external_origin() {
    let assets = all_assets();
    let wizard = assets.get("static/wizard.js").expect("wizard.js shipped");
    let code: String = wizard.lines().filter(|l| !is_comment(l)).collect::<Vec<_>>().join("\n");
    for banned in ["innerHTML", "outerHTML", "document.write", "insertAdjacentHTML", "http://", "https://", "service install --account"] {
        assert!(!code.contains(banned), "wizard.js contains {banned}");
    }
    // P13.1f: the account is changed in place by Setup, not by re-registering the service.
    assert!(code.contains("/change-account"), "the service-account hints do not point to Setup");
    assert!(code.contains("/api/setup/wizard/account"), "the account step is not wired");
    assert!(code.contains("/api/admin/settings/watchfolder/test"), "the watchfolder test is not wired");
    assert!(code.contains("/api/admin/settings/complete"), "Finish does not complete setup");

    // One URL per ES module: admin.js imports it, nothing else does.
    let mut versions = std::collections::BTreeSet::new();
    for s in assets.values() {
        for (i, _) in s.match_indices("/static/wizard.js?v=") {
            let rest = &s[i + "/static/wizard.js?v=".len()..];
            versions.insert(rest.chars().take_while(|c| c.is_ascii_digit()).collect::<String>());
        }
    }
    assert_eq!(versions, ["7".to_string()].into(), "wizard.js is imported under different versions");
    let admin = assets.get("static/admin.js").expect("admin.js shipped");
    assert!(admin.contains("runWizard"), "admin.js never starts the wizard");
    let page = assets.get("admin.html").expect("admin.html shipped");
    assert!(page.contains("id=\"setup-rerun\""), "the Setup card is missing");
}

/// P13.4: the wizard ends with a delivered test video. Mail's Next needs a
/// passing «Δοκιμή σύνδεσης»; journalists and tools steps are wired; Finish
/// sends the test job or an explicit skip; no browser dialogs.
#[test]
fn the_wizard_ends_with_a_test_download_and_mail_needs_a_passing_test() {
    let assets = all_assets();
    let wizard = assets.get("static/wizard.js").expect("wizard.js shipped");
    let code: String = wizard.lines().filter(|l| !is_comment(l)).collect::<Vec<_>>().join("\n");
    assert!(code.contains("ids.push('check', 'testdl')"), "the test download is not the last step");
    assert!(code.contains("valid: () => complete() && passed()"), "mail's Next does not need a passing test");
    assert!(code.contains("copyable('Mail.Read')"), "the exact permission has no copy button");
    assert!(code.contains("/api/admin/journalists/import"), "the journalists step is not wired");
    assert!(code.contains("/api/admin/tools"), "the tools step is not wired");
    assert!(code.contains("/api/admin/setup/test-download"), "the test download is not wired");
    assert!(code.contains("test_job_id: T.jobId") && code.contains("skip_test: true"), "Finish sends no proof");
    assert!(code.contains("valid: () => Boolean(T.status && T.status.delivered)"), "Finish is open before delivery");
    assert!(code.contains("playout never reads"), "the watchfolder skip does not say where files go");
    for banned in ["confirm(", "prompt(", "alert("] {
        assert!(!code.contains(banned), "wizard.js uses {banned}");
    }
}
