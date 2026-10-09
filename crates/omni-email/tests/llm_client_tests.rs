//! The provider-neutral LLM client (plan P4.22) against a scripted
//! OpenAI-compatible server: auth styles, the optional fields a server may
//! refuse, and the failures an operator must be able to read.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use omni_core::config::{LlmAuth, LlmConfig};
use omni_email::assist::redact_contacts;
use omni_email::llm::{check_base_url, is_local_endpoint, LlmClient};

#[derive(Default)]
struct Mock {
    /// Fields the server refuses with a 400 that names them.
    refuse: Vec<&'static str>,
    /// Refuse any response_format, without naming it.
    refuse_formats: bool,
    /// Require this `Authorization` or `api-key` value.
    want_bearer: Option<String>,
    want_api_key: Option<String>,
    /// What the model "says", or None for the default answer.
    content: Option<Value>,
    finish_reason: Option<&'static str>,
    requests: Vec<(HeaderMap, Value)>,
}

type Shared = Arc<Mutex<Mock>>;

async fn completions(State(mock): State<Shared>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let mut m = mock.lock().unwrap();
    m.requests.push((headers.clone(), body.clone()));
    if let Some(want) = &m.want_bearer {
        if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(&format!("Bearer {want}")) {
            return (StatusCode::UNAUTHORIZED, r#"{"error":{"message":"Incorrect API key provided"}}"#).into_response();
        }
    }
    if let Some(want) = &m.want_api_key {
        if headers.get("api-key").and_then(|v| v.to_str().ok()) != Some(want.as_str()) {
            return (StatusCode::UNAUTHORIZED, r#"{"error":{"message":"Access denied due to invalid subscription key"}}"#).into_response();
        }
    }
    for field in m.refuse.clone() {
        if body.get(field).is_some() {
            let msg = format!(r#"{{"error":{{"message":"Unsupported parameter: '{field}'"}}}}"#);
            return (StatusCode::BAD_REQUEST, msg).into_response();
        }
    }
    if m.refuse_formats && body.get("response_format").is_some() {
        return (StatusCode::BAD_REQUEST, r#"{"error":{"message":"invalid request"}}"#).into_response();
    }
    let content = m.content.clone().unwrap_or_else(|| json!(r#"{"word":"LIMANI"}"#));
    Json(json!({ "choices": [ { "message": { "role": "assistant", "content": content }, "finish_reason": m.finish_reason.unwrap_or("stop") } ] }))
        .into_response()
}

async fn models(State(mock): State<Shared>, headers: HeaderMap) -> Response {
    let m = mock.lock().unwrap();
    if let Some(want) = &m.want_bearer {
        if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(&format!("Bearer {want}")) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    Json(json!({ "data": [ {"id": "google/gemma-4-e4b"}, {"id": "qwen3.5:4b"} ] })).into_response()
}

async fn start(mock: Mock) -> (Shared, SocketAddr) {
    let shared: Shared = Arc::new(Mutex::new(mock));
    let app = Router::new()
        .route("/v1/chat/completions", post(completions))
        .route("/v1/models", get(models))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app.into_make_service()).await.unwrap() });
    (shared, addr)
}

fn client(addr: SocketAddr, auth: LlmAuth, key: &str) -> LlmClient {
    let cfg = LlmConfig {
        timeout_secs: 10,
        auth,
        api_key: key.into(),
        disable_thinking: true,
        ..LlmConfig::default()
    };
    LlmClient::from_settings(&format!("http://{addr}/v1/"), "test-model", &cfg)
}

fn schema() -> Value {
    json!({"type":"object","properties":{"word":{"type":"string"}},"required":["word"]})
}

async fn ask(c: &LlmClient) -> anyhow::Result<Value> {
    c.chat_json("Answer JSON.", "ΛΙΜΑΝΙ", &schema(), 100).await
}

#[tokio::test]
async fn the_key_goes_the_way_the_provider_expects_and_never_into_an_error() {
    let (m, addr) = start(Mock { want_bearer: Some("sk-good".into()), ..Default::default() }).await;
    assert_eq!(ask(&client(addr, LlmAuth::Bearer, "sk-good")).await.unwrap()["word"], "LIMANI");
    assert_eq!(client(addr, LlmAuth::Bearer, "sk-good").list_models().await.unwrap(), vec!["google/gemma-4-e4b", "qwen3.5:4b"]);

    let err = format!("{:#}", ask(&client(addr, LlmAuth::Bearer, "sk-wrong-secret")).await.unwrap_err());
    assert!(err.contains("401") && err.contains("API key"), "{err}");
    assert!(!err.contains("sk-wrong-secret"), "the key leaked into the error: {err}");
    assert!(!format!("{:?}", client(addr, LlmAuth::Bearer, "sk-wrong-secret")).contains("sk-wrong"));
    drop(m);

    let (_m, addr) = start(Mock { want_api_key: Some("azure-key".into()), ..Default::default() }).await;
    assert!(ask(&client(addr, LlmAuth::ApiKeyHeader, "azure-key")).await.is_ok());
    assert!(ask(&client(addr, LlmAuth::Bearer, "azure-key")).await.is_err(), "Azure wants api-key, not Bearer");

    // No auth: no Authorization header at all, even with a key typed.
    let (m, addr) = start(Mock::default()).await;
    ask(&client(addr, LlmAuth::None, "unused")).await.unwrap();
    let (headers, _) = m.lock().unwrap().requests[0].clone();
    assert!(headers.get("authorization").is_none() && headers.get("api-key").is_none());
}

#[tokio::test]
async fn thinking_is_switched_off_and_a_server_that_refuses_the_field_is_asked_again_without_it() {
    let (m, addr) = start(Mock { refuse: vec!["reasoning_effort"], ..Default::default() }).await;
    let c = client(addr, LlmAuth::None, "");
    assert_eq!(ask(&c).await.unwrap()["word"], "LIMANI");
    ask(&c).await.unwrap();
    let reqs = m.lock().unwrap().requests.clone();
    assert_eq!(reqs[0].1["reasoning_effort"], "none", "thinking not switched off");
    assert!(reqs[1].1.get("reasoning_effort").is_none(), "retry still sent the refused field");
    assert_eq!(reqs.len(), 3, "the refusal was not remembered: {}", reqs.len());
}

#[tokio::test]
async fn newer_openai_models_get_max_completion_tokens_and_no_temperature() {
    let (m, addr) = start(Mock { refuse: vec!["max_tokens", "temperature"], ..Default::default() }).await;
    assert!(ask(&client(addr, LlmAuth::None, "")).await.is_ok());
    let last = m.lock().unwrap().requests.last().unwrap().1.clone();
    assert_eq!(last["max_completion_tokens"], 100);
    assert!(last.get("max_tokens").is_none() && last.get("temperature").is_none(), "{last}");
}

#[tokio::test]
async fn a_server_without_structured_output_is_asked_in_plain_json_then_with_no_format() {
    let (m, addr) = start(Mock { refuse_formats: true, ..Default::default() }).await;
    assert!(ask(&client(addr, LlmAuth::None, "")).await.is_ok());
    let formats: Vec<Value> = m.lock().unwrap().requests.iter().map(|(_, b)| b["response_format"]["type"].clone()).collect();
    assert_eq!(formats.first().unwrap(), "json_schema");
    assert!(formats.contains(&json!("json_object")));
    assert_eq!(formats.last().unwrap(), &Value::Null, "{formats:?}");
}

#[tokio::test]
async fn answers_in_prose_in_parts_or_cut_short_are_handled() {
    let (_m, addr) = start(Mock { content: Some(json!("Sure! Here it is: {\"word\": \"LIMANI\"} Hope this helps.")), ..Default::default() }).await;
    assert_eq!(ask(&client(addr, LlmAuth::None, "")).await.unwrap()["word"], "LIMANI");

    let parts = json!([{"type": "text", "text": "{\"word\":"}, {"type": "text", "text": "\"LIMANI\"}"}]);
    let (_m, addr) = start(Mock { content: Some(parts), ..Default::default() }).await;
    assert_eq!(ask(&client(addr, LlmAuth::None, "")).await.unwrap()["word"], "LIMANI");

    // A reasoning model that spent the budget thinking: say what to change.
    let (_m, addr) = start(Mock { content: Some(json!("")), finish_reason: Some("length"), ..Default::default() }).await;
    let err = ask(&client(addr, LlmAuth::None, "")).await.unwrap_err().to_string();
    assert!(err.contains("Disable thinking"), "{err}");
}

#[test]
fn local_and_online_endpoints_are_told_apart_and_online_needs_https() {
    for local in ["http://127.0.0.1:1234/v1", "http://localhost:11434/v1", "http://192.168.1.20:1234/v1", "http://10.20.0.5/v1", "http://llm-box:8000/v1", "http://gpu.newsroom.lan/v1", "http://[::1]:1234/v1"] {
        assert!(is_local_endpoint(local), "{local}");
        assert!(check_base_url(local).is_ok(), "{local}");
    }
    for online in ["https://api.openai.com/v1", "https://generativelanguage.googleapis.com/v1beta/openai", "https://x.openai.azure.com/openai/v1", "http://8.8.8.8/v1"] {
        assert!(!is_local_endpoint(online), "{online}");
    }
    assert!(check_base_url("https://api.openai.com/v1").is_ok());
    assert!(check_base_url("http://api.openai.com/v1").unwrap_err().contains("https"));
    assert!(check_base_url("ftp://127.0.0.1/v1").is_err());
    assert!(check_base_url("not a url").is_err());
}

#[test]
fn contact_details_are_redacted_but_links_are_not() {
    let text = "Στείλε στο maria.p@example.gr ή πάρε στο 694 123 4567 ή +30 2810 123456.\n\
                https://www.youtube.com/watch?v=12345678901234 και https://example.gr/contact?mail=a@b.gr";
    let out = redact_contacts(text);
    assert!(!out.contains("maria.p@example.gr") && out.contains("[email]"), "{out}");
    assert!(!out.contains("694 123 4567") && !out.contains("2810 123456") && out.contains("[phone]"), "{out}");
    assert!(out.contains("https://www.youtube.com/watch?v=12345678901234"), "a link was changed: {out}");
    assert!(out.contains("https://example.gr/contact?mail=a@b.gr"), "a link was changed: {out}");
    assert!(out.contains("15:30") || !text.contains("15:30"));
}
