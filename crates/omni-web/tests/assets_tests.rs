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
    for (module, version) in [
        ("desk.js", 23),
        ("inbox.js", 30),
        ("approval.js", 18),
        ("notify.js", 18),
        ("mcr.js", 56),
        ("status.js", 6),
        ("status.css", 8),
        ("worklist.js", 6),
        ("undo.js", 7),
        ("dialog.js", 6),
        ("keys.js", 4),
        ("sound.js", 3),
        ("user.js", 16),
        ("toast.js", 6),
        ("text.js", 5),
        ("text.el.js", 5),
    ] {
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
    // P12.7: the words are in the dictionary (text.el.js), the code reads them by key.
    let words = desk_dictionary(&assets);
    assert!(mcr.contains("t('late.more'") && words["late.more"].contains("και άλλα"), "the strip does not cap its rows");
    let page = assets.get("mcr.html").expect("mcr.html shipped");
    let strip = page.find("id=\"late-strip\"").expect("the strip is in mcr.html");
    assert!(strip < page.find("class=\"tabs\"").unwrap(), "the strip is not above the tab bar");

    let notify = assets.get("static/notify.js").expect("notify.js shipped");
    assert!(notify.contains("  workers: '"), "CHECK_WHO has no entry for the workers check");
    assert!(notify.contains("queue.late"), "notify.js does not read queue.late");
    let need = notify.split("function needCount").nth(1).and_then(|s| s.split('}').next()).unwrap();
    assert!(!need.contains("late"), "late videos must not count as needing a person");

    let desk = assets.get("static/desk.js").expect("desk.js shipped");
    assert!(
        desk.contains("t('desk.wait.delivery')") && words["desk.wait.delivery"] == "Περιμένει τον φάκελο παράδοσης",
        "desk.js has no watchfolder wait text"
    );
    assert!(desk.contains("export function lateReason"), "desk.js has no lateReason");

    // P11.7: a stream still live says so, with its next check's time, in the late strip.
    let late = desk.split("export function lateReason").nth(1).and_then(|s| s.split("\nexport function").next()).unwrap();
    let live = late.split("case 'live':").nth(1).expect("lateReason has no case for wait_reason 'live'");
    let live = live.split("case '").next().unwrap();
    assert!(
        live.contains("t('desk.late.live')") && words["desk.late.live"] == "Ζωντανή μετάδοση",
        "the live case does not say «Ζωντανή μετάδοση»:\n{live}"
    );
    assert!(
        live.contains("t('desk.late.live_at', { at })") && live.contains("clock(job.next_try_at)")
            && words["desk.late.live_at"].contains("επόμενος έλεγχος {at}"),
        "the live case does not show the next check's time:\n{live}"
    );
}

/// The desk's dictionary (P12.7), `static/text.el.js`, as key -> text. Fails on a key given twice
/// (a JS object keeps the last one silently).
fn desk_dictionary(assets: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let src = assets.get("static/text.el.js").expect("text.el.js shipped");
    let body = src.split("export default {").nth(1).and_then(|s| s.split("\n};").next()).expect("text.el.js exports a dictionary");
    let mut out = BTreeMap::new();
    for line in body.lines().map(str::trim).filter(|l| l.starts_with('\'')) {
        let key = line[1..].split('\'').next().expect("a key").to_string();
        let value = line.split_once("': '").map(|(_, v)| v.trim_end_matches(',').trim_end_matches('\'').to_string()).unwrap_or_else(|| panic!("no text for {key}"));
        assert!(out.insert(key.clone(), value).is_none(), "text.el.js has `{key}` twice");
    }
    out
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

/// P12.1: one vocabulary. Every status the server sends has its words, its colour family and a
/// mark in `status.js`, and every colour family has its badge, tag and legend colour. Approval is
/// violet everywhere (it was amber on the badges, violet in the rail), a cancelled job no longer
/// shares the waiting colour, and no page keeps a status table of its own (/user's `statusClass`
/// listed statuses that do not exist, so RUNNING was grey there).
#[test]
fn every_status_has_one_word_one_colour_and_a_mark_in_status_js() {
    let assets = all_assets();
    let js = assets.get("static/status.js").expect("status.js shipped");
    let css = format!(
        "{}\n{}",
        assets.get("static/app.css").expect("app.css shipped"),
        assets.get("static/status.css").expect("status.css shipped")
    );
    // P12.7: the tones are in status.js (`NAME: 'tone',`), the words in text.el.js
    // (`status.NAME.label|short|phrase|user`, `tone.<tone>`).
    let words = desk_dictionary(&assets);
    let tones = js
        .split("export const TONES = {")
        .nth(1)
        .and_then(|s| s.split("};").next())
        .expect("status.js has a TONES table");
    let statuses = js
        .split("export const STATUS_TONE = {")
        .nth(1)
        .and_then(|s| s.split("};").next())
        .expect("status.js has a STATUS_TONE table");
    let tone_of = |name: &str| -> String {
        let head = format!("{name}: '");
        statuses
            .lines()
            .find_map(|l| l.trim_start().strip_prefix(head.as_str()))
            .and_then(|s| s.split('\'').next())
            .unwrap_or_else(|| panic!("status.js has no tone for {name}"))
            .to_string()
    };

    for status in JobStatus::ALL {
        let name = status.as_str();
        for key in ["label", "short", "phrase", "user"] {
            let k = format!("status.{name}.{key}");
            assert!(words.get(&k).is_some_and(|w| !w.is_empty()), "{name} has no `{key}` text (`{k}`)");
        }
        let tone = tone_of(name);
        let mark = tones
            .lines()
            .find(|l| l.trim_start().starts_with(&format!("{tone}: {{")))
            .and_then(|l| l.split("mark: '").nth(1))
            .and_then(|s| s.split('\'').next())
            .unwrap_or("");
        assert!(!mark.is_empty(), "tone {tone} ({name}) has no mark beside its colour");
        assert!(words.contains_key(&format!("tone.{tone}")), "tone {tone} has no legend word");
        for selector in [format!(".badge.{tone} "), format!(".tag.{tone} "), format!(".legend i.{tone} ")] {
            assert!(css.contains(&selector), "tone {tone} ({name}) has no `{selector}` colour rule");
        }
    }
    assert_eq!(tone_of("AWAITING_APPROVAL"), "approval", "approval is not its own (violet) colour");
    assert_ne!(tone_of("CANCELLED"), tone_of("PENDING"), "a cancelled job looks like a waiting one");
    assert!(css.contains(".badge.approval") && css.contains("var(--busy)"), "the approval badge is not violet");

    // Everyone reads the one table; nobody keeps a status table of their own.
    for module in ["desk.js", "inbox.js", "approval.js", "mcr.js", "user.js"] {
        let src = assets.get(&format!("static/{module}")).unwrap_or_else(|| panic!("{module} shipped"));
        assert!(src.contains("/static/status.js?v="), "{module} does not read status.js");
        let code: String = src.lines().filter(|l| !is_comment(l)).collect::<Vec<_>>().join("\n");
        for own in [
            "statusClass(",
            "STATUS_TEXT",
            "function statusBadge",
            "function tone(",
            "case 'CANCELLED': return",
            "'Για έγκριση'",
        ] {
            assert!(!code.contains(own), "{module} keeps its own status words or colours: {own}");
        }
    }
    for page in ["mcr.html", "user.html"] {
        let html = assets.get(page).unwrap_or_else(|| panic!("{page} shipped"));
        let app = html.find("/static/app.css?v=").expect("links app.css");
        let status = html.find("/static/status.css?v=").unwrap_or_else(|| panic!("{page} does not link status.css"));
        assert!(app < status, "{page} links status.css before app.css, so app.css would win");
    }
}

/// P12.2: the review tab is a work list grouped by what the operator has to do. «Για έγκριση»
/// first (the approval section), then cookies, no video, manual download, other; a job that will
/// retry by itself is not in it; one sentence why, one primary action, the rest folded, and the
/// code and the tool's own words only under «Τεχνικές λεπτομέρειες».
#[test]
fn the_review_tab_is_a_work_list_grouped_by_what_to_do() {
    use omni_broadcast::errors::ErrorCode;
    let assets = all_assets();
    let wl = assets.get("static/worklist.js").expect("worklist.js shipped");
    let mcr = assets.get("static/mcr.js").expect("mcr.js shipped");
    let page = assets.get("mcr.html").expect("mcr.html shipped");

    // The groups, in order, after the approval section; their names in the dictionary (P12.7).
    let words = desk_dictionary(&assets);
    let groups = wl.split("export const GROUPS = [").nth(1).and_then(|s| s.split("];").next()).expect("GROUPS");
    let agreed = [
        ("cookies", "Χρειάζεται σύνδεση (cookies)"),
        ("novideo", "Δεν βρέθηκε βίντεο"),
        ("manual", "Χειροκίνητη λήψη"),
        ("other", "Άλλο"),
    ];
    let order = agreed.map(|(key, label)| {
        assert_eq!(words.get(&format!("work.group.{key}")).map(String::as_str), Some(label), "group {key} is not «{label}»");
        groups.find(&format!("key: '{key}'")).unwrap_or_else(|| panic!("no group {key}"))
    });
    assert!(order.windows(2).all(|w| w[0] < w[1]), "the groups are not in the agreed order");
    let review = page.find("id=\"tab-review\"").expect("review tab");
    let approval = page[review..].find("id=\"approval-section\"").expect("approval section in the review tab");
    let cards = page[review..].find("id=\"review-cards\"").expect("review cards");
    assert!(approval < cards, "«Για έγκριση» is not the first group");

    // Only what waits for a person; never a job that retries by itself.
    let needs = wl.split("const NEEDS_YOU = [").nth(1).and_then(|s| s.split("];").next()).expect("NEEDS_YOU");
    for status in ["'REQUIRES_REVIEW'", "'MANUAL_DOWNLOAD'", "'FAILED'"] {
        assert!(needs.contains(status), "{status} is missing from the work list");
    }
    for status in ["PENDING", "RUNNING", "AWAITING_APPROVAL", "COMPLETED", "CANCELLED"] {
        assert!(!needs.contains(status), "{status} would be in the work list");
    }
    // Every code the groups name is a real one (a typo would silently land in «Άλλο»).
    for list in ["COOKIE_CODES", "NO_VIDEO_CODES", "MANUAL_CODES"] {
        let body = wl.split(&format!("const {list} = [")).nth(1).and_then(|s| s.split("];").next()).expect(list);
        for code in body.split(',').map(|c| c.trim().trim_matches('\'')).filter(|c| !c.is_empty()) {
            assert!(ErrorCode::from_code(code).is_some(), "{list} names {code}, which is not an error code");
        }
    }
    // The state decides before the code: a file-locker link is a manual download.
    let group_of = wl.split("export function groupOf").nth(1).and_then(|s| s.split("\n}").next()).expect("groupOf");
    let state = group_of.find("job.status === 'MANUAL_DOWNLOAD'").expect("groupOf does not look at the state");
    assert!(state < group_of.find("includes(code)").unwrap(), "groupOf reads the code before the state");
    // A job in the list never says it is being retried.
    let why = wl.split("export function whyText").nth(1).and_then(|s| s.split("\n}").next()).expect("whyText");
    assert!(
        why.contains("EL['server.retrying']") && why.contains("t('work.why.stopped')") && words["server.retrying"] == "αυτόματη επανάληψη",
        "a stopped job can say it retries by itself"
    );

    // mcr.js: the list is grouped, still reconciled in place (P12.3), with the panels kept open.
    assert!(mcr.contains("const jobs = inGroupOrder(visible('review', data.jobs || []));"), "the review list is not grouped");
    assert!(mcr.contains("heading: (job) => groupLabel(groupOf(job)) || null"), "the headings are not the groups");
    assert!(!mcr.contains("dayHeading"), "the review list is still grouped by date");
    let card = mcr.split("function reviewCard(job)").nth(1).and_then(|s| s.split("\n}\n").next()).expect("reviewCard");
    assert!(card.contains("whyText(job)") && !card.contains("job.hint"), "the card does not use the one why-sentence");
    assert!(card.contains("primaryOf(job)"), "the card has no primary action");
    assert!(
        card.contains("keptDetails(moreOpen, job.id, 'wl-more', t('review.more')") && words["review.more"] == "Περισσότερα",
        "no «Περισσότερα» that stays open"
    );
    assert_eq!(words["card.tech"], "Τεχνικές λεπτομέρειες");
    let tech = card
        .split("keptDetails(techOpen, job.id, 'tech', t('card.tech'),")
        .nth(1)
        .and_then(|s| s.split("    : null;").next())
        .expect("no «Τεχνικές λεπτομέρειες» that stays open on the card");
    for (shown, field) in [("t('card.code'", "job.error_code"), ("t('card.cause'", "job.error_message")] {
        assert!(tech.contains(shown) && tech.contains(field), "«Τεχνικές λεπτομέρειες» no longer shows {shown}");
        assert_eq!(card.matches(shown).count(), 1, "{shown} is shown outside «Τεχνικές λεπτομέρειες»");
    }

    // Every button a hint names by «…» is on the card (or its history panel).
    let history = mcr.split("function historyPanel(job)").nth(1).and_then(|s| s.split("\n}\n").next()).expect("historyPanel");
    let panel = format!("{history}\n{card}");
    // The words the card and its history panel show: the texts of the keys they look up.
    let shown: Vec<&str> = panel
        .split("t('")
        .skip(1)
        .filter_map(|s| s.split('\'').next())
        .filter_map(|key| words.get(key).map(String::as_str))
        .collect();
    let quoted = |text: &str| -> Vec<String> {
        text.split('«').skip(1).filter_map(|s| s.split('»').next()).map(str::to_string).collect()
    };
    let mut hints: Vec<String> = ErrorCode::ALL.iter().map(|c| c.hint_el().to_string()).collect();
    hints.push(omni_broadcast::errors::exhausted_hint_el(3));
    hints.push(omni_broadcast::errors::LIVE_STREAM_EXHAUSTED_EL.to_string());
    hints.extend(["work.why.locker", "work.why.stopped"].map(|k| words[k].clone()));
    for hint in &hints {
        for name in quoted(hint) {
            if name.starts_with('-') {
                continue; // an example of a link's tail, not a button
            }
            assert!(shown.iter().any(|w| w.contains(name.as_str())), "a hint names «{name}», which the review card does not have:\n{hint}");
        }
    }
}

/// The body of `function name(` in `src`, up to the next top-level `}`.
fn function_body<'a>(src: &'a str, name: &str) -> &'a str {
    src.split(&format!("function {name}(")).nth(1).and_then(|s| s.split("\n}\n").next()).unwrap_or_else(|| panic!("no function {name}"))
}

/// P12.4: no native dialogs on the desk and /user. What can be taken back acts at once with
/// «Αναίρεση» for 8 s; what puts a video on air or closes a job for good asks in the page's own
/// dialog (focus trap, Escape, focus return); «Λάθος βίντεο» is the same dialog wherever it is;
/// every action of an Email-tab card shows without selecting the card first.
#[test]
fn the_desk_uses_no_native_dialogs() {
    let assets = all_assets();
    let code = |module: &str| -> String {
        let src = assets.get(&format!("static/{module}")).unwrap_or_else(|| panic!("{module} shipped"));
        src.lines().filter(|l| !is_comment(l)).collect::<Vec<_>>().join("\n")
    };
    // A call of the browser's own, not `confirmDialog(` or `x.confirm(`-like names of ours.
    let calls = |src: &str, name: &str| -> bool {
        let needle = format!("{name}(");
        src.match_indices(needle.as_str()).any(|(i, _)| {
            let before = src[..i].chars().next_back();
            !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
    };
    let desk_modules = [
        "app.js", "desk.js", "mcr.js", "inbox.js", "approval.js", "notify.js", "user.js",
        "status.js", "worklist.js", "undo.js", "dialog.js", "toast.js", "keys.js", "sound.js",
    ];
    for module in desk_modules {
        let src = code(module);
        for native in ["confirm", "prompt", "alert"] {
            assert!(!calls(&src, native), "{module} still calls the browser's {native}()");
        }
    }

    // Reversible: at once, with «Αναίρεση» for 8 seconds.
    let undo = code("undo.js");
    let words = desk_dictionary(&assets);
    assert!(
        undo.contains("export const UNDO_MS = 8000;") && undo.contains("t('undo.button')") && words["undo.button"] == "Αναίρεση",
        "no 8 s «Αναίρεση»"
    );
    let desk = code("desk.js");
    assert!(function_body(&desk, "discardJob").contains("deferAction({"), "removing a job is not undoable");
    assert!(function_body(&desk, "cancelJob").contains("undoToast("), "a desk cancel has no «Αναίρεση»");
    let mcr = code("mcr.js");
    for (what, call) in [("clear-finished", "'/api/jobs/clear-finished'"), ("hide-old", "'/api/jobs/review/hide-old'")] {
        let at = mcr.find(call).unwrap_or_else(|| panic!("mcr.js no longer calls {what}"));
        let before = &mcr[..at];
        let defer = before.rfind("deferAction({").unwrap_or(0);
        assert!(defer > 0 && before[defer..].matches("addEventListener").count() == 0, "{what} is not sent through deferAction");
    }
    assert!(function_body(&code("user.js"), "cancelJob").contains("deferAction({"), "a journalist's cancel has no «Αναίρεση»");
    // What is waiting to be sent is already gone from every list.
    for needle in ["visible('live',", "visible('review',", "visible('late',", "visible('completed',"] {
        assert!(mcr.contains(needle), "mcr.js does not hide what waits for its «Αναίρεση»: {needle}");
    }
    assert!(code("inbox.js").contains("d.jobs = visible('email',"), "the Email tab shows a job being removed");

    // On air or closed for good: the page's dialog.
    for name in ["markDoneJob", "redownloadJob"] {
        assert!(function_body(&desk, name).contains("await confirmDialog({"), "{name} does not ask in the dialog");
    }
    assert!(function_body(&desk, "wrongVideoJob").contains("await confirmWrongVideo("), "«Λάθος βίντεο» (delivered) does not ask");
    let approval = code("approval.js");
    assert!(
        approval.contains("await confirmDialog({") && approval.contains("confirm: t('approval.approve')") && words["approval.approve"] == "Έγκριση → Dalet",
        "approve does not ask in the dialog"
    );
    assert_eq!(approval.matches("await confirmWrongVideo(").count(), 2, "«Λάθος βίντεο» and «Επόμενη επιλογή» do not ask like the delivered card");
    let dialog = code("dialog.js");
    for (what, needle) in [
        ("Escape closes", "ev.key === 'Escape'"),
        ("Tab stays inside", "ev.key === 'Tab'"),
        ("the page behind is inert", "node.inert = true"),
        ("focus goes back", "opener.focus()"),
        ("focus starts on «Άκυρο»", "no.focus();"),
    ] {
        assert!(dialog.contains(needle), "the dialog lost: {what}");
    }

    // Rename is a field on the card with a live preview, from the server's own keyword and
    // file-name rules (P12.11: the slug preview, read as a rename).
    let rename = function_body(&desk, "renameForm");
    assert!(rename.contains("slugPreview({ index: job.index_str, journalist: job.journalist, keyword: input.value, for: 'rename' })"), "the rename field has no live preview");
    assert!(rename.contains("ev.key === 'Escape'") && rename.contains("ev.key === 'Enter'"), "the rename field lacks Enter/Escape");

    // Every action of an Email-tab card shows without selecting it first.
    let more = function_body(&code("inbox.js"), "moreNode").to_string();
    for gate in ["sel ||", "sel &&", "else if (sel)"] {
        assert!(!more.contains(gate), "an Email-tab card still hides actions until selected: {gate}");
    }
}

/// `(selector, font-size in px)` of every CSS rule that sets a px font size.
/// Comments are dropped; a rule inside `@media` keeps its own selector.
fn css_font_sizes(css: &str) -> Vec<(String, f64)> {
    let mut text = String::new();
    let mut rest = css;
    while let Some(i) = rest.find("/*") {
        text.push_str(&rest[..i]);
        rest = rest[i..].find("*/").map_or("", |j| &rest[i + j + 2..]);
    }
    text.push_str(rest);
    let mut out = Vec::new();
    for chunk in text.split('}') {
        let Some((pre, body)) = chunk.rsplit_once('{') else { continue };
        let selector = pre.rsplit('{').next().unwrap_or(pre).trim().to_string();
        for decl in body.split(';') {
            let Some(value) = decl.trim().strip_prefix("font-size:") else { continue };
            if let Some(px) = value.trim().strip_suffix("px").and_then(|n| n.trim().parse::<f64>().ok()) {
                out.push((selector.clone(), px));
            }
        }
    }
    out
}

/// WCAG relative luminance of `#rrggbb`.
fn luminance(hex: &str) -> f64 {
    let hex = hex.trim().trim_start_matches('#');
    let channel = |i: usize| {
        let c = f64::from(u8::from_str_radix(&hex[i..i + 2], 16).expect("hex colour")) / 255.0;
        if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * channel(0) + 0.7152 * channel(2) + 0.0722 * channel(4)
}

fn contrast(a: &str, b: &str) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// The last `--name: #rrggbb;` in `css`.
fn css_var(css: &str, name: &str) -> String {
    let needle = format!("--{name}:");
    let at = css.rfind(&needle).unwrap_or_else(|| panic!("no --{name}"));
    css[at + needle.len()..].split(';').next().unwrap().trim().to_string()
}

/// P12.5: the desk laid out for the control room. One page width on every tab (fluid to 1760 px);
/// a header with the product, one health pill, sound and account, on one line; nothing under
/// 12 px on the desk's pages and the dim text readable at 4.5:1; no static inline styles left in
/// the desk's JS and pages.
#[test]
fn the_desk_is_laid_out_for_the_control_room() {
    let assets = all_assets();
    let app_css = assets.get("static/app.css").expect("app.css shipped");
    let desk_css = assets.get("static/status.css").expect("status.css shipped");

    // Nothing under 12 px: every smaller size of app.css is raised in status.css (which /mcr and
    // /user load after it), and status.css itself has none.
    let raised: BTreeMap<String, f64> = css_font_sizes(desk_css).into_iter().collect();
    for (selector, px) in css_font_sizes(desk_css) {
        assert!(px >= 12.0, "status.css sets {selector} to {px}px");
    }
    for (selector, px) in css_font_sizes(app_css) {
        if px >= 12.0 {
            continue;
        }
        let to = raised.get(&selector).unwrap_or_else(|| panic!("{selector} is {px}px in app.css and not raised on the desk"));
        assert!(*to >= 12.0, "{selector} is raised to only {to}px");
    }
    let base = css_font_sizes(desk_css).into_iter().find(|(s, _)| s == "body").expect("status.css sets the body size").1;
    assert!((15.0..=16.0).contains(&base), "body text is {base}px, not 15–16");

    // Contrast: text, dim and faint against the page and a card, at least 4.5:1.
    let both = format!("{app_css}\n{desk_css}");
    let background = css_var(&both, "bg");
    for name in ["text", "text-dim", "text-faint"] {
        let colour = css_var(&both, name);
        for (surface, bg) in [("the page", background.as_str()), ("a card", "#12141d")] {
            let ratio = contrast(&colour, bg);
            assert!(ratio >= 4.5, "--{name} {colour} on {surface} is {ratio:.2}:1");
        }
    }

    // One width on every tab.
    assert!(desk_css.contains(".desk-page main { max-width: 1760px;"), "the desk is not fluid to 1760 px");
    let page = assets.get("mcr.html").expect("mcr.html shipped");
    assert!(page.contains("<body class=\"desk-page\">"), "mcr.html is not a desk page");
    let mcr = assets.get("static/mcr.js").expect("mcr.js shipped");
    assert!(!mcr.contains("desk-wide"), "one tab is still wider than the others");

    // The header: product, health pill, sound, account; on one line.
    let header = page.split("<header").nth(1).and_then(|s| s.split("</header>").next()).expect("header");
    for needle in ["OmniDownloader", "id=\"health-pill\"", "id=\"sound-slot\"", "href=\"/admin\"", "id=\"logout-btn\"", "id=\"health-panel\""] {
        assert!(header.contains(needle), "the header lost {needle}");
    }
    for old in ["mail-status", "llm-status", "queue-status", "storage-status", "temp-status"] {
        assert!(!header.contains(old), "the header still carries the old status line {old}");
    }
    assert!(desk_css.contains(".desk-page .topbar-status { flex-wrap: nowrap;"), "the header can wrap");
    let notify = assets.get("static/notify.js").expect("notify.js shipped");
    let summary = function_body(notify, "healthSummary");
    let words = desk_dictionary(&assets);
    assert!(
        summary.contains("t('health.pill.ok')") && summary.contains("tn('health.pill.problems', count)")
            && words["health.pill.ok"] == "Όλα καλά" && words["health.pill.problems.other"] == "{n} προβλήματα",
        "the pill does not say «Όλα καλά» / «N προβλήματα»"
    );
    assert!(
        mcr.contains("paintHealth(data)") && mcr.contains("t('health.fact.watchfolder')") && words["health.fact.watchfolder"] == "Φάκελος παράδοσης (Dalet)",
        "the panel has no free space"
    );

    // No static inline styles: only a progress width, a CSS variable and a computed scroll margin.
    for module in [
        "desk.js", "mcr.js", "inbox.js", "approval.js", "notify.js", "user.js", "status.js", "worklist.js", "undo.js", "dialog.js",
        "toast.js", "keys.js", "sound.js",
    ] {
        let src = assets.get(&format!("static/{module}")).unwrap_or_else(|| panic!("{module} shipped"));
        for (i, _) in src.match_indices("style: ") {
            let value = &src[i + "style: ".len()..];
            assert!(value.starts_with("`width:${"), "{module} has a static inline style: {}", &value[..value.len().min(60)]);
        }
        for (i, _) in src.match_indices(".style.") {
            let value = &src[i + ".style.".len()..];
            assert!(
                value.starts_with("setProperty('--p'") || value.starts_with("scrollMarginTop"),
                "{module} sets a style property: {}",
                &value[..value.len().min(60)]
            );
        }
    }
    for page in ["mcr.html", "user.html"] {
        assert!(!assets.get(page).unwrap().contains("style=\""), "{page} has an inline style");
    }
}

/// P12.6: health in plain Greek. Every check the daemon can report (the names in
/// `omni_core::health::checks`) has three Greek texts on the desk: what happened (by state), what
/// it means for the videos, what to do and who does it. The banner never shows the tool's English
/// detail, a site line is a Greek sentence, and an error message stays until closed, is announced
/// assertively, and stacks at most three.
#[test]
fn health_is_told_in_plain_greek() {
    let assets = all_assets();
    let notify = assets.get("static/notify.js").expect("notify.js shipped");
    let mcr = assets.get("static/mcr.js").expect("mcr.js shipped");
    let greek = |s: &str| s.chars().any(|c| ('\u{0370}'..='\u{03FF}').contains(&c));

    let health_rs = include_str!("../../omni-core/src/health.rs");
    let module = health_rs.split("pub mod checks {").nth(1).and_then(|s| s.split("\n}").next()).expect("checks module");
    let names: Vec<&str> = module
        .lines()
        .filter_map(|l| l.trim().strip_prefix("pub const "))
        .filter_map(|l| l.split('"').nth(1))
        .collect();
    assert!(names.len() >= 15, "found only {names:?} in health.rs");
    // P12.7: who acts is in notify.js (`  name: 'who',`), the texts in text.el.js (`health.<name>.…`).
    let words = desk_dictionary(&assets);
    let table = notify.split("export const CHECK_WHO = {").nth(1).and_then(|s| s.split("\n};").next()).expect("CHECK_WHO");
    for name in names {
        let head = format!("  {name}: '");
        let who = table
            .lines()
            .find_map(|l| l.strip_prefix(head.as_str()))
            .and_then(|s| s.split('\'').next())
            .unwrap_or_else(|| panic!("no row for the `{name}` check in CHECK_WHO"));
        assert!(who == "mcr" || who == "admin", "`{name}` does not say who acts: {who:?}");
        assert!(words.get(&format!("health.{name}.name")).is_some_and(|w| !w.is_empty()), "`{name}` has no name");
        for key in ["degraded", "down", "means", "todo"] {
            let text = words.get(&format!("health.{name}.{key}")).map(String::as_str).unwrap_or("");
            assert!(greek(text), "`{name}` has no Greek `{key}`: {text:?}");
        }
    }

    // The banner and the panel's lines: the three texts, never the tool's detail.
    let texts = function_body(mcr, "healthTextNodes");
    for needle in ["ct.happened", "ct.means", "ct.todo", "WHO[ct.who]", "t('health.label.means')", "t('health.label.todo')"] {
        assert!(texts.contains(needle), "a health line lost {needle}");
    }
    assert_eq!((words["health.label.means"].as_str(), words["health.label.todo"].as_str()), ("Για τα βίντεο: ", "Τι κάνετε: "));
    assert!(!texts.contains(".detail"), "a health line shows the tool's own detail");
    let banner = mcr
        .split("const problems = healthProblems(checks);")
        .nth(1)
        .and_then(|s| s.split("render(banner,").nth(1))
        .and_then(|s| s.split(";\n").next())
        .expect("the health banner");
    assert!(banner.contains("healthTextNodes(") && !banner.contains("detail"), "the banner shows the tool's detail:\n{banner}");
    assert!(mcr.contains("el('summary', {}, t('card.tech'))"), "the panel has no place for the tool's detail");
    // A site is a sentence, from the payload's `sites` rows.
    assert!(texts.contains("name === 'sites'") && texts.contains("map(siteSentence)"), "the sites line is not told per site");
    let site = function_body(notify, "siteSentence");
    for needle in ["t('site.sentence'", "domain: site.domain", "t('site.since', { at })", "tn('site.fails', n)", "'site.broken'"] {
        assert!(site.contains(needle), "the site sentence lost {needle}");
    }
    for (key, text) in [
        ("site.sentence", "Ο ιστότοπος {domain} {what}{since}{fails}."),
        ("site.since", " από τις {at}"),
        ("site.fails.other", " ({n} αποτυχίες στη σειρά)"),
        ("site.broken", "δεν δίνει βίντεο"),
    ] {
        assert_eq!(words[key], text, "the site sentence's `{key}` changed");
    }

    // Messages: an error stays until closed, is announced at once, three at most.
    let toast = assets.get("static/toast.js").expect("toast.js shipped");
    assert!(toast.contains("role: 'alert', 'aria-live': 'assertive'"), "errors are not announced assertively");
    assert!(toast.contains("export const MAX_ERRORS = 3;") && toast.contains("errors.children.length > MAX_ERRORS"), "errors can pile up");
    let bad = toast.split("if (kind === 'bad') {").nth(1).and_then(|s| s.split("\n  }\n").next()).expect("the error branch");
    assert!(bad.contains("toast-close") && !bad.contains("setTimeout"), "an error message goes away by itself");
    for module in ["desk.js", "mcr.js", "inbox.js", "approval.js", "notify.js", "user.js", "undo.js"] {
        let src = assets.get(&format!("static/{module}")).unwrap();
        assert!(src.contains("import { toast } from '/static/toast.js?v="), "{module} does not use the desk's messages");
        let app_import = src.split("from '/static/app.js").next().and_then(|s| s.rsplit("import").next()).unwrap_or("");
        assert!(!app_import.contains("toast"), "{module} still takes toast() from app.js");
    }
}

/// P12.7: every word of the desk's modules is in one dictionary, `static/text.el.js`, read through
/// `text.js` (a lookup with a language switch, Greek by default, that the admin pages can give an
/// English dictionary). No module keeps Greek text of its own; every key a module or the desk page
/// asks for exists; the stragglers are translated; lane auto's two new checks have their words.
#[test]
fn every_word_of_the_desk_is_in_one_dictionary() {
    let assets = all_assets();
    let words = desk_dictionary(&assets);
    let greek = |s: &str| s.chars().any(|c| ('\u{0370}'..='\u{03FF}').contains(&c));
    let modules = [
        "desk.js", "mcr.js", "inbox.js", "approval.js", "notify.js", "user.js", "status.js", "worklist.js", "undo.js",
        "dialog.js", "toast.js", "text.js", "keys.js", "sound.js",
    ];

    // The lookup: a language switch, Greek the default and the fallback.
    let lookup = assets.get("static/text.js").expect("text.js shipped");
    for export in [
        "export function t(", "export function tn(", "export function setLanguage(", "export function addLanguage(",
        "export function language(", "export function applyText(",
    ] {
        assert!(lookup.contains(export), "text.js lost {export}");
    }
    assert!(lookup.contains("let current = 'el';") && lookup.contains(": EL[key]"), "Greek is not the default and the fallback");

    for module in modules {
        let src = assets.get(&format!("static/{module}")).unwrap_or_else(|| panic!("{module} shipped"));
        // No Greek of its own (comments aside).
        for line in src.lines().filter(|l| !is_comment(l)) {
            assert!(!greek(line), "{module} keeps Greek text of its own: {}", line.trim());
        }
        let code = src.lines().filter(|l| !is_comment(l)).collect::<Vec<_>>().join("\n");
        let src = code.as_str();
        // Every literal key it asks for is in the dictionary (tn: both forms).
        for (call, plural) in [("t('", false), ("tn('", true)] {
            // `t('` as a call of its own, not the end of `set('` or `split('`.
            let keys = src.match_indices(call).filter(|(i, _)| {
                !src[..*i].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
            });
            for key in keys.filter_map(|(i, _)| src[i + call.len()..].split('\'').next()) {
                let wanted: Vec<String> = if plural { vec![format!("{key}.one"), format!("{key}.other")] } else { vec![key.to_string()] };
                for k in wanted {
                    assert!(words.contains_key(&k), "{module} asks for `{k}`, which text.el.js does not have");
                }
            }
        }
    }
    // Keys chosen by a condition (`t(ok ? 'a.b' : 'a.c')`): any quoted `prefix.rest` whose prefix is
    // one of the dictionary's must be in it.
    let prefixes: std::collections::BTreeSet<&str> = words.keys().filter_map(|k| k.split('.').next()).collect();
    for module in modules {
        let src = assets.get(&format!("static/{module}")).unwrap();
        let code = src.lines().filter(|l| !is_comment(l)).collect::<Vec<_>>().join("\n");
        for quoted in code.split('\'').skip(1).step_by(2) {
            let looks_like_key = quoted.contains('.')
                && quoted.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_')
                && prefixes.contains(quoted.split('.').next().unwrap());
            if looks_like_key && !quoted.ends_with('.') {
                assert!(words.contains_key(quoted) || words.contains_key(&format!("{quoted}.one")), "{module} names `{quoted}`, which text.el.js does not have");
            }
        }
    }
    // The keys built from a name: every stage, every short stage, every filter, every gate check.
    for stage in ["QUEUED", "EXTRACT", "DOWNLOAD", "PROBE", "TRANSCODE", "REWRAP", "VERIFY", "DELIVER", "ARCHIVE", "DONE"] {
        assert!(words.contains_key(&format!("stage.{stage}")) && words.contains_key(&format!("user.stage.{stage}")), "no words for {stage}");
    }
    for key in ["all", "active", "attention", "done", "empty"] {
        assert!(words.contains_key(&format!("filter.{key}")), "no words for the filter {key}");
    }
    for key in ["egress", "ad", "probe", "length", "short"] {
        assert!(words.contains_key(&format!("approval.check.{key}")), "no words for the gate check {key}");
    }

    // The page's fixed words, by key (data-t), all there; the stragglers are gone.
    let page = assets.get("mcr.html").expect("mcr.html shipped");
    for attr in ["data-t=\"", "data-t-aria-label=\""] {
        for key in page.split(attr).skip(1).filter_map(|s| s.split('"').next()) {
            assert!(words.contains_key(key), "mcr.html asks for `{key}`, which text.el.js does not have");
        }
    }
    assert!(assets.get("static/mcr.js").unwrap().contains("applyText();"), "the page's fixed words are never filled");
    for straggler in ["Master Control Room", "Watchfolder:", "Temp:", "LLM:", ">Email<", "Email <span"] {
        assert!(!page.contains(straggler), "mcr.html still says {straggler}");
        for module in modules {
            assert!(!assets.get(&format!("static/{module}")).unwrap().contains(straggler), "{module} still says {straggler}");
        }
    }
    assert_eq!(words["tab.email"], "Εισερχόμενα");
    // The broadcast specification keeps its values word for word (the on-air format does not change).
    for (key, value) in [
        ("spec.video.value", "MPEG-2 (Sony XDCAM HD422)"),
        ("spec.bitrate.value", "50 Mbps CBR"),
        ("spec.size.value", "1920×1080, 16:9"),
        ("spec.scan.value", "25 fps, 1080i50, πρώτο το πάνω πεδίο"),
        ("spec.chroma.value", "4:2:2 (yuv422p)"),
        ("spec.audio_layout.value", "EBU R48 — 8 ανεξάρτητα μονοφωνικά κανάλια"),
        ("spec.audio.value", "PCM 24-bit @ 48 kHz"),
        ("spec.channels.value", "1 Αριστερό, 2 Δεξί, 3–8 σιωπή"),
        ("spec.wrap.value", "SMPTE RDD9 OP1a (bmxtranswrap)"),
    ] {
        assert_eq!(words[key], value, "the broadcast specification's `{key}` changed");
    }

    // Lane auto's two checks (P12.7 brief): words ready before the check reaches this branch.
    let notify = assets.get("static/notify.js").unwrap();
    for name in ["maintenance", "database"] {
        assert!(notify.contains(&format!("  {name}: 'admin',")), "the `{name}` check is not the administrator's");
        for key in ["name", "degraded", "down", "means", "todo"] {
            assert!(words.contains_key(&format!("health.{name}.{key}")), "the `{name}` check has no `{key}`");
        }
    }
    assert!(words["health.database.todo.down"].contains("αντίγραφο ασφαλείας"), "a damaged database does not say to restore the backup");
}

/// P12.8: keyboard and accessibility. J/K (and the arrows on a card) walk the work list, Enter
/// opens a card, R retries, A approves through the dialog, `/` searches, `?` shows the shortcuts;
/// none of it fires while typing, with a modifier, or under a dialog. Real buttons only (no
/// `role=button`, so no button inside one); the pictures that seek or open large are buttons; one
/// focus ring; less motion when the system asks for it.
#[test]
fn the_desk_works_from_the_keyboard() {
    let assets = all_assets();
    let words = desk_dictionary(&assets);
    let code = |module: &str| -> String {
        let src = assets.get(&format!("static/{module}")).unwrap_or_else(|| panic!("{module} shipped"));
        src.lines().filter(|l| !is_comment(l)).collect::<Vec<_>>().join("\n")
    };
    let keys = code("keys.js");
    let handler = function_body(&keys, "onKey");
    for needle in [
        "key === 'j'", "key === 'k'", "(key === 'ArrowDown' && here)", "(key === 'ArrowUp' && here)",
        "key === 'Enter' && here.acts.open", "key === 'r'", "key === 'a'", "key === '/'", "key === '?'",
    ] {
        assert!(handler.contains(needle), "the keyboard lost {needle}");
    }
    for guard in ["ev.ctrlKey", "ev.metaKey", "ev.altKey", "dialogOpen()", "typing(ev.target)"] {
        assert!(handler.contains(guard), "a shortcut fires despite {guard}");
    }
    let typing = function_body(&keys, "typing");
    assert!(typing.contains("isContentEditable") && typing.contains("'INPUT', 'TEXTAREA', 'SELECT'"), "typing is not recognised");
    // The sheet lists every shortcut, in words from the dictionary.
    let sheet = keys.split("export const SHORTCUTS = [").nth(1).and_then(|s| s.split("];").next()).expect("SHORTCUTS");
    for (name, key) in [("J", "keys.next"), ("K", "keys.prev"), ("Enter", "keys.open"), ("R", "keys.retry"), ("A", "keys.approve"), ("/", "keys.search"), ("?", "keys.sheet")] {
        assert!(sheet.contains(&format!("'{name}")) && sheet.contains(&format!("'{key}'")) && words.contains_key(key), "the sheet lacks {name}");
    }
    // The cards take part: review cards open and retry (never a file-locker link), held videos approve via the dialog.
    let mcr = code("mcr.js");
    assert!(mcr.contains("initKeys({"), "the desk never listens to the keyboard");
    let card = function_body(&mcr, "reviewCard");
    assert!(card.contains("return navCard(card, {") && card.contains("retry: locker ? null :"), "a review card is not on the keyboard");
    let approval = code("approval.js");
    assert!(approval.contains("navCard(node, {") && approval.contains("approve: () => { if (!approve.disabled) approve.click(); }"), "A does not go through the approve button (and its dialog)");
    assert!(code("inbox.js").contains("navCard(head)"), "J/K do not walk the mails");

    // Real buttons only.
    let desk_modules = [
        "desk.js", "mcr.js", "inbox.js", "approval.js", "notify.js", "user.js", "status.js", "worklist.js", "undo.js",
        "dialog.js", "toast.js", "keys.js", "text.js", "sound.js",
    ];
    for module in desk_modules {
        let src = code(module);
        assert!(!src.contains("role: 'button'"), "{module} makes a button out of something else");
        assert!(!src.contains("behavior: 'smooth'"), "{module} scrolls smoothly whatever the person asked for");
    }
    for page in ["mcr.html", "user.html"] {
        assert!(!assets.get(page).unwrap().contains("role=\"button\""), "{page} makes a button out of something else");
    }
    let inbox = code("inbox.js");
    assert!(function_body(&inbox, "chipNode").contains("el('button', { class: 'lnk', type: 'button'"), "a link in the text is not a button");
    assert!(function_body(&inbox, "jobCard").contains("el('div', { class: 'jcard', 'data-job'"), "a video's card is still a button holding buttons");
    // Pictures: buttons, from the keyboard too.
    assert!(!approval.contains("el('img', {") || !approval.split("el('img', {").skip(1).any(|s| s.split(')').next().unwrap_or("").contains("onClick")), "a picture is clicked but is not a button");
    assert!(function_body(&approval, "figure").contains("el('button', {"), "the article picture and frame are not buttons");
    let sheet_btn = function_body(&approval, "sheetButton");
    assert!(sheet_btn.contains("ev.key !== 'ArrowRight' && ev.key !== 'ArrowLeft'") && sheet_btn.contains("aria-label"), "the contact sheet does not work from the keyboard");

    // One focus ring; less motion when asked.
    let css = assets.get("static/status.css").unwrap();
    assert!(css.contains(":focus-visible { outline: 3px solid var(--accent)"), "no focus ring");
    let motion = css.split("@media (prefers-reduced-motion: reduce) {").nth(1).expect("no reduced-motion rule");
    assert!(motion.contains("animation-duration: 0.01ms !important") && motion.contains("transition-duration: 0.01ms !important"), "motion is not reduced");
    assert!(code("desk.js").contains("matchMedia('(prefers-reduced-motion: reduce)')"), "scrolling ignores the reduced-motion setting");
}

/// P12.9: sound. One volume, a switch per event (delivered, needs a person, health, held video),
/// a test button per sound, and a reminder repeated every N minutes while something has waited
/// longer than N minutes; all stored in this browser. Each event plays its own sound, a switched
/// off event stays quiet, and the old on/off choice («Σίγαση») still means silence.
#[test]
fn the_desk_sounds_are_chosen_per_browser() {
    let assets = all_assets();
    let words = desk_dictionary(&assets);
    let code = |module: &str| -> String {
        let src = assets.get(&format!("static/{module}")).unwrap_or_else(|| panic!("{module} shipped"));
        src.lines().filter(|l| !is_comment(l)).collect::<Vec<_>>().join("\n")
    };
    let sound = code("sound.js");
    let notify = code("notify.js");

    // The four events, each with a sound, a switch (on by default) and words.
    assert!(sound.contains("export const EVENTS = ['delivered', 'person', 'health', 'held'];"), "the events changed");
    let defaults = sound.split("const DEFAULTS = {").nth(1).and_then(|s| s.split("};").next()).expect("DEFAULTS");
    for event in ["delivered", "person", "health", "held"] {
        assert!(defaults.contains(&format!("  {event}: true,")), "the `{event}` sound is off by default");
        assert!(sound.contains(&format!("  {event}: (start) => {{")), "the `{event}` event has no sound");
        assert!(words.contains_key(&format!("sound.event.{event}")), "the `{event}` switch has no words");
    }
    assert!(sound.contains("  remind: (start) => {"), "the reminder has no sound");
    // A switched-off event, or the sound switched off, plays nothing.
    let play = function_body(&sound, "play");
    assert!(play.contains("if (!prefs.on) return false;"), "the sound plays while switched off");
    assert!(play.contains("kind === 'remind' ? !prefs.remind : !prefs[kind]"), "a switched-off event still plays");
    // Stored in this browser; a broken value is made safe; the old «Σίγαση» stays silent.
    assert!(sound.contains("localStorage.setItem(PREF_KEY, JSON.stringify(prefs))"), "the settings are not kept in this browser");
    assert!(sound.contains("sanitize({ on: localStorage.getItem(OLD_KEY) !== 'off' })"), "the old mute is forgotten");
    let sanitize = function_body(&sound, "sanitize");
    assert!(sanitize.contains("Math.min(100, Math.max(0, volume))"), "the volume is not kept within 0..100");
    assert!(sanitize.contains("Math.min(REMIND_MAX, Math.max(REMIND_MIN, mins))"), "the reminder's minutes are not bounded");
    // One volume, through one gain every note goes through.
    assert!(sound.contains("osc.connect(amp).connect(master);") && sound.matches("ctx.destination").count() == 1, "a note bypasses the volume");
    assert!(sound.contains("master.gain.value = gainOf(prefs.volume)"), "the volume is not applied");
    // The window: volume, a switch and a «Δοκιμή» per event, the reminder's minutes.
    let form = function_body(&sound, "settingsForm");
    assert!(form.contains("type: 'range'") && form.contains("type: 'number'"), "no volume or no minutes in the settings");
    assert!(form.contains("EVENTS.map((kind) => el('div', { class: 'snd-row' }, check(kind, t(`sound.event.${kind}`)), testButton(kind)))"), "an event has no switch or no test");
    assert!(form.contains("check('remind'") && form.contains("testButton('remind')"), "the reminder has no switch or no test");
    // «Δοκιμή» plays at the chosen volume even when the event is switched off.
    assert!(function_body(&sound, "testButton").contains("onClick: () => { unlock(); sound(kind); }"), "the test button obeys the switches");
    assert!(code("notify.js").contains("onClick: () => openSoundSettings(),"), "the header's sound button does not open the settings");

    // notify.js: every event plays its own sound, and has no audio of its own.
    assert!(!notify.contains("AudioContext") && !notify.contains("createOscillator"), "notify.js plays sound behind sound.js");
    let watch = function_body(&notify, "watchStatus");
    for needle in ["play('person')", "play('held')", "play('health')", "tn('notify.held', diff.newlyHeld)", "remind(data);"] {
        assert!(watch.contains(needle), "watchStatus lost {needle}");
    }
    assert!(function_body(&notify, "serviceDown").contains("play('health')"), "a silent outage");
    assert!(function_body(&notify, "announce").contains("play(allOk ? 'delivered' : 'person')"), "a finished mail plays the wrong sound");
    // A held video is its own news, not «needs a person».
    let diff = function_body(&notify, "diffStatus");
    assert!(diff.contains("personCount(next.queue) - personCount(prev.queue)") && diff.contains("next.queue.approval || 0) - (prev.queue.approval || 0)"), "held videos are not told apart");
    assert!(!function_body(&notify, "personCount").contains("approval"), "a held video counts as needing a person");

    // The reminder: due after N minutes of waiting, then again only every N minutes.
    let due = function_body(&notify, "remindDue");
    assert!(due.contains("if (oldestAt === null || !Number.isFinite(oldestAt)) return false;"), "an unknown wait reminds");
    assert!(due.contains("return now - oldestAt >= span && (!last || now - last >= span);"), "the reminder does not wait N minutes, or repeats faster");
    let remind = function_body(&notify, "remind");
    assert!(remind.contains("if (needCount(q) === 0) { lastRemind = 0; return; }"), "the reminder does not start afresh when nothing waits");
    assert!(remind.contains("if (!p.on || !p.remind || reminding) return;"), "the reminder ignores its switch");
    assert!(remind.contains("now - lastRemindCheck < REMIND_CHECK_MS"), "the lists are fetched on every status poll");
    let oldest = function_body(&notify, "oldestWaitAt");
    assert!(oldest.contains("view=review") && oldest.contains("view=approval"), "the reminder does not read both waiting lists");
    assert!(oldest.contains("Number.isFinite(at) && (oldest === null || at < oldest)"), "a missing time is taken as some time");
    assert!(words["notify.remind.other"].contains("{n} λεπτά"), "the reminder does not say how long");
}

/// P12.10: only desk work on the desk. Four tabs; adding a link is a side panel opened by a
/// button; the broadcast specification and the shortcuts are in a «Βοήθεια» menu; the journalists
/// are not edited on the desk; every word of mcr.html and user.html comes from the dictionary; and
/// «Να το θυμάμαι για αυτή τη διεύθυνση» appears only when the server says the sender is not on
/// the roster (a server without `sender_on_roster` shows no button).
#[test]
fn only_desk_work_on_the_desk() {
    let assets = all_assets();
    let words = desk_dictionary(&assets);
    let greek = |s: &str| s.chars().any(|c| ('\u{0370}'..='\u{03FF}').contains(&c));
    let code = |module: &str| -> String {
        let src = assets.get(&format!("static/{module}")).unwrap_or_else(|| panic!("{module} shipped"));
        src.lines().filter(|l| !is_comment(l)).collect::<Vec<_>>().join("\n")
    };
    // Markup without its comments (they may span lines).
    let markup = |page: &str| -> String {
        let html = assets.get(page).unwrap_or_else(|| panic!("{page} shipped"));
        let mut out = String::new();
        let mut rest = html.as_str();
        while let Some(at) = rest.find("<!--") {
            out.push_str(&rest[..at]);
            rest = rest[at..].split_once("-->").map(|(_, r)| r).unwrap_or("");
        }
        out.push_str(rest);
        out
    };

    // Every word of both pages by key, and every key in the dictionary.
    for page in ["mcr.html", "user.html"] {
        let html = markup(page);
        for line in html.lines() {
            assert!(!greek(line), "{page} keeps Greek text of its own: {}", line.trim());
        }
        for attr in ["data-t=\"", "data-t-aria-label=\"", "data-t-placeholder=\"", "data-t-title=\""] {
            for key in html.split(attr).skip(1).filter_map(|s| s.split('"').next()) {
                assert!(words.contains_key(key), "{page} asks for `{key}`, which text.el.js does not have");
            }
        }
    }
    assert!(code("user.js").contains("\napplyText();"), "user.html's words are never filled");

    // Four tabs; no journalists and no add-a-link tab; no specification tab or modal.
    let page = markup("mcr.html");
    let tabs: Vec<&str> = page.split("data-tab=\"").skip(1).filter_map(|s| s.split('"').next()).collect();
    assert_eq!(tabs, ["email", "queue", "review", "completed"], "the desk's tabs");
    let mcr = code("mcr.js");
    assert!(mcr.contains("const TABS = ['email', 'queue', 'review', 'completed'];"), "mcr.js still knows other tabs");
    for gone in ["tab-journalists", "journalist-form", "journalists-body", "tab-manual", "specs-modal", "specs-btn"] {
        assert!(!page.contains(gone), "mcr.html still has {gone}");
        assert!(!mcr.contains(gone), "mcr.js still uses {gone}");
    }
    // The journalists are edited on the administration page, not here.
    assert!(!mcr.contains("api('/api/journalists', {") && !mcr.contains("/api/journalists/${"), "the desk still edits the journalists");

    // «+ Προσθήκη συνδέσμου»: a button that opens the side panel holding the form.
    assert!(page.contains("id=\"add-btn\" aria-expanded=\"false\" aria-controls=\"add-panel\""), "no add button for the panel");
    let panel = page.split("<aside id=\"add-panel\"").nth(1).and_then(|s| s.split("</aside>").next()).expect("the add panel");
    assert!(panel.contains("id=\"manual-form\"") && panel.contains("id=\"add-close\""), "the form or its close is not in the panel");
    assert!(function_body(&mcr, "openAdd").contains("document.getElementById('m-url').focus();"), "opening the panel leaves the focus behind");
    let close = function_body(&mcr, "closeAdd");
    assert!(close.contains("addPanel.hidden = true;") && close.contains("if (hadFocus) addButton.focus();"), "closing the panel loses the focus");
    assert!(mcr.contains("addPanel.addEventListener('keydown', (ev) => {\n  if (ev.key !== 'Escape') return;"), "Escape does not close the panel");
    let submit = mcr.split("getElementById('manual-form').addEventListener('submit'").nth(1).and_then(|s| s.split("\n});").next()).expect("submit");
    assert!(submit.contains("closeAdd();\n    switchTab('queue');"), "after adding, the panel stays and the queue is not shown");

    // «Βοήθεια»: a menu button with the specification and the shortcuts.
    assert!(page.contains("id=\"help-btn\" aria-haspopup=\"menu\" aria-expanded=\"false\" aria-controls=\"help-menu\""), "no help menu button");
    let menu = page.split("id=\"help-menu\"").nth(1).and_then(|s| s.split("</div>").next()).expect("the help menu");
    assert!(menu.contains("role=\"menu\""), "the help menu is not a menu");
    for item in ["id=\"help-specs\"", "id=\"help-keys\""] {
        assert!(menu.contains(item), "the help menu lacks {item}");
    }
    assert!(menu.matches("role=\"menuitem\"").count() == 2, "the help menu's items are not menu items");
    for key in ["'ArrowDown'", "'ArrowUp'", "'Home'", "'End'", "'Escape'", "'Tab'"] {
        assert!(mcr.contains(&format!("ev.key === {key}")), "the help menu ignores {key}");
    }
    let rows = mcr.split("const SPEC_ROWS = [").nth(1).and_then(|s| s.split("];").next()).expect("SPEC_ROWS");
    for row in ["video", "bitrate", "size", "scan", "chroma", "audio_layout", "audio", "channels", "wrap"] {
        assert!(rows.contains(&format!("'{row}'")), "the specification lost its `{row}` line");
        assert!(words.contains_key(&format!("spec.{row}")) && words.contains_key(&format!("spec.{row}.value")), "no words for `{row}`");
    }
    assert!(code("keys.js").contains("export function showShortcuts()"), "the shortcuts cannot be opened from the menu");

    // «Να το θυμάμαι για αυτή τη διεύθυνση».
    let inbox = code("inbox.js");
    let offer = function_body(&inbox, "rememberOffer");
    assert!(offer.contains("if (d.sender_on_roster !== false) return null;"), "the button shows without the server saying the sender is unknown");
    assert!(offer.contains("d.kind === 'manual'") && offer.contains("d.outcome === 'FAILED'"), "a link added by hand or a failed mail offers it");
    assert!(offer.contains("d.journalist === 'MCR'") && offer.contains("!d.from_address"), "it is offered for MCR or without an address");
    assert!(offer.contains("return { surname: d.journalist, address: d.from_address };"), "the offer names the wrong journalist or address");
    let remember = function_body(&inbox, "rememberSender");
    let ask = remember.find("await confirmDialog({").expect("it does not ask first");
    let post = remember.find("api('/api/mails/remember-sender', { method: 'POST', body: { key, surname: offer.surname } })").expect("the contract's request");
    assert!(ask < post, "it remembers before asking");
    assert!(remember.contains("e.code === 'ADDRESS_TAKEN' ? t('mail.remember.taken', offer)"), "an address of another journalist is not explained");
    assert!(remember.contains("r.already ? 'mail.remember.already' : 'mail.remember.done'"), "an address already known is announced as new");
    assert!(function_body(&inbox, "metaNode").contains("const offer = rememberOffer(d);"), "the open mail never offers it");
    assert_eq!(words["mail.remember.button"], "Να το θυμάμαι για αυτή τη διεύθυνση");
}

/// P12.11: no page builds a file name itself. Every preview (the add panel, /user, the rename)
/// asks `/api/slug-preview`, debounced, dropping an answer to older fields; the article offers
/// show the name the server composed for them.
#[test]
fn every_file_name_preview_comes_from_the_server() {
    let assets = all_assets();
    let code = |module: &str| -> String {
        let src = assets.get(&format!("static/{module}")).unwrap_or_else(|| panic!("{module} shipped"));
        src.lines().filter(|l| !is_comment(l)).collect::<Vec<_>>().join("\n")
    };
    // Nothing joins the parts in the page: `${…}_${…}_${…}` is how a page guessed a name.
    for module in ["desk.js", "mcr.js", "inbox.js", "approval.js", "user.js"] {
        let src = code(module);
        assert!(!src.contains("}_${"), "{module} builds a file name of its own");
        assert!(!src.contains(".toUpperCase()}"), "{module} guesses the upper-case form of a name");
    }
    let desk = code("desk.js");
    assert!(function_body(&desk, "slugPreview").contains("api(`/api/slug-preview?${params}`)"), "the desk's preview is not the server's");
    let mcr = code("mcr.js");
    let manual = function_body(&mcr, "showManualPreview");
    assert!(manual.contains("await slugPreview({ index, journalist, keyword })"), "the add panel's preview is not the server's");
    assert!(manual.contains("setTimeout(") && manual.contains("if (mine === manualAsked)"), "the add panel's preview is not debounced or keeps stale answers");
    assert!(mcr.contains("(job.offer_slugs || {})[offer.index_str]"), "the offers do not show the composed name");
    let user = code("user.js");
    let preview = function_body(&user, "showSlugPreview");
    assert!(preview.contains("api(`/api/slug-preview?${params}`)") && preview.contains("if (mine !== previewAsked) return;"), "/user has no server preview");
    assert!(preview.contains("index: many ? `${index}A` : index"), "/user's preview of several links is not the first one's name");
    assert!(assets.get("user.html").unwrap().contains("id=\"slug-preview\" aria-live=\"polite\""), "/user has no place for the preview");
}

#[test]
fn the_user_panel_posts_to_its_own_endpoint_and_is_imported_at_v16() {
    let assets = all_assets();
    let page = assets.get("user.html").expect("user.html shipped");
    assert!(page.contains("/static/user.js?v=16"), "user.html does not import user.js?v=16");
    assert!(!page.contains("/static/user.js?v=15"), "user.html still imports the old user.js");

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
fn the_first_run_password_card_ships_with_admin_v30_and_login_v6() {
    let assets = all_assets();
    let admin = assets.get("admin.html").expect("admin.html shipped");
    assert!(admin.contains("/static/admin.js?v=30"), "admin.html does not import admin.js?v=30");
    assert!(!admin.contains("/static/admin.js?v=29"), "admin.html still imports the old admin.js");
    let login = assets.get("login.html").expect("login.html shipped");
    assert!(login.contains("/static/login.js?v=6"), "login.html does not import login.js?v=6");
    assert!(!login.contains("/static/login.js?v=5"), "login.html still imports the old login.js");

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
    assert_eq!(versions, ["4".to_string()].into(), "wizard.js is imported under different versions");
    let admin = assets.get("static/admin.js").expect("admin.js shipped");
    assert!(admin.contains("runWizard"), "admin.js never starts the wizard");
    let page = assets.get("admin.html").expect("admin.html shipped");
    assert!(page.contains("id=\"setup-rerun\""), "the Setup card is missing");
}
