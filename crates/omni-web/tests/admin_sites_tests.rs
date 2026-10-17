//! The admin's Sites, Recipes and Repair-report routes (plan P10.15a): what
//! they return and change once the policy (see `auth_matrix_tests.rs`) has let
//! the admin in. Invented sites and recipes; no network.

use anyhow::Result;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tower::ServiceExt;

use omni_core::config::AppConfig;
use omni_core::models::UserRole;
use omni_core::repository::{AttemptEnd, Repository, DEFAULT_DEDUP_WINDOW_HOURS};
use omni_core::site_health::{SiteOutcome, SiteState};
use omni_web::server::WebServer;
use omni_web::state::{AppState, RunNow, SiteRunner};

const PEER: &str = "127.0.0.1:50000";

/// A runner that answers as told and remembers what it was asked to start.
struct FakeRunner {
    answer: RunNow,
    started: Mutex<Vec<String>>,
}

impl SiteRunner for FakeRunner {
    fn run_now(&self, domain: &str) -> RunNow {
        if self.answer == RunNow::Started {
            self.started.lock().unwrap().push(domain.to_string());
        }
        self.answer
    }
}

struct App {
    router: axum::Router,
    repo: Repository,
    admin: String,
    _dir: TempDir,
}

impl App {
    fn new(runner: Option<Arc<FakeRunner>>) -> Result<Self> {
        let dir = TempDir::new()?;
        let repo = Repository::new(dir.path().join("omni.db"))?;
        let id = repo.create_user("it@station.gr", "correct-horse-battery", UserRole::Admin, "Administrator", None)?;
        let admin = omni_core::auth::generate_session_token();
        repo.create_session(id, &admin, 1)?;
        let mut state = AppState::new(repo.clone(), AppConfig::default(), dir.path().join("config.json"));
        if let Some(r) = runner {
            state = state.with_site_runner(r);
        }
        Ok(Self { router: WebServer::build_router(state), repo, admin, _dir: dir })
    }

    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> Result<(StatusCode, Value)> {
        self.send_with(method, uri, body, true).await
    }

    async fn send_with(&self, method: &str, uri: &str, body: Option<Value>, marker: bool) -> Result<(StatusCode, Value)> {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", format!("omni_session={}", self.admin))
            .header("host", "mcr.local")
            .header("origin", "http://mcr.local");
        if marker {
            b = b.header("x-omni-request", "1");
        }
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&v)?)
            }
            None => Body::empty(),
        };
        let res = self.router.clone().layer(MockConnectInfo(PEER.parse::<SocketAddr>().unwrap())).oneshot(b.body(body)?).await?;
        let status = res.status();
        let bytes = res.into_body().collect().await?.to_bytes();
        Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
    }
}

const ATTR: &str = r#"{"dsl_version":1,"steps":[{"op":"attr","css":"video","attr":"data-src"},{"op":"abs","from":0},{"op":"media","from":1,"referer":"page"}]}"#;
const OTHER: &str = r#"{"dsl_version":1,"steps":[{"op":"attr","css":"div.player","attr":"data-video"},{"op":"abs","from":0},{"op":"media","from":1,"referer":"page"}]}"#;

fn entry(kind: &str, key: &str, recipe: &str) -> Value {
    json!({ "scope_kind": kind, "scope_key": key, "family": null, "recipe": serde_json::from_str::<Value>(recipe).unwrap() })
}

fn runner(answer: RunNow) -> Arc<FakeRunner> {
    Arc::new(FakeRunner { answer, started: Mutex::new(Vec::new()) })
}

// ------------------------------------------------------------------ sites

#[tokio::test]
async fn the_site_list_shows_state_rates_canaries_and_whether_run_now_exists() -> Result<()> {
    let app = App::new(None)?;
    let (_, none) = app.send("GET", "/api/admin/sites", None).await?;
    assert_eq!(none["sites"], json!([]));
    assert_eq!(none["can_run"], false, "no runner: the button is off");

    app.repo.record_site_outcome("news.example", SiteOutcome::Success)?;
    app.repo.record_site_outcome("news.example", SiteOutcome::SiteFailure("NO_VIDEO".into()))?;
    app.repo.set_site_state("news.example", SiteState::Broken)?;
    app.repo.add_canary("news.example", "https://news.example/a/secret-path?token=1", None, chrono::Utc::now())?;
    let (status, json) = app.send("GET", "/api/admin/sites", None).await?;
    assert_eq!(status, StatusCode::OK);
    let s = &json["sites"][0];
    assert_eq!((s["domain"].as_str(), s["state"].as_str()), (Some("news.example"), Some("broken")));
    assert_eq!(s["recent_rate"], 0.5);
    assert_eq!(s["last_code"], "NO_VIDEO");
    assert_eq!((s["canaries"].as_i64(), s["canaries_ok"].as_i64()), (Some(1), Some(0)));
    assert_eq!(json["repair_mode"], "off");
    assert!(!json.to_string().contains("secret-path"), "a host, never a page address: {json}");
    Ok(())
}

#[tokio::test]
async fn run_now_starts_one_check_and_says_busy_while_the_canary_lock_is_held() -> Result<()> {
    let free = runner(RunNow::Started);
    let app = App::new(Some(free.clone()))?;
    app.repo.record_site_outcome("news.example", SiteOutcome::SiteFailure("NO_VIDEO".into()))?;

    let (status, _) = app.send("POST", "/api/admin/sites/nosuch.example/check", None).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "an unknown site");
    let (status, json) = app.send("POST", "/api/admin/sites/news.example/check", None).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no canary pages: {json}");
    assert!(free.started.lock().unwrap().is_empty(), "nothing started for either");

    app.repo.add_canary("news.example", "https://news.example/a", None, chrono::Utc::now())?;
    let (status, json) = app.send("POST", "/api/admin/sites/NEWS.example/check", None).await?;
    assert_eq!((status, json["status"].as_str()), (StatusCode::OK, Some("started")));
    assert_eq!(*free.started.lock().unwrap(), ["news.example"], "the runner got the cleaned domain");

    let cooling = runner(RunNow::Cooldown(240));
    let app = App::new(Some(cooling.clone()))?;
    app.repo.record_site_outcome("news.example", SiteOutcome::SiteFailure("NO_VIDEO".into()))?;
    app.repo.add_canary("news.example", "https://news.example/a", None, chrono::Utc::now())?;
    let (status, json) = app.send("POST", "/api/admin/sites/news.example/check", None).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["error"]["code"], "COOLDOWN");
    let message = json["error"]["message"].as_str().unwrap();
    assert!(message.contains("4 min") && message.contains("again"), "says when: {message}");

    let busy = runner(RunNow::Busy);
    let app = App::new(Some(busy.clone()))?;
    app.repo.record_site_outcome("news.example", SiteOutcome::SiteFailure("NO_VIDEO".into()))?;
    app.repo.add_canary("news.example", "https://news.example/a", None, chrono::Utc::now())?;
    let (status, json) = app.send("POST", "/api/admin/sites/news.example/check", None).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["error"]["code"], "BUSY");
    assert!(busy.started.lock().unwrap().is_empty());

    let bare = App::new(None)?;
    bare.repo.record_site_outcome("news.example", SiteOutcome::SiteFailure("X".into()))?;
    bare.repo.add_canary("news.example", "https://news.example/a", None, chrono::Utc::now())?;
    let (status, json) = bare.send("POST", "/api/admin/sites/news.example/check", None).await?;
    assert_eq!(status, StatusCode::CONFLICT, "no runner at all: {json}");
    Ok(())
}

// ---------------------------------------------------------------- recipes

#[tokio::test]
async fn an_import_with_one_bad_entry_stores_nothing_and_lists_every_problem() -> Result<()> {
    let app = App::new(None)?;
    let mut upper = entry("site", "News.Example", ATTR);
    upper["family"] = json!(null);
    let file = json!({ "recipes": [
        entry("site", "good.example", ATTR),
        upper,
        entry("player", "nosuchplayer", ATTR),
        entry("site", "news.example.gr/path", ATTR),
        entry("generic", "x", ATTR),
        { "scope_kind": "site", "scope_key": "bad-recipe.example", "recipe": { "dsl_version": 1, "steps": [] } },
        { "scope_kind": "site", "scope_key": "worse.example", "recipe": { "dsl_version": 1, "steps": [{ "op": "run_script", "code": "x" }] } },
        { "scope_kind": "site", "scope_key": "another.example", "family": "nosuchfamily", "recipe": serde_json::from_str::<Value>(ATTR)? },
        "not an object",
    ]});
    let (status, json) = app.send("POST", "/api/admin/recipes/import", Some(file)).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    let message = json["error"]["message"].as_str().unwrap();
    for entry_no in 2..=9 {
        assert!(message.contains(&format!("Entry {entry_no} ")) || message.contains(&format!("Entry {entry_no}:")), "entry {entry_no} not listed: {message}");
    }
    assert!(!message.contains("Entry 1 ") && !message.contains("Entry 1:"), "the good entry is not a problem: {message}");
    assert!(message.contains("not imported"));
    assert!(app.repo.list_recipes(chrono::Utc::now())?.is_empty(), "all or nothing: the good entry was not stored either");

    let (status, _) = app.send("POST", "/api/admin/recipes/import", Some(json!({ "recipes": [] }))).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "an empty file");
    Ok(())
}

#[tokio::test]
async fn a_good_import_stores_admin_candidates_and_reports_each_entry() -> Result<()> {
    let app = App::new(None)?;
    let file = json!({ "recipes": [entry("site", "good.example", ATTR), entry("player", "jwplayer", OTHER)] });
    let (status, json) = app.send("POST", "/api/admin/recipes/import", Some(file.clone())).await?;
    assert_eq!(status, StatusCode::OK, "{json}");
    let outcomes: Vec<&str> = json["results"].as_array().unwrap().iter().map(|r| r["outcome"].as_str().unwrap()).collect();
    assert_eq!(outcomes, ["inserted", "inserted"]);

    let list = app.repo.list_recipes(chrono::Utc::now())?;
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|r| r.origin == "admin" && r.state == "candidate"), "still held for MCR: {list:?}");
    assert!(list.iter().any(|r| r.scope_kind == "player" && r.family.as_deref() == Some("jwplayer")));

    let (_, again) = app.send("POST", "/api/admin/recipes/import", Some(file)).await?;
    let again: Vec<&str> = again["results"].as_array().unwrap().iter().map(|r| r["outcome"].as_str().unwrap()).collect();
    assert_eq!(again, ["merged", "merged"], "the same file twice adds nothing");
    assert_eq!(app.repo.list_recipes(chrono::Utc::now())?.len(), 2);

    // A retired recipe is refused, not revived.
    let id = list.iter().find(|r| r.scope_kind == "site").unwrap().id;
    let (status, _) = app.send("POST", &format!("/api/admin/recipes/{id}/retire"), None).await?;
    assert_eq!(status, StatusCode::OK);
    let (_, third) = app.send("POST", "/api/admin/recipes/import", Some(json!({ "recipes": [entry("site", "good.example", ATTR)] }))).await?;
    assert_eq!(third["results"][0]["outcome"], "refused", "{third}");
    assert_eq!(third["results"][0]["detail"], "admin");
    Ok(())
}

#[tokio::test]
async fn export_has_the_admin_and_model_recipes_never_shipped_or_retired_and_imports_elsewhere() -> Result<()> {
    let app = App::new(None)?;
    let shipped = omni_browser::recipe::shipped_for_store();
    assert!(!shipped.is_empty());
    app.repo.sync_shipped(&shipped, chrono::Utc::now())?;
    app.send("POST", "/api/admin/recipes/import", Some(json!({ "recipes": [entry("site", "a.example", ATTR), entry("site", "b.example", OTHER)] }))).await?;
    let (_, list) = app.send("GET", "/api/admin/recipes", None).await?;
    let recipes = list["recipes"].as_array().unwrap();
    assert!(recipes.iter().any(|r| r["origin"] == "shipped"), "shipped ones are listed");
    let b = recipes.iter().find(|r| r["scope_key"] == "b.example").unwrap();
    assert_eq!(b["ops"], json!(["attr", "abs", "media"]));
    app.send("POST", &format!("/api/admin/recipes/{}/retire", b["id"]), None).await?;

    let (status, export) = app.send("GET", "/api/admin/recipes/export", None).await?;
    assert_eq!(status, StatusCode::OK);
    let rows = export["recipes"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{export}");
    assert_eq!((rows[0]["scope_kind"].as_str(), rows[0]["scope_key"].as_str()), (Some("site"), Some("a.example")));
    assert!(rows[0]["recipe"]["steps"].is_array(), "the recipe is an object, not text");
    for s in &shipped {
        assert!(!export.to_string().contains(&s.hash), "no shipped recipe in the file");
    }

    // The file goes into a fresh installation as it is.
    let other = App::new(None)?;
    let (status, json) = other.send("POST", "/api/admin/recipes/import", Some(export)).await?;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["results"][0]["outcome"], "inserted");
    Ok(())
}

#[tokio::test]
async fn suspend_and_retire_change_the_recipe_and_refuse_what_cannot_be_done() -> Result<()> {
    let app = App::new(None)?;
    app.repo.sync_shipped(&omni_browser::recipe::shipped_for_store(), chrono::Utc::now())?;
    app.send("POST", "/api/admin/recipes/import", Some(json!({ "recipes": [entry("site", "a.example", ATTR)] }))).await?;
    let list = app.repo.list_recipes(chrono::Utc::now())?;
    let id = list.iter().find(|r| r.origin == "admin").unwrap().id;
    let shipped = list.iter().find(|r| r.origin == "shipped").unwrap().id;

    let (status, _) = app.send("POST", &format!("/api/admin/recipes/{id}/suspend"), Some(json!({ "domain": "A.example" }))).await?;
    assert_eq!(status, StatusCode::OK);
    let after = app.repo.list_recipes(chrono::Utc::now())?;
    let site = &after.iter().find(|r| r.id == id).unwrap().sites[0];
    assert_eq!((site.domain.as_str(), site.state.as_str(), site.suspended_reason.as_deref()), ("a.example", "suspended", Some("failing")));
    let (status, _) = app.send("POST", &format!("/api/admin/recipes/{id}/suspend"), Some(json!({ "domain": "not a domain!" }))).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = app.send("POST", "/api/admin/recipes/9999/suspend", Some(json!({ "domain": "a.example" }))).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no such recipe");

    let (status, _) = app.send("POST", &format!("/api/admin/recipes/{shipped}/retire"), None).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a built-in recipe is not retired by hand");
    assert!(app.repo.list_recipes(chrono::Utc::now())?.iter().any(|r| r.id == shipped && r.state != "retired"));

    let (status, _) = app.send("POST", &format!("/api/admin/recipes/{id}/retire"), None).await?;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = app.send("POST", &format!("/api/admin/recipes/{id}/retire"), None).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "already retired");
    let (status, _) = app.send("POST", &format!("/api/admin/recipes/{id}/suspend"), Some(json!({ "domain": "a.example" }))).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a retired recipe is not suspended");
    Ok(())
}

#[tokio::test]
async fn the_new_post_routes_refuse_a_request_without_the_panel_marker() -> Result<()> {
    let app = App::new(Some(runner(RunNow::Started)))?;
    app.send("POST", "/api/admin/recipes/import", Some(json!({ "recipes": [entry("site", "a.example", ATTR)] }))).await?;
    let id = app.repo.list_recipes(chrono::Utc::now())?[0].id;
    app.repo.record_site_outcome("a.example", SiteOutcome::SiteFailure("X".into()))?;
    app.repo.add_canary("a.example", "https://a.example/x", None, chrono::Utc::now())?;
    for (uri, body) in [
        (format!("/api/admin/recipes/{id}/suspend"), Some(json!({ "domain": "a.example" }))),
        (format!("/api/admin/recipes/{id}/retire"), None),
        ("/api/admin/recipes/import".to_string(), Some(json!({ "recipes": [entry("site", "c.example", OTHER)] }))),
        ("/api/admin/sites/a.example/check".to_string(), None),
    ] {
        let (status, _) = app.send_with("POST", &uri, body, false).await?;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
    let list = app.repo.list_recipes(chrono::Utc::now())?;
    assert_eq!(list.len(), 1, "the refused import stored nothing");
    assert!(list[0].sites.is_empty() && list[0].state != "retired", "and nothing else changed");
    Ok(())
}

// ----------------------------------------------------------------- report

#[tokio::test]
async fn the_report_takes_one_to_ninety_days_and_counts_what_the_period_holds() -> Result<()> {
    let app = App::new(None)?;
    for bad in ["0", "91", "-3"] {
        let (status, _) = app.send("GET", &format!("/api/admin/repair-report?days={bad}"), None).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "days={bad}");
    }
    for good in ["1", "30", "90"] {
        let (status, json) = app.send("GET", &format!("/api/admin/repair-report?days={good}"), None).await?;
        assert_eq!(status, StatusCode::OK, "days={good}");
        assert_eq!(json["days"].as_i64().unwrap().to_string(), good);
    }
    let (_, empty) = app.send("GET", "/api/admin/repair-report", None).await?;
    assert_eq!((empty["days"].as_i64(), empty["sites"].clone()), (Some(30), json!([])), "30 days by default");

    let now = chrono::Utc::now();
    for (i, (ago, url)) in [(2, "https://cdn.example/a/secret.mp4?t=9"), (10, "https://cdn.example/b.mp4")].into_iter().enumerate() {
        let job = app
            .repo
            .enqueue(&omni_core::models::NewJob::new(&format!("https://news.example/s{i}"), &format!("{i}_ANNA_TEST"), "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)?
            .job_id();
        let at = now - chrono::Duration::days(ago);
        let id = app.repo.start_attempt(job, "hold", at)?;
        let end = AttemptEnd { outcome: Some("held".into()), domain: Some("news.example".into()), held_url: Some(url.into()), duration_ms: Some(1000), ..Default::default() };
        app.repo.finish_attempt(id, &end, at)?;
    }
    let (_, week) = app.send("GET", "/api/admin/repair-report?days=7", None).await?;
    let (_, month) = app.send("GET", "/api/admin/repair-report?days=30", None).await?;
    assert_eq!(week["sites"][0]["attempts"], 1, "only the run inside 7 days");
    assert_eq!(month["sites"][0]["attempts"], 2);
    assert_eq!(week["sites"][0]["length_stated_rate"], 0.0, "found, but the page stated no length");
    assert_eq!(week["sites"][0]["next_choices"], 0);
    assert_eq!(empty["sites"], json!([]), "no run, no row: nothing to divide");
    assert!(!month.to_string().contains("secret.mp4"), "a host at most, never an address: {month}");
    Ok(())
}

fn variant(n: usize) -> String {
    format!(r#"{{"dsl_version":1,"steps":[{{"op":"attr","css":"div.p{n}","attr":"data-video"}},{{"op":"abs","from":0}},{{"op":"media","from":1,"referer":"page"}}]}}"#)
}

#[tokio::test]
async fn a_file_with_more_recipes_for_one_scope_than_it_can_hold_is_refused_whole() -> Result<()> {
    let app = App::new(None)?;
    let five: Vec<Value> = (0..5).map(|n| entry("site", "crowded.example", &variant(n))).collect();
    let mut with_good = five.clone();
    with_good.push(entry("site", "fine.example", ATTR));
    let (status, json) = app.send("POST", "/api/admin/recipes/import", Some(json!({ "recipes": with_good }))).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    let message = json["error"]["message"].as_str().unwrap();
    assert!(message.contains("crowded.example") && message.contains("at most 4"), "{message}");
    assert!(app.repo.list_recipes(chrono::Utc::now())?.is_empty(), "not even the good entry was stored");

    let (status, _) = app.send("POST", "/api/admin/recipes/import", Some(json!({ "recipes": &five[..4] }))).await?;
    assert_eq!(status, StatusCode::OK, "four fit");
    Ok(())
}

#[tokio::test]
async fn the_import_results_name_the_model_recipe_an_entry_pushed_out() -> Result<()> {
    let app = App::new(None)?;
    for n in 0..4 {
        let r = omni_browser::recipe::Recipe::from_json(&variant(n)).unwrap();
        app.repo.insert_recipe(
            &omni_core::recipe_score::NewRecipe {
                scope_kind: omni_core::recipe_score::ScopeKind::Site,
                scope_key: "full.example".into(),
                family: None,
                hash: omni_browser::recipe::scope_hash("site", "full.example", &r),
                steps_hash: omni_browser::recipe::steps_hash(&r),
                recipe_json: r.canonical_json(),
                dsl_version: r.dsl_version,
                origin: omni_core::recipe_score::Origin::Llm,
            },
            chrono::Utc::now(),
        )?;
    }
    let (status, json) = app.send("POST", "/api/admin/recipes/import", Some(json!({ "recipes": [entry("site", "full.example", &variant(9))] }))).await?;
    assert_eq!(status, StatusCode::OK, "{json}");
    let evicted = json["results"][0]["evicted"].as_array().unwrap();
    assert_eq!(evicted.len(), 1, "{json}");
    assert_eq!(evicted[0]["scope"], "site full.example");
    assert_eq!(evicted[0]["why"], "its scope was full");
    let id = evicted[0]["id"].as_i64().unwrap();
    let gone = app.repo.list_recipes(chrono::Utc::now())?.into_iter().find(|r| r.id == id).unwrap();
    assert_eq!((gone.state.as_str(), gone.retired_reason.as_deref(), gone.origin.as_str()), ("retired", Some("evicted"), "llm"));
    Ok(())
}

#[tokio::test]
async fn the_import_hint_names_a_domain_the_import_accepts_and_a_barred_recipe_is_not_exported() -> Result<()> {
    let app = App::new(None)?;
    let (_, json) = app.send("POST", "/api/admin/recipes/import", Some(json!({ "recipes": [entry("site", "www.example.gr", ATTR)] }))).await?;
    let message = json["error"]["message"].as_str().unwrap();
    let hint = message.rsplit("such as ").next().unwrap().trim();
    assert_eq!(hint, "example.gr", "{message}");
    let (status, _) = app.send("POST", "/api/admin/recipes/import", Some(json!({ "recipes": [entry("site", hint, ATTR), entry("site", "ok.example", OTHER)] }))).await?;
    assert_eq!(status, StatusCode::OK, "the example the hint gives is accepted");

    let id = app.repo.list_recipes(chrono::Utc::now())?.into_iter().find(|r| r.scope_key == "example.gr").unwrap().id;
    app.repo.recipe_event(id, "example.gr", omni_core::recipe_score::Event::Rejection, chrono::Utc::now())?;
    let (_, export) = app.send("GET", "/api/admin/recipes/export", None).await?;
    let keys: Vec<&str> = export["recipes"].as_array().unwrap().iter().map(|r| r["scope_key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["ok.example"], "MCR said wrong video for the first: it is not carried to another installation");
    Ok(())
}
