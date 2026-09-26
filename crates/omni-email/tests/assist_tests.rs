//! LLM assist (plan P4.4) against a mock OpenAI-compatible server.
//!
//! The rule under test: the model can only ever *confirm* a journalist on the
//! roster or *supply* a well-formed keyword for a section the parser made.
//! Anything else it says is dropped, and with the model unreachable the jobs
//! are exactly the parser's.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use omni_core::config::{LlmConfig, LlmMode, ParserConfig};
use omni_core::models::{JobStatus, Journalist};
use omni_email::assist::Assist;
use omni_email::mail::InboundMail;
use omni_email::parser::{self, warnings, ParsedEmail, Resolution};

#[derive(Default)]
struct Mock {
    /// What the model "says": the message content, verbatim.
    content: String,
    /// Refuse `response_format: json_schema` like an older server would.
    reject_schema: bool,
    requests: Vec<Value>,
}

type Shared = Arc<Mutex<Mock>>;

async fn completions(State(mock): State<Shared>, Json(body): Json<Value>) -> Response {
    let mut m = mock.lock().unwrap();
    m.requests.push(body.clone());
    if m.reject_schema && body["response_format"]["type"] == "json_schema" {
        return (StatusCode::BAD_REQUEST, "response_format json_schema not supported").into_response();
    }
    Json(json!({ "choices": [ { "message": { "role": "assistant", "content": m.content } } ] })).into_response()
}

async fn start(content: &str) -> (Shared, SocketAddr) {
    let mock: Shared = Arc::new(Mutex::new(Mock { content: content.into(), ..Default::default() }));
    let app = Router::new().route("/v1/chat/completions", post(completions)).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app.into_make_service()).await.unwrap() });
    (mock, addr)
}

fn roster() -> Vec<Journalist> {
    let j = |surname: &str, full: &str, emails: &[&str], aliases: &[&str]| Journalist {
        id: 0,
        surname: surname.into(),
        full_name: full.into(),
        emails: emails.iter().map(|s| s.to_string()).collect(),
        default_priority: 0,
        aliases: aliases.iter().map(|s| s.to_string()).collect(),
        groups: vec![],
        created_at: None,
    };
    vec![
        j("MCR", "Master Control Room", &[], &[]),
        j("PAPADAKI", "Anna Papadaki", &["a.papadaki@example.gr"], &["ΠΑΠΑΔΑΚΗ", "ΑΝΝΑ"]),
        j("NIKOLAOU", "Giorgos Nikolaou", &["g.nikolaou@example.gr"], &["ΝΙΚΟΛΑΟΥ", "ΓΙΩΡΓΟΣ"]),
        j("GEORGIOU", "Eleni Georgiou", &["e.georgiou@example.gr"], &["ΓΕΩΡΓΙΟΥ", "ΕΛΕΝΗ"]),
        j("DIMITRIOU", "Kostas Dimitriou", &["k.dimitriou@example.gr"], &["ΔΗΜΗΤΡΙΟΥ", "ΚΩΣΤΑ"]),
    ]
}

fn assist(endpoint: &str) -> Assist {
    Assist::new(endpoint, "test-model", LlmConfig { mode: LlmMode::Assist, timeout_secs: 5, ..Default::default() })
}

fn mail(from: &str, subject: &str, body: &str) -> InboundMail {
    InboundMail {
        id: "m".into(),
        internet_message_id: "<m@example.gr>".into(),
        from_address: from.into(),
        subject: subject.into(),
        body_text: body.into(),
        ..Default::default()
    }
}

async fn run(a: &Assist, m: &InboundMail) -> ParsedEmail {
    let r = roster();
    let cfg = ParserConfig::default();
    let mut p = parser::parse(m, &r, &cfg);
    let body = m.readable_body();
    if a.wanted(&p, &body) {
        a.refine(m, &body, &mut p, &r, &cfg).await;
    }
    p
}

/// What goes to air: everything about each job except nothing.
fn jobs(p: &ParsedEmail) -> Vec<Value> {
    p.jobs().map(|j| serde_json::to_value(j).unwrap()).collect()
}

const UNROUTED: &str = "Πλάνα για τον Γιώργο από τη σύσκεψη:\nhttps://www.youtube.com/watch?v=as0001";

#[tokio::test]
async fn a_journalist_on_the_roster_is_adopted_and_the_penalty_lifted() {
    for name in ["Giorgos Nikolaou", "Νικολάου", "ΓΙΩΡΓΟΣ"] {
        let (_m, addr) = start(&json!({"journalist_surname_latin": name, "keywords": {}}).to_string()).await;
        let a = assist(&format!("http://{addr}/v1"));
        let p = run(&a, &mail("someone@example.org", "Βίντεο σύσκεψης", UNROUTED)).await;
        assert_eq!(p.journalist.surname, "NIKOLAOU", "{name}");
    }

    let (_m, addr) = start(r#"{"journalist_surname_latin": "NIKOLAOU", "keywords": {}}"#).await;
    let a = assist(&format!("http://{addr}/v1"));
    let p = run(&a, &mail("someone@example.org", "Βίντεο σύσκεψης", UNROUTED)).await;

    assert_eq!(p.journalist.surname, "NIKOLAOU");
    assert_eq!(p.journalist.how, Resolution::LlmAssist);
    let j = p.jobs().next().unwrap();
    assert_eq!(j.confidence, 0.95, "the unresolved penalty must be taken back");
    assert_eq!(j.status, JobStatus::Pending);
    assert!(!p.has_warning(warnings::JOURNALIST_UNRESOLVED));
    assert!(p.has_warning(warnings::LLM_ASSIST_APPLIED));
}

#[tokio::test]
async fn a_journalist_not_on_the_roster_is_discarded() {
    for proposal in [r#""KONSTANTINOU""#, r#""MCR""#, r#""ANNA; DROP TABLE""#, "null", "42"] {
        let (_m, addr) = start(&format!(r#"{{"journalist_surname_latin": {proposal}, "keywords": {{}}}}"#)).await;
        let a = assist(&format!("http://{addr}/v1"));
        let p = run(&a, &mail("someone@example.org", "Βίντεο σύσκεψης", UNROUTED)).await;
        assert_eq!(p.journalist.surname, "MCR", "{proposal} was accepted");
        assert!(p.has_warning(warnings::LLM_ASSIST_SKIPPED), "{proposal}");
        assert_eq!(p.jobs().next().unwrap().confidence, 0.75);
    }
}

const NO_KEYWORD: &str = "1. Δείτε εδώ\nhttps://www.youtube.com/watch?v=as0002\n\n2. ΠΑΡΕΛΑΣΗ\nhttps://youtu.be/as0003";

#[tokio::test]
async fn a_valid_keyword_fills_an_asset_section_and_nothing_else_moves() {
    let (_m, addr) = start(
        r#"{"journalist_surname_latin": null,
            "keywords": {"1": "Κνωσός", "2": "OVERRIDE", "9": "GHOST"},
            "jobs": [{"url": "https://evil.example/x", "index_str": "9"}]}"#,
    )
    .await;
    let a = assist(&format!("http://{addr}/v1"));
    let p = run(&a, &mail("a.papadaki@example.gr", "ΘΕΜΑΤΑ", NO_KEYWORD)).await;

    let kws: Vec<(&str, &str)> = p.jobs().map(|j| (j.index_str.as_str(), j.keyword.as_str())).collect();
    // Section 1 had no keyword and gets the transliterated one; section 2's
    // own keyword is not overridden (not asked about); 9 and the URL are
    // inventions and change nothing.
    assert_eq!(kws, vec![("1", "KNOSOS"), ("2", "PARELASI")]);
    assert_eq!(p.jobs().count(), 2);
    assert!(!p.jobs().any(|j| j.url.contains("evil")));
    assert_eq!(p.journalist.surname, "PAPADAKI");
}

#[tokio::test]
async fn a_malformed_keyword_is_discarded() {
    for bad in ["knicks parade", "K", "ABCDEFGHIJKLMNOPQRSTUVWXYZ", "HTTPS://X", "VIDEO", ""] {
        let (_m, addr) = start(&json!({"journalist_surname_latin": null, "keywords": {"1": bad}}).to_string()).await;
        let a = assist(&format!("http://{addr}/v1"));
        let p = run(&a, &mail("a.papadaki@example.gr", "ΘΕΜΑΤΑ", NO_KEYWORD)).await;
        assert_eq!(p.jobs().next().unwrap().keyword, "ASSET", "{bad:?} was accepted");
        assert!(p.has_warning(warnings::LLM_ASSIST_SKIPPED), "{bad:?}");
    }
}

#[tokio::test]
async fn a_server_without_json_schema_is_asked_again_in_json_mode_and_remembered() {
    let (mock, addr) = start(r#"{"journalist_surname_latin": "NIKOLAOU", "keywords": {}}"#).await;
    mock.lock().unwrap().reject_schema = true;
    let a = assist(&format!("http://{addr}/v1"));

    let p = run(&a, &mail("someone@example.org", "Βίντεο σύσκεψης", UNROUTED)).await;
    assert_eq!(p.journalist.surname, "NIKOLAOU");
    run(&a, &mail("someone@example.org", "Βίντεο σύσκεψης", UNROUTED)).await;

    let m = mock.lock().unwrap();
    let kinds: Vec<&str> = m.requests.iter().map(|r| r["response_format"]["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, vec!["json_schema", "json_object", "json_object"]);
    assert_eq!(m.requests[0]["temperature"], 0);
}

#[tokio::test]
async fn a_resolved_mail_with_keywords_never_calls_the_model() {
    let (mock, addr) = start("{}").await;
    let a = assist(&format!("http://{addr}/v1"));
    run(&a, &mail("a.papadaki@example.gr", "ΘΕΜΑΤΑ", "1. ΠΑΡΕΛΑΣΗ\nhttps://youtu.be/as0004")).await;
    assert!(mock.lock().unwrap().requests.is_empty());
}

/// Plan P4 acceptance: with the LLM unreachable, every fixture yields exactly
/// the jobs the parser alone yields.
#[tokio::test]
async fn with_the_llm_down_every_fixture_keeps_the_parsers_jobs() {
    let dead = assist("http://127.0.0.1:9/v1");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures");
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|x| x != "eml") {
            continue;
        }
        let m = InboundMail::from_rfc822("f", &std::fs::read(&path).unwrap()).unwrap();
        let alone = parser::parse(&m, &roster(), &ParserConfig::default());
        let assisted = run(&dead, &m).await;
        assert_eq!(jobs(&assisted), jobs(&alone), "{}", path.display());
        assert_eq!(assisted.journalist, alone.journalist, "{}", path.display());
        checked += 1;
    }
    assert!(checked >= 15);
}
