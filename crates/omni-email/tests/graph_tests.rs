//! Graph mail source against a mock of the Graph and login endpoints
//! (plan P4.2, P4.8). No test reaches the real tenant; this is where the
//! client is exercised end to end.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::{json, Value};

use omni_core::config::GraphConfig;
use omni_email::graph::{GraphMailSource, RetryAfter};
use chrono::{TimeZone, Utc};
use omni_email::source::{MailGone, MailOutcome, MailSource};

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    query: String,
    auth: String,
    prefer: String,
    body: String,
}

#[derive(Default)]
struct Mock {
    seen: Vec<Seen>,
    tokens_issued: u32,
    /// Answer the next Graph call with 401 (an expired / revoked token).
    reject_next: bool,
    /// Answer every Graph call with 429.
    throttle: bool,
    /// Answer every Graph call with 403 (no Mail.Read, or outside the
    /// application access policy).
    forbid: bool,
    /// Returned as `@odata.nextLink` on the first listing page.
    next_link: Option<String>,
    /// displayName → id, keyed by parent id ("" = mailbox root).
    folders: Vec<(String, String, String)>,
}

type Shared = Arc<Mutex<Mock>>;

fn json_resp(v: Value) -> Response {
    ([("content-type", "application/json")], v.to_string()).into_response()
}

async fn handler(State(mock): State<Shared>, req: Request) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let (auth, prefer) = {
        let header = |n: &str| req.headers().get(n).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        (header("authorization"), header("prefer"))
    };
    let body = String::from_utf8(to_bytes(req.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
    let query_decoded = percent(&query);

    let mut m = mock.lock().unwrap();
    m.seen.push(Seen {
        method: method.to_string(),
        path: path.clone(),
        query: query_decoded.clone(),
        auth: auth.clone(),
        prefer,
        body: body.clone(),
    });

    if path.ends_with("/oauth2/v2.0/token") {
        assert!(body.contains("grant_type=client_credentials"), "{body}");
        if !body.contains("client_secret=s3cret") {
            return (StatusCode::UNAUTHORIZED, r#"{"error":"invalid_client","error_description":"AADSTS7000215: Invalid client secret provided."}"#).into_response();
        }
        m.tokens_issued += 1;
        return json_resp(json!({ "access_token": format!("tok-{}", m.tokens_issued), "expires_in": 3600 }));
    }

    if m.throttle {
        return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "7")], "").into_response();
    }
    if m.forbid {
        return (StatusCode::FORBIDDEN, r#"{"error":{"code":"ErrorAccessDenied","message":"Access is denied."}}"#).into_response();
    }
    if m.reject_next {
        m.reject_next = false;
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !auth.starts_with("Bearer tok-") {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let base = "/v1.0/users/ingest@example.gr";
    let rest = path.strip_prefix(base).unwrap_or_else(|| panic!("unexpected path {path}"));
    match (method, rest) {
        (Method::GET, "/mailFolders/Inbox/messages") if query_decoded.contains("$top=1&") => {
            json_resp(json!({ "value": [ { "id": "AAMk-1" } ] }))
        }
        (Method::GET, "/mailFolders/Inbox/messages") if query_decoded.contains("$skiptoken=") => json_resp(json!({ "value": [
            { "id": "AAMk-3", "internetMessageId": "<g3@example.gr>", "subject": "Τρίτο",
              "receivedDateTime": "2026-09-24T08:10:00Z", "lastModifiedDateTime": "2026-09-24T08:10:00Z" }
        ]})),
        (Method::GET, "/mailFolders/Inbox/messages") => {
            let mut page = json!({ "value": [
                { "id": "AAMk-1", "internetMessageId": "<g1@example.gr>", "subject": "ΘΕΜΑΤΑ",
                  "from": { "emailAddress": { "name": "Anna Papadaki", "address": "A.Papadaki@example.gr" } },
                  "receivedDateTime": "2026-09-24T08:00:00Z", "lastModifiedDateTime": "2026-09-24T08:00:30Z" },
                { "id": "AAMk-2", "internetMessageId": "<g2@example.gr>", "subject": "Βίντεο",
                  "receivedDateTime": "2026-09-22T08:05:00Z", "lastModifiedDateTime": "2026-09-24T08:05:00Z" }
            ]});
            if let Some(link) = &m.next_link {
                page["@odata.nextLink"] = json!(link);
            }
            json_resp(page)
        }
        (Method::GET, "/messages/AAMk-1") => json_resp(json!({
            "id": "AAMk-1", "internetMessageId": "<g1@example.gr>", "subject": "ΘΕΜΑΤΑ",
            "from": { "emailAddress": { "name": "Anna Papadaki", "address": "A.Papadaki@example.gr" } },
            "toRecipients": [ { "emailAddress": { "address": "ingest@example.gr" } } ],
            "ccRecipients": [],
            "receivedDateTime": "2026-09-24T08:00:00Z",
            "lastModifiedDateTime": "2026-09-24T08:00:30Z",
            "body": { "contentType": "text", "content": "1. ΤΕΣΤ\nhttps://youtu.be/g0001" },
            "hasAttachments": false
        })),
        (Method::GET, "/messages/AAMk-2") => json_resp(json!({
            "id": "AAMk-2", "internetMessageId": "<g2@example.gr>", "subject": "Βίντεο",
            "from": { "emailAddress": { "name": "Kostas", "address": "k.dimitriou@example.gr" } },
            "receivedDateTime": "2026-09-22T08:05:00Z",
            "body": { "contentType": "text", "content": "" },
            "hasAttachments": true
        })),
        (Method::GET, "/messages/AAMk-gone") => (
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":"ErrorItemNotFound","message":"The specified object was not found in the store."}}"#,
        )
            .into_response(),
        (Method::GET, "/messages/AAMk-2/attachments") => json_resp(json!({ "value": [
            { "id": "att-1", "name": "limani.mp4", "contentType": "video/mp4", "size": 11, "isInline": false },
            { "id": "att-logo", "name": "logo.png", "contentType": "image/png", "size": 900, "isInline": true }
        ]})),
        (Method::GET, "/messages/AAMk-2/attachments/att-1/$value") => "video-bytes".into_response(),
        (Method::PATCH, r) if r.starts_with("/messages/") => json_resp(json!({})),
        (Method::POST, r) if r.ends_with("/move") => json_resp(json!({ "id": "moved" })),
        (Method::POST, r) if r.ends_with("/reply") => StatusCode::ACCEPTED.into_response(),
        (Method::GET, "/mailFolders/Inbox") => json_resp(json!({ "id": "inbox", "displayName": "Inbox", "unreadItemCount": 4 })),
        (Method::GET, r) if r == "/mailFolders" || r.ends_with("/childFolders") => {
            let parent = r.strip_prefix("/mailFolders/").map(|p| p.trim_end_matches("/childFolders")).unwrap_or("");
            let name = query_decoded.split("displayName eq '").nth(1).and_then(|s| s.split('\'').next()).unwrap_or("");
            let found: Vec<Value> = m
                .folders
                .iter()
                .filter(|(p, n, _)| p == parent && n == name)
                .map(|(_, n, id)| json!({ "id": id, "displayName": n }))
                .collect();
            json_resp(json!({ "value": found }))
        }
        (Method::POST, r) if r == "/mailFolders" || r.ends_with("/childFolders") => {
            let parent = r.strip_prefix("/mailFolders/").map(|p| p.trim_end_matches("/childFolders")).unwrap_or("").to_string();
            let name = serde_json::from_str::<Value>(&body).unwrap()["displayName"].as_str().unwrap().to_string();
            let id = format!("folder-{}", name.to_lowercase());
            m.folders.push((parent, name.clone(), id.clone()));
            (StatusCode::CREATED, json_resp(json!({ "id": id, "displayName": name }))).into_response()
        }
        (method, r) => panic!("mock has no route for {method} {r}"),
    }
}

fn percent(s: &str) -> String {
    let s = s.replace('+', " ");
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn start() -> (Shared, SocketAddr) {
    let mock: Shared = Arc::default();
    let app = Router::new().fallback(handler).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app.into_make_service()).await.unwrap() });
    (mock, addr)
}

fn source(addr: SocketAddr, secret: &str) -> GraphMailSource {
    source_with(addr, secret, false)
}

fn source_with(addr: SocketAddr, secret: &str, write_access: bool) -> GraphMailSource {
    let cfg = GraphConfig {
        tenant_id: "tenant-1".into(),
        client_id: "client-1".into(),
        mailbox: "ingest@example.gr".into(),
        client_secret: secret.into(),
        write_access,
        ..Default::default()
    };
    let base = format!("http://{addr}");
    GraphMailSource::with_endpoints(cfg, &base, &base)
}

fn graph_calls(m: &Mock) -> Vec<Seen> {
    m.seen.iter().filter(|s| !s.path.ends_with("/token")).cloned().collect()
}

fn since() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 24, 7, 45, 0).unwrap()
}

/// Regression (P4.8): the P4.2 listing filtered on `isRead eq false`. With
/// Mail.Read nothing can be marked read, so processed mail stayed in that
/// filter forever and, a page later, hid all new mail.
#[tokio::test]
async fn the_listing_filters_on_change_time_never_on_read_state() {
    let (mock, addr) = start().await;
    let src = source(addr, "s3cret");
    let headers = src.list_changed(since(), 500).await.unwrap();

    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0].id, "AAMk-1");
    assert_eq!(headers[0].internet_message_id, "<g1@example.gr>");
    assert_eq!(headers[0].from_address, "a.papadaki@example.gr");
    assert_eq!(headers[0].modified_at, Utc.with_ymd_and_hms(2026, 9, 24, 8, 0, 30).unwrap());
    // Received two days before it changed: moved into the inbox later.
    assert!(headers[1].received_at.unwrap() < headers[1].modified_at);

    let m = mock.lock().unwrap();
    let list = &graph_calls(&m)[0];
    assert!(!list.query.contains("isRead"), "{}", list.query);
    assert!(list.query.contains("$filter=lastModifiedDateTime ge 2026-09-24T07:45:00Z"), "{}", list.query);
    assert!(list.query.contains("$orderby=lastModifiedDateTime asc"), "{}", list.query);
    // Headers only: no bodies in a listing that re-reads the overlap.
    assert!(!list.query.contains("body"), "{}", list.query);
}

#[tokio::test]
async fn the_listing_follows_paging_links_on_the_graph_host_only() {
    let (mock, addr) = start().await;
    let src = source(addr, "s3cret");
    mock.lock().unwrap().next_link =
        Some(format!("http://{addr}/v1.0/users/ingest@example.gr/mailFolders/Inbox/messages?$skiptoken=2"));
    let ids: Vec<String> = src.list_changed(since(), 500).await.unwrap().into_iter().map(|h| h.id).collect();
    assert_eq!(ids, vec!["AAMk-1", "AAMk-2", "AAMk-3"]);

    // `max` stops the paging.
    assert_eq!(src.list_changed(since(), 2).await.unwrap().len(), 2);

    // The bearer token goes with the link, so another host is refused.
    mock.lock().unwrap().next_link = Some("https://attacker.example/v1.0/steal?$skiptoken=2".into());
    let err = src.list_changed(since(), 500).await.unwrap_err().to_string();
    assert!(err.contains("another host"), "{err}");
    let m = mock.lock().unwrap();
    assert!(m.seen.iter().all(|s| !s.path.contains("steal")));
}

#[tokio::test]
async fn fetch_maps_one_message_with_its_attachments_and_takes_the_body_as_sent() {
    let (mock, addr) = start().await;
    let src = source(addr, "s3cret");

    let m1 = src.fetch_mail("AAMk-1").await.unwrap();
    assert_eq!(m1.internet_message_id, "<g1@example.gr>");
    assert_eq!(m1.from_address, "a.papadaki@example.gr");
    assert_eq!(m1.to, vec!["ingest@example.gr"]);
    assert_eq!(m1.body_text, "1. ΤΕΣΤ\nhttps://youtu.be/g0001");
    assert!(m1.received_at.is_some());

    // Inline images (signature logos) are not attachments worth parsing.
    let m2 = src.fetch_mail("AAMk-2").await.unwrap();
    assert_eq!(m2.attachments.len(), 1);
    assert_eq!(m2.attachments[0].name, "limani.mp4");

    // The body as sent (HTML): our converter, not Graph's, reads it (P4.10).
    let m = mock.lock().unwrap();
    let get = graph_calls(&m).into_iter().find(|c| c.path.ends_with("/messages/AAMk-1")).unwrap();
    assert!(!get.prefer.contains("body-content-type"), "{}", get.prefer);
}

#[tokio::test]
async fn a_message_that_vanished_is_reported_as_gone() {
    let (_mock, addr) = start().await;
    let err = source(addr, "s3cret").fetch_mail("AAMk-gone").await.unwrap_err();
    assert!(err.chain().any(|c| c.is::<MailGone>()), "{err:#}");
}

#[tokio::test]
async fn with_mail_read_only_nothing_in_the_mailbox_is_written() {
    let (mock, addr) = start().await;
    let src = source(addr, "s3cret");
    src.list_changed(since(), 500).await.unwrap();
    src.fetch_mail("AAMk-1").await.unwrap();
    src.mark_processed("AAMk-1", MailOutcome::Processed).await.unwrap();
    src.mark_processed("AAMk-2", MailOutcome::Failed).await.unwrap();
    src.test_access().await.unwrap();
    src.health().await;

    let m = mock.lock().unwrap();
    let writes: Vec<Seen> = graph_calls(&m).into_iter().filter(|c| c.method != "GET").collect();
    assert!(writes.is_empty(), "a Mail.Read app would get 403 for these: {writes:?}");
}

#[tokio::test]
async fn the_token_is_cached_and_refreshed_once_on_401() {
    let (mock, addr) = start().await;
    let src = source(addr, "s3cret");
    src.list_changed(since(), 5).await.unwrap();
    src.list_changed(since(), 5).await.unwrap();
    assert_eq!(mock.lock().unwrap().tokens_issued, 1, "token not cached");

    mock.lock().unwrap().reject_next = true;
    src.list_changed(since(), 5).await.unwrap();
    let m = mock.lock().unwrap();
    assert_eq!(m.tokens_issued, 2, "401 did not refresh the token");
    assert_eq!(graph_calls(&m).last().unwrap().auth, "Bearer tok-2");
}

#[tokio::test]
async fn a_bad_secret_is_reported_without_echoing_it() {
    let (_mock, addr) = start().await;
    let err = source(addr, "wrong-secret-value").list_changed(since(), 5).await.unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("Graph login failed"), "{text}");
    assert!(text.contains("AADSTS7000215"), "{text}");
    assert!(!text.contains("wrong-secret-value"), "secret leaked into the error: {text}");
    assert!(!format!("{:?}", source(addr, "wrong-secret-value")).contains("wrong-secret-value"));
}

#[tokio::test]
async fn with_write_access_processed_mail_is_read_and_moved_failed_mail_moved_but_left_unread() {
    let (mock, addr) = start().await;
    let src = source_with(addr, "s3cret", true);

    src.mark_processed("AAMk-1", MailOutcome::Processed).await.unwrap();
    src.mark_processed("AAMk-9", MailOutcome::Failed).await.unwrap();
    // Folder ids are cached: a second processed mail creates nothing.
    src.mark_processed("AAMk-3", MailOutcome::Processed).await.unwrap();

    let m = mock.lock().unwrap();
    let calls = graph_calls(&m);
    let patches: Vec<&Seen> = calls.iter().filter(|s| s.method == "PATCH").collect();
    assert_eq!(patches.len(), 2, "only processed mail is marked read");
    assert!(patches.iter().all(|p| p.body.contains("\"isRead\":true")));
    assert!(!patches.iter().any(|p| p.path.ends_with("AAMk-9")), "failed mail must stay unread");

    let moves: Vec<&Seen> = calls.iter().filter(|s| s.path.ends_with("/move")).collect();
    assert_eq!(moves.len(), 3);
    assert!(moves[0].body.contains("folder-processed"));
    assert!(moves[1].body.contains("folder-failed"));
    assert!(moves[2].body.contains("folder-processed"));

    // Omni created once, Processed and Failed once each, under Omni.
    let mut names: Vec<(String, String)> = m.folders.iter().map(|(p, n, _)| (p.clone(), n.clone())).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            ("".to_string(), "Omni".to_string()),
            ("folder-omni".to_string(), "Failed".to_string()),
            ("folder-omni".to_string(), "Processed".to_string()),
        ]
    );
}

#[tokio::test]
async fn throttling_surfaces_retry_after() {
    let (mock, addr) = start().await;
    mock.lock().unwrap().throttle = true;
    let err = source(addr, "s3cret").list_changed(since(), 5).await.unwrap_err();
    let ra = err.chain().find_map(|c| c.downcast_ref::<RetryAfter>()).expect("RetryAfter in the chain");
    assert_eq!(ra.0, Duration::from_secs(7));
}

#[tokio::test]
async fn attachments_stream_to_disk_and_health_reports_the_inbox() {
    let (_mock, addr) = start().await;
    let src = source(addr, "s3cret");
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("jobs").join("7").join("source.mp4");
    let out = src.download_attachment("AAMk-2", "att-1", &dest).await.unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"video-bytes");
    assert!(!dest.with_extension("part").exists());

    let h = src.health().await;
    assert!(h.ok, "{}", h.detail);
    assert_eq!(h.detail, "ingest@example.gr: 4 unread");

    source_with(addr, "s3cret", true).reply("AAMk-1", "<p>ok</p>", "ok").await.unwrap();
}

#[tokio::test]
async fn an_unconfigured_source_says_so() {
    let (_mock, addr) = start().await;
    let src = source(addr, "");
    assert!(!src.is_configured());
    assert!(!src.health().await.ok);
}

#[tokio::test]
async fn test_access_signs_in_and_reads_the_inbox_and_one_message() {
    let (mock, addr) = start().await;
    let detail = source(addr, "s3cret").test_access().await.unwrap();
    assert_eq!(detail, "Signed in; the Inbox of ingest@example.gr is readable (4 unread).");
    let m = mock.lock().unwrap();
    let calls = graph_calls(&m);
    assert!(calls.iter().all(|c| c.method == "GET"), "a test must not change the mailbox: {calls:?}");
    assert!(calls.iter().any(|c| c.path.ends_with("/mailFolders/Inbox/messages") && c.query.contains("$top=1")));
}

#[tokio::test]
async fn test_access_names_the_fix_for_a_bad_secret_and_for_a_missing_permission() {
    let (mock, addr) = start().await;
    let err = source(addr, "wrong-secret-value").test_access().await.unwrap_err().to_string();
    assert!(err.contains("AADSTS7000215"), "{err}");
    assert!(!err.contains("wrong-secret-value"), "{err}");

    mock.lock().unwrap().forbid = true;
    let err = source(addr, "s3cret").test_access().await.unwrap_err().to_string();
    assert!(err.contains("Mail.Read") && err.contains("application access policy"), "{err}");

    let err = source(addr, "").test_access().await.unwrap_err().to_string();
    assert!(err.contains("client secret"), "{err}");
}
