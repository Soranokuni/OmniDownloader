//! Graph mail source against a mock of the Graph and login endpoints
//! (plan P4.2). The station's app registration does not exist yet, so this
//! is the only place the client is exercised end to end.

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
use omni_email::source::{MailOutcome, MailSource};

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
        (Method::GET, "/mailFolders/Inbox/messages") => json_resp(json!({ "value": [
            {
                "id": "AAMk-1", "internetMessageId": "<g1@example.gr>", "subject": "ΘΕΜΑΤΑ",
                "from": { "emailAddress": { "name": "Anna Papadaki", "address": "A.Papadaki@example.gr" } },
                "toRecipients": [ { "emailAddress": { "address": "ingest@example.gr" } } ],
                "ccRecipients": [],
                "receivedDateTime": "2026-09-24T08:00:00Z",
                "body": { "contentType": "text", "content": "1. ΤΕΣΤ\nhttps://youtu.be/g0001" },
                "hasAttachments": false
            },
            {
                "id": "AAMk-2", "internetMessageId": "<g2@example.gr>", "subject": "Βίντεο",
                "from": { "emailAddress": { "name": "Kostas", "address": "k.dimitriou@example.gr" } },
                "receivedDateTime": "2026-09-24T08:05:00Z",
                "body": { "contentType": "text", "content": "" },
                "hasAttachments": true
            }
        ]})),
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
    let cfg = GraphConfig {
        tenant_id: "tenant-1".into(),
        client_id: "client-1".into(),
        mailbox: "ingest@example.gr".into(),
        client_secret: secret.into(),
        ..Default::default()
    };
    let base = format!("http://{addr}");
    GraphMailSource::with_endpoints(cfg, &base, &base)
}

fn graph_calls(m: &Mock) -> Vec<Seen> {
    m.seen.iter().filter(|s| !s.path.ends_with("/token")).cloned().collect()
}

#[tokio::test]
async fn fetch_maps_messages_and_attachments_and_asks_for_text_bodies() {
    let (mock, addr) = start().await;
    let src = source(addr, "s3cret");
    let mails = src.fetch_unprocessed(20).await.unwrap();

    assert_eq!(mails.len(), 2);
    assert_eq!(mails[0].id, "AAMk-1");
    assert_eq!(mails[0].internet_message_id, "<g1@example.gr>");
    assert_eq!(mails[0].from_address, "a.papadaki@example.gr");
    assert_eq!(mails[0].body_text, "1. ΤΕΣΤ\nhttps://youtu.be/g0001");
    assert!(mails[0].received_at.is_some());
    // Inline images (signature logos) are not attachments worth parsing.
    assert_eq!(mails[1].attachments.len(), 1);
    assert_eq!(mails[1].attachments[0].name, "limani.mp4");

    let m = mock.lock().unwrap();
    let list = &graph_calls(&m)[0];
    assert!(list.query.contains("isRead eq false"), "{}", list.query);
    assert!(list.query.contains("$orderby=receivedDateTime asc"), "{}", list.query);
    assert_eq!(list.prefer, "outlook.body-content-type=\"text\"");
}

#[tokio::test]
async fn the_token_is_cached_and_refreshed_once_on_401() {
    let (mock, addr) = start().await;
    let src = source(addr, "s3cret");
    src.fetch_unprocessed(5).await.unwrap();
    src.fetch_unprocessed(5).await.unwrap();
    assert_eq!(mock.lock().unwrap().tokens_issued, 1, "token not cached");

    mock.lock().unwrap().reject_next = true;
    src.fetch_unprocessed(5).await.unwrap();
    let m = mock.lock().unwrap();
    assert_eq!(m.tokens_issued, 2, "401 did not refresh the token");
    assert_eq!(graph_calls(&m).last().unwrap().auth, "Bearer tok-2");
}

#[tokio::test]
async fn a_bad_secret_is_reported_without_echoing_it() {
    let (_mock, addr) = start().await;
    let err = source(addr, "wrong-secret-value").fetch_unprocessed(5).await.unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("Graph login failed"), "{text}");
    assert!(text.contains("AADSTS7000215"), "{text}");
    assert!(!text.contains("wrong-secret-value"), "secret leaked into the error: {text}");
    assert!(!format!("{:?}", source(addr, "wrong-secret-value")).contains("wrong-secret-value"));
}

#[tokio::test]
async fn processed_mail_is_read_and_moved_failed_mail_moved_but_left_unread() {
    let (mock, addr) = start().await;
    let src = source(addr, "s3cret");

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
    let err = source(addr, "s3cret").fetch_unprocessed(5).await.unwrap_err();
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

    src.reply("AAMk-1", "<p>ok</p>", "ok").await.unwrap();
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
