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
        ("desk.js", 18),
        ("inbox.js", 25),
        ("approval.js", 13),
        ("notify.js", 13),
        ("mcr.js", 51),
        ("status.js", 1),
        ("status.css", 5),
        ("worklist.js", 1),
        ("undo.js", 2),
        ("dialog.js", 1),
        ("user.js", 11),
        ("toast.js", 1),
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
    // A one-line table row, `NAME: { key: '…', … }`, and one of its fields.
    let row = |block: &str, name: &str| -> String {
        let head = format!("{name}: {{");
        block
            .lines()
            .find(|l| l.trim_start().starts_with(&head))
            .unwrap_or_else(|| panic!("status.js has no row for {name}"))
            .to_string()
    };
    let field = |row: &str, key: &str| -> String {
        let pat = format!("{key}: '");
        row.split(pat.as_str()).nth(1).and_then(|s| s.split('\'').next()).unwrap_or("").to_string()
    };
    let tones = js
        .split("export const TONES = {")
        .nth(1)
        .and_then(|s| s.split("};").next())
        .expect("status.js has a TONES table");
    let statuses = js
        .split("export const STATUS = {")
        .nth(1)
        .and_then(|s| s.split("};").next())
        .expect("status.js has a STATUS table");

    for status in JobStatus::ALL {
        let name = status.as_str();
        let r = row(statuses, name);
        for key in ["label", "short", "phrase", "user"] {
            assert!(!field(&r, key).is_empty(), "{name} has no `{key}` text in status.js");
        }
        let tone = field(&r, "tone");
        let t = row(tones, &tone);
        assert!(!field(&t, "mark").is_empty(), "tone {tone} ({name}) has no mark beside its colour");
        for selector in [format!(".badge.{tone} "), format!(".tag.{tone} "), format!(".legend i.{tone} ")] {
            assert!(css.contains(&selector), "tone {tone} ({name}) has no `{selector}` colour rule");
        }
    }
    let tone_of = |name: &str| field(&row(statuses, name), "tone");
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

    // The groups, in order, after the approval section.
    let groups = wl.split("export const GROUPS = [").nth(1).and_then(|s| s.split("];").next()).expect("GROUPS");
    let at = |label: &str| groups.find(label).unwrap_or_else(|| panic!("no group «{label}»"));
    let order = ["Χρειάζεται σύνδεση (cookies)", "Δεν βρέθηκε βίντεο", "Χειροκίνητη λήψη", "Άλλο"].map(at);
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
    assert!(why.contains("αυτόματη επανάληψη") && why.contains("STOPPED_TEXT"), "a stopped job can say it retries by itself");

    // mcr.js: the list is grouped, still reconciled in place (P12.3), with the panels kept open.
    assert!(mcr.contains("const jobs = inGroupOrder(visible('review', data.jobs || []));"), "the review list is not grouped");
    assert!(mcr.contains("heading: (job) => groupLabel(groupOf(job)) || null"), "the headings are not the groups");
    assert!(!mcr.contains("dayHeading"), "the review list is still grouped by date");
    let card = mcr.split("function reviewCard(job)").nth(1).and_then(|s| s.split("\n}\n").next()).expect("reviewCard");
    assert!(card.contains("whyText(job)") && !card.contains("job.hint"), "the card does not use the one why-sentence");
    assert!(card.contains("primaryOf(job)"), "the card has no primary action");
    assert!(card.contains("keptDetails(moreOpen, job.id, 'wl-more', 'Περισσότερα'"), "no «Περισσότερα» that stays open");
    let tech = card
        .split("keptDetails(techOpen, job.id, 'tech', 'Τεχνικές λεπτομέρειες',")
        .nth(1)
        .and_then(|s| s.split("    : null;").next())
        .expect("no «Τεχνικές λεπτομέρειες» that stays open on the card");
    for (shown, field) in [("Κωδικός:", "job.error_code"), ("Αιτία:", "job.error_message")] {
        assert!(tech.contains(shown) && tech.contains(field), "«Τεχνικές λεπτομέρειες» no longer shows {shown}");
        assert_eq!(card.matches(shown).count(), 1, "{shown} is shown outside «Τεχνικές λεπτομέρειες»");
    }

    // Every button a hint names by «…» is on the card (or its history panel).
    let history = mcr.split("function historyPanel(job)").nth(1).and_then(|s| s.split("\n}\n").next()).expect("historyPanel");
    let panel = format!("{history}\n{card}");
    let quoted = |text: &str| -> Vec<String> {
        text.split('«').skip(1).filter_map(|s| s.split('»').next()).map(str::to_string).collect()
    };
    let mut hints: Vec<String> = ErrorCode::ALL.iter().map(|c| c.hint_el().to_string()).collect();
    hints.push(omni_broadcast::errors::exhausted_hint_el(3));
    hints.push(omni_broadcast::errors::LIVE_STREAM_EXHAUSTED_EL.to_string());
    hints.extend(wl.lines().filter(|l| l.starts_with("const LOCKER_TEXT") || l.starts_with("const STOPPED_TEXT")).map(str::to_string));
    for hint in &hints {
        for name in quoted(hint) {
            if name.starts_with('-') {
                continue; // an example of a link's tail, not a button
            }
            assert!(panel.contains(&name), "a hint names «{name}», which the review card does not have:\n{hint}");
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
        "status.js", "worklist.js", "undo.js", "dialog.js", "toast.js",
    ];
    for module in desk_modules {
        let src = code(module);
        for native in ["confirm", "prompt", "alert"] {
            assert!(!calls(&src, native), "{module} still calls the browser's {native}()");
        }
    }

    // Reversible: at once, with «Αναίρεση» for 8 seconds.
    let undo = code("undo.js");
    assert!(undo.contains("export const UNDO_MS = 8000;") && undo.contains("'Αναίρεση'"), "no 8 s «Αναίρεση»");
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
    assert!(approval.contains("await confirmDialog({") && approval.contains("confirm: 'Έγκριση → Dalet'"), "approve does not ask in the dialog");
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

    // Rename is a field on the card with a live preview, from the server's own keyword rule.
    let rename = function_body(&desk, "renameForm");
    assert!(rename.contains("/api/keyword-preview?text="), "the rename field has no live preview");
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
    assert!(summary.contains("'Όλα καλά'") && summary.contains("προβλήματα"), "the pill does not say «Όλα καλά» / «N προβλήματα»");
    assert!(mcr.contains("paintHealth(data)") && mcr.contains("'Φάκελος παράδοσης (Dalet)'"), "the panel has no free space");

    // No static inline styles: only a progress width, a CSS variable and a computed scroll margin.
    for module in [
        "desk.js", "mcr.js", "inbox.js", "approval.js", "notify.js", "user.js", "status.js", "worklist.js", "undo.js", "dialog.js",
        "toast.js",
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
    let table = notify.split("export const CHECK_INFO = {").nth(1).and_then(|s| s.split("\n};").next()).expect("CHECK_INFO");
    let field = |row: &str, key: &str| -> String {
        let pat = format!("{key}: '");
        row.split(pat.as_str()).nth(1).and_then(|s| s.split('\'').next()).unwrap_or("").to_string()
    };
    for name in names {
        let head = format!("  {name}: {{");
        let row = table.lines().find(|l| l.starts_with(&head)).unwrap_or_else(|| panic!("no Greek texts for the `{name}` check"));
        assert!(!field(row, "name").is_empty(), "`{name}` has no name");
        for key in ["degraded", "down", "means", "todo"] {
            let text = field(row, key);
            assert!(greek(&text), "`{name}` has no Greek `{key}`: {text:?}");
        }
        let who = field(row, "who");
        assert!(who == "mcr" || who == "admin", "`{name}` does not say who acts: {who:?}");
    }

    // The banner and the panel's lines: the three texts, never the tool's detail.
    let texts = function_body(mcr, "healthTextNodes");
    for needle in ["t.happened", "t.means", "t.todo", "WHO[t.who]", "'Για τα βίντεο: '", "'Τι κάνετε: '"] {
        assert!(texts.contains(needle), "a health line lost {needle}");
    }
    assert!(!texts.contains(".detail"), "a health line shows the tool's own detail");
    let banner = mcr
        .split("const problems = healthProblems(checks);")
        .nth(1)
        .and_then(|s| s.split("render(banner,").nth(1))
        .and_then(|s| s.split(";\n").next())
        .expect("the health banner");
    assert!(banner.contains("healthTextNodes(") && !banner.contains("detail"), "the banner shows the tool's detail:\n{banner}");
    assert!(mcr.contains("'Τεχνικές λεπτομέρειες'"), "the panel has no place for the tool's detail");
    // A site is a sentence, from the payload's `sites` rows.
    assert!(texts.contains("name === 'sites'") && texts.contains("map(siteSentence)"), "the sites line is not told per site");
    let site = function_body(notify, "siteSentence");
    for needle in ["Ο ιστότοπος ${site.domain}", "από τις ${at}", "αποτυχίες στη σειρά", "'δεν δίνει βίντεο'"] {
        assert!(site.contains(needle), "the site sentence lost {needle}");
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

#[test]
fn the_user_panel_posts_to_its_own_endpoint_and_is_imported_at_v11() {
    let assets = all_assets();
    let page = assets.get("user.html").expect("user.html shipped");
    assert!(page.contains("/static/user.js?v=11"), "user.html does not import user.js?v=11");
    assert!(!page.contains("/static/user.js?v=10"), "user.html still imports the old user.js");

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
