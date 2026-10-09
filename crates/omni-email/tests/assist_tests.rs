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
use omni_email::link_titles::TitleSource;
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
        .with_titles(TitleSource::Fixed(Default::default()))
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
    if a.wanted(m, &p, &body, &[]) {
        a.refine(m, &body, &mut p, &r, &[], &cfg).await;
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

// ---------------------------------------------------------------------------
// Group tie-break (plan P4.19)
// ---------------------------------------------------------------------------

fn show(code: &str, name: &str, description: &str) -> omni_core::taxonomy::Group {
    omni_core::taxonomy::Group {
        code: code.into(),
        name: name.into(),
        kind: "show".into(),
        keywords: vec![],
        description: description.into(),
    }
}

fn shows() -> Vec<omni_core::taxonomy::Group> {
    vec![
        show("NEWS", "Κεντρικό Δελτίο", "Ειδήσεις της ημέρας"),
        show("MORNING", "Πρωινή εκπομπή", "Συνεντεύξεις και θέματα πόλης"),
    ]
}

/// Nothing names a group and the sender is unknown: the rules give none.
const UNGROUPED: &str = "Η συνέντευξη του δημάρχου:\nhttps://www.youtube.com/watch?v=as0100";

#[tokio::test]
async fn a_listed_group_is_adopted_when_the_rules_chose_none() {
    let (mock, addr) = start(r#"{"journalist_surname_latin": null, "keywords": {}, "group_code": "morning"}"#).await;
    let a = assist(&format!("http://{addr}/v1"));
    let m = mail("someone@example.org", "Συνέντευξη", UNGROUPED);
    let without = omni_email::assist::interpret(&m, &roster(), &shows(), &ParserConfig::default(), None).await;
    let p = omni_email::assist::interpret(&m, &roster(), &shows(), &ParserConfig::default(), Some(&a)).await;

    let g = p.group.clone().expect("group adopted");
    assert_eq!((g.code.as_str(), g.how), ("MORNING", omni_email::groups::GroupResolution::LlmAssist));
    assert_eq!(jobs(&p), jobs(&without), "a group must not change any job");
    // The model was shown the groups with their descriptions.
    let prompt = mock.lock().unwrap().requests[0]["messages"][1]["content"].as_str().unwrap().to_string();
    assert!(prompt.contains("- MORNING: Πρωινή εκπομπή, show; Συνεντεύξεις και θέματα πόλης"), "{prompt}");
}

#[tokio::test]
async fn a_group_that_is_not_in_the_list_is_discarded() {
    let (_m, addr) = start(r#"{"journalist_surname_latin": null, "keywords": {}, "group_code": "WEATHER"}"#).await;
    let a = assist(&format!("http://{addr}/v1"));
    let m = mail("someone@example.org", "Συνέντευξη", UNGROUPED);
    let p = omni_email::assist::interpret(&m, &roster(), &shows(), &ParserConfig::default(), Some(&a)).await;
    assert!(p.group.is_none());
    assert!(p.has_warning(warnings::LLM_ASSIST_SKIPPED));
}

#[tokio::test]
async fn a_rule_outranks_the_models_group() {
    // The model names the journalist *and* a group. NIKOLAOU's default group
    // is NEWS: once he is known, that rule decides, not the model's guess.
    let mut r = roster();
    r.iter_mut().find(|j| j.surname == "NIKOLAOU").unwrap().groups = vec!["NEWS".into()];
    let (_m, addr) =
        start(r#"{"journalist_surname_latin": "NIKOLAOU", "keywords": {}, "group_code": "MORNING"}"#).await;
    let a = assist(&format!("http://{addr}/v1"));
    let m = mail("someone@example.org", "Βίντεο σύσκεψης", UNROUTED);
    let p = omni_email::assist::interpret(&m, &r, &shows(), &ParserConfig::default(), Some(&a)).await;
    assert_eq!(p.journalist.surname, "NIKOLAOU");
    let g = p.group.expect("group");
    assert_eq!((g.code.as_str(), g.how), ("NEWS", omni_email::groups::GroupResolution::Member));
}

#[tokio::test]
async fn with_a_group_decided_by_rules_the_model_is_not_asked_about_groups() {
    let (mock, addr) = start(r#"{"journalist_surname_latin": null, "keywords": {}, "group_code": null}"#).await;
    let a = assist(&format!("http://{addr}/v1"));
    let m = mail("a.papadaki@example.gr", "Για την πρωινή εκπομπή", "https://youtu.be/as0101");
    let p = omni_email::assist::interpret(&m, &roster(), &shows(), &ParserConfig::default(), Some(&a)).await;
    assert_eq!(p.group.unwrap().how, omni_email::groups::GroupResolution::Subject);
    // Journalist known, keyword made from the subject, group decided: no call.
    assert!(mock.lock().unwrap().requests.is_empty());
}

// ---------------------------------------------------------------------------
// Recipient from context, desk senders, suggested names (plan P4.26)
// ---------------------------------------------------------------------------

/// A desk address on the roster as MCR, and a journalist known by a short form.
fn desk_roster() -> Vec<Journalist> {
    let mut r = roster();
    r.iter_mut().find(|j| j.surname == "MCR").unwrap().emails = vec!["desk@example.gr".into()];
    r.push(Journalist {
        id: 0,
        surname: "STAVRAKI".into(),
        full_name: "Evangelia Stavraki".into(),
        emails: vec![],
        default_priority: 0,
        aliases: vec!["ΣΤΑΥΡΑΚΗ".into(), "ΕΥΑΓΓΕΛΙΑ".into()],
        groups: vec![],
        created_at: None,
    });
    r
}

async fn run_with(a: &Assist, m: &InboundMail, r: &[Journalist]) -> ParsedEmail {
    omni_email::assist::interpret(m, r, &[], &ParserConfig::default(), Some(a)).await
}

#[tokio::test]
async fn a_mail_from_the_desk_address_is_routed_to_the_journalist_it_names() {
    let (mock, addr) = start(r#"{"journalist_surname_latin": "STAVRAKI", "journalist_named_in_mail": null, "keywords": {}, "group_code": null}"#).await;
    let a = assist(&format!("http://{addr}/v1"));
    let m = mail("desk@example.gr", "VIRAL ΓΙΑ ΕΥΗ", "https://www.youtube.com/watch?v=as0200");
    let without = omni_email::assist::interpret(&m, &desk_roster(), &[], &ParserConfig::default(), None).await;
    // The desk passes mail on and is never the journalist: a desk mail that
    // names nobody the parser can place is unresolved, not "MCR by sender".
    assert_eq!((without.journalist.surname.as_str(), without.journalist.how), ("MCR", Resolution::Unresolved));

    let p = run_with(&a, &m, &desk_roster()).await;
    assert_eq!((p.journalist.surname.as_str(), p.journalist.how), ("STAVRAKI", Resolution::LlmAssist));
    // The unresolved penalty is given back once the model placed the journalist.
    assert_eq!(jobs(&without)[0]["confidence"], 0.75);
    assert_eq!(jobs(&p)[0]["confidence"], 0.95);

    // The model saw the aliases, which is how "Εύη" can be matched.
    let prompt = mock.lock().unwrap().requests[0]["messages"][1]["content"].as_str().unwrap().to_string();
    assert!(prompt.contains("- STAVRAKI: Evangelia Stavraki; ΣΤΑΥΡΑΚΗ, ΕΥΑΓΓΕΛΙΑ"), "{prompt}");
    let system = mock.lock().unwrap().requests[0]["messages"][0]["content"].as_str().unwrap().to_string();
    assert!(system.contains("Εύη = Ευαγγελία") && system.contains("never the recipient journalist's name"), "{system}");
}

#[tokio::test]
async fn a_recipient_not_on_the_roster_is_suggested_to_mcr_and_changes_no_job() {
    let (_m, addr) = start(r#"{"journalist_surname_latin": null, "journalist_named_in_mail": "Διαμαντής", "keywords": {}, "group_code": null}"#).await;
    let a = assist(&format!("http://{addr}/v1"));
    let m = mail("desk@example.gr", "Πρ: Για Διαμαντη", "https://www.instagram.com/reel/as0300/");
    let without = omni_email::assist::interpret(&m, &desk_roster(), &[], &ParserConfig::default(), None).await;
    let p = run_with(&a, &m, &desk_roster()).await;
    assert_eq!(p.journalist.surname, "MCR");
    // The subject says it plainly: the parser reports it as written, and the
    // model's version is not added a second time.
    let found: Vec<&str> = p.warnings.iter().filter(|w| w.code == warnings::JOURNALIST_SUGGESTED).filter_map(|w| w.detail.as_deref()).collect();
    assert_eq!(found, vec!["Διαμαντη"]);
    assert_eq!(jobs(&p), jobs(&without));

    // Only the body names the recipient: that one needs the model.
    let body_named = mail("desk@example.gr", "Βίντεο", "Για τον Διαμαντή, από τη συνέντευξη:
https://www.instagram.com/reel/as0301/");
    let p = run_with(&a, &body_named, &desk_roster()).await;
    let found: Vec<&str> = p.warnings.iter().filter(|w| w.code == warnings::JOURNALIST_SUGGESTED).filter_map(|w| w.detail.as_deref()).collect();
    assert_eq!(found, vec!["Διαμαντής"]);

    // Anything that is not a plain name is not passed on.
    let (_m, addr) = start(r#"{"journalist_surname_latin": null, "journalist_named_in_mail": "<script>x</script>", "keywords": {}, "group_code": null}"#).await;
    let p = run_with(&assist(&format!("http://{addr}/v1")), &body_named, &desk_roster()).await;
    assert!(!p.has_warning(warnings::JOURNALIST_SUGGESTED));
}

// ---------------------------------------------------------------------------
// Keyword confidence (plan P4.34)
// ---------------------------------------------------------------------------

/// The desk's mail of 2026-10-07: four film trailers, one subject word.
const TRAILERS: &str = "Καλημέρα κι ευχαριστώ!\nhttps://www.youtube.com/watch?v=tr0000001aa\nhttps://www.youtube.com/watch?v=tr0000002aa\nhttps://www.youtube.com/watch?v=tr0000003aa\nhttps://www.youtube.com/watch?v=tr0000004aa";

fn trailer_titles() -> TitleSource {
    TitleSource::Fixed(
        [
            ("https://www.youtube.com/watch?v=tr0000001aa", "Dune: Part Three | Official Trailer"),
            ("https://www.youtube.com/watch?v=tr0000002aa", "Mission Kalyx - Official Trailer (HD)"),
            ("https://www.youtube.com/watch?v=tr0000004aa", "Official Teaser"),
        ]
        .into_iter()
        .map(|(u, t)| (u.to_string(), t.to_string()))
        .collect(),
    )
}

fn keywords(p: &ParsedEmail) -> Vec<(String, String)> {
    p.jobs().map(|j| (j.index_str.clone(), j.keyword.clone())).collect()
}

#[tokio::test]
async fn uncertain_keywords_are_asked_about_with_the_videos_own_titles() {
    // The model names 1A, answers a generic word for 1B and junk for 1C.
    let (mock, addr) = start(
        r#"{"journalist_surname_latin": null, "journalist_named_in_mail": null, "keywords": {},
            "video_keywords": {"1A": "DUNE", "1B": "TRAILER", "1C": "NULL", "9Z": "EVIL"}, "group_code": null}"#,
    )
    .await;
    let a = assist(&format!("http://{addr}/v1")).with_titles(trailer_titles());
    let p = run(&a, &mail("a.papadaki@example.gr", "Πρ: τρέιλερ 9-10", TRAILERS)).await;

    let prompt = mock.lock().unwrap().requests[0].to_string();
    assert!(prompt.contains("Dune: Part Three | Official Trailer"), "the title reaches the model: {prompt}");
    assert!(prompt.contains("1C: https://www.youtube.com/watch?v=tr0000003aa | unknown | TREILER"), "{prompt}");

    assert_eq!(
        keywords(&p),
        [
            ("1A".to_string(), "DUNE".to_string()),      // the model
            ("1B".to_string(), "MISSIONKALYX".to_string()), // "TRAILER" refused; its own title
            ("1C".to_string(), "TREILER".to_string()),   // nothing to go on
            ("1D".to_string(), "TREILER".to_string()),   // its title is only "Official Teaser"
        ]
    );
    // Same videos, same order, same statuses: only names moved.
    assert_eq!(p.jobs().count(), 4);
    let uncertain = p.warnings.iter().find(|w| w.code == warnings::KEYWORD_UNCERTAIN).expect("MCR is told");
    let detail = uncertain.detail.as_deref().unwrap();
    assert!(detail.contains("1C TREILER") && detail.contains("1D TREILER") && !detail.contains("1A"), "{detail}");
}

#[tokio::test]
async fn with_the_llm_down_the_videos_own_titles_still_name_them() {
    let a = assist("http://127.0.0.1:9/v1").with_titles(trailer_titles());
    let p = run(&a, &mail("a.papadaki@example.gr", "Πρ: τρέιλερ 9-10", TRAILERS)).await;
    let k = keywords(&p);
    assert_eq!(k[0].1, "DUNEPART");
    assert_eq!(k[1].1, "MISSIONKALYX");
    assert!(p.has_warning(warnings::KEYWORD_FROM_TITLES));
    assert!(p.has_warning(warnings::LLM_ASSIST_SKIPPED));
}

#[tokio::test]
async fn a_mail_whose_keywords_are_sure_asks_nothing_and_fetches_nothing() {
    let (mock, addr) = start("{}").await;
    // A title source that would show up in the keywords if it were used.
    let a = assist(&format!("http://{addr}/v1")).with_titles(TitleSource::Fixed(
        [("https://youtu.be/sure0000001".to_string(), "Something Else Entirely".to_string())].into_iter().collect(),
    ));
    let p = run(&a, &mail("a.papadaki@example.gr", "Σεισμός στη Σητεία", "https://youtu.be/sure0000001")).await;
    assert!(mock.lock().unwrap().requests.is_empty());
    assert_eq!(keywords(&p)[0].1, "SEISMOSSITEIA");
}
