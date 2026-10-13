//! The admin's Sites, Recipes and Repair-report cards (plan P10.15a): which
//! sites are degraded or broken and why, which recipes exist and how they
//! score, switching a recipe off, carrying recipes between installations, and
//! the 30-day evidence for moving `repair.mode`. Admin only; hosts only, never
//! a full page address.

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use omni_core::recipe_score::{InsertOutcome, NewRecipe, Origin, ScopeKind, MAX_RECIPE_JSON, SCOPE_CAP};
use omni_core::repository::{AdminRetire, SiteReport};

use crate::auth::{internal_error, ApiError, RequireAdmin};
use crate::state::{AppState, RunNow, TestUrlError};

type JsonResult = Result<Json<Value>, ApiError>;

/// Entries one import may carry.
const MAX_IMPORT_ENTRIES: usize = 100;
/// Longest period the report may ask for: the attempts are kept 90 days.
const MAX_REPORT_DAYS: i64 = 90;
const DEFAULT_REPORT_DAYS: i64 = 30;

fn short(hash: &str) -> String {
    hash.chars().take(12).collect()
}

/// A domain as the panel sends it: lowercase letters, digits, dots and dashes.
fn clean_domain(raw: &str) -> Option<String> {
    let d = raw.trim().to_lowercase();
    let ok = !d.is_empty() && d.len() <= 100 && d.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    ok.then_some(d)
}

/// The share of successes in a site's recent outcomes (`'1'` / `'0'`); `None` when there are none.
fn recent_rate(recent: &str) -> Option<f64> {
    let n = recent.chars().filter(|c| matches!(c, '0' | '1')).count();
    (n > 0).then(|| recent.chars().filter(|c| *c == '1').count() as f64 / n as f64)
}

// ------------------------------------------------------------------ sites

pub async fn api_admin_sites(RequireAdmin(_): RequireAdmin, State(state): State<AppState>) -> JsonResult {
    let rows = state.repo.list_sites(Utc::now()).map_err(internal_error("Could not read the sites."))?;
    let sites: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "domain": r.site.domain,
                "state": r.site.state.as_str(),
                "state_since": r.site.state_since,
                "recent_rate": recent_rate(&r.site.recent),
                "recent_len": r.site.recent.len(),
                "last_ok_at": r.site.last_ok_at,
                "last_fail_at": r.site.last_fail_at,
                "last_code": r.site.last_code,
                "consecutive_fail": r.site.consecutive_fail,
                "llm_breaker_until": r.site.llm_breaker_until,
                "canaries": r.canaries,
                "canaries_ok": r.canaries_ok,
                "canary_checked_at": r.canary_checked_at,
            })
        })
        .collect();
    let mode = state.config.read().await.repair.mode;
    Ok(Json(json!({
        "sites": sites,
        "repair_mode": mode,
        "can_run": state.site_runner.is_some(),
    })))
}

/// «Run now»: one canary check of the site in the background, and its
/// self-heal when it is broken and repair is on. 409 while another canary run
/// (or the yt-dlp update) holds the lock.
pub async fn api_admin_site_check(
    RequireAdmin(admin): RequireAdmin,
    AxumPath(domain): AxumPath<String>,
    State(state): State<AppState>,
) -> JsonResult {
    let Some(domain) = clean_domain(&domain) else { return Err(ApiError::bad_request("Unknown site.")) };
    let known = state.repo.get_site(&domain).map_err(internal_error("Could not read the site."))?;
    if known.is_none() {
        return Err(ApiError::bad_request("Unknown site."));
    }
    let canaries = state.repo.list_canaries_for(&domain).map_err(internal_error("Could not read the site's canaries."))?;
    if canaries.iter().all(|c| c.is_gone()) {
        return Err(ApiError::bad_request("This site has no canary pages to check yet."));
    }
    let Some(runner) = state.site_runner.as_ref() else {
        return Err(ApiError::new(StatusCode::CONFLICT, "UNAVAILABLE", "The check is not available here."));
    };
    match runner.run_now(&domain) {
        RunNow::Busy => Err(ApiError::new(
            StatusCode::CONFLICT,
            "BUSY",
            "Another site check or the yt-dlp update is running. Try again in a few minutes.",
        )),
        RunNow::Cooldown(secs) => {
            let at = (Utc::now() + Duration::seconds(secs as i64)).format("%H:%M UTC");
            Err(ApiError::new(
                StatusCode::CONFLICT,
                "COOLDOWN",
                format!("This site was checked a few minutes ago. It can be checked again in about {} min (at {at}).", secs.div_ceil(60).max(1)),
            ))
        }
        RunNow::Started => {
            let _ = state.repo.log_audit("INFO", "ADMIN", &format!("{} started a check of {domain}", admin.email));
            Ok(Json(json!({ "status": "started" })))
        }
    }
}

// ---------------------------------------------------------------- recipes

/// The step names of a recipe, for the list (`ldjson`, `media`, ...); nothing else of its content.
fn ops_of(recipe_json: &str) -> Vec<String> {
    serde_json::from_str::<Value>(recipe_json)
        .ok()
        .and_then(|v| v.get("steps").and_then(Value::as_array).cloned())
        .map(|steps| steps.iter().filter_map(|s| s.get("op").and_then(Value::as_str).map(str::to_string)).collect())
        .unwrap_or_default()
}

pub async fn api_admin_recipes(RequireAdmin(_): RequireAdmin, State(state): State<AppState>) -> JsonResult {
    let rows = state.repo.list_recipes(Utc::now()).map_err(internal_error("Could not read the recipes."))?;
    let recipes: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.id,
                "hash": short(&r.hash),
                "scope_kind": r.scope_kind,
                "scope_key": r.scope_key,
                "family": r.family,
                "origin": r.origin,
                "state": r.state,
                "created_at": r.created_at,
                "retired_at": r.retired_at,
                "retired_reason": r.retired_reason,
                "ops": ops_of(&r.recipe_json),
                "rank": r.rank,
                "sites": r.sites.into_iter().map(|s| json!({
                    "domain": s.domain,
                    "state": s.state,
                    "suspended_reason": s.suspended_reason,
                    "rank": s.rank,
                    "approvals": s.approvals,
                    "rejections": s.rejections,
                    "uses": s.uses,
                    "last_used_at": s.last_used_at,
                    "last_ok_at": s.last_ok_at,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(Json(json!({ "recipes": recipes })))
}

#[derive(Deserialize)]
pub struct SuspendPayload {
    domain: String,
}

pub async fn api_admin_recipe_suspend(
    RequireAdmin(admin): RequireAdmin,
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
    Json(payload): Json<SuspendPayload>,
) -> JsonResult {
    let Some(domain) = clean_domain(&payload.domain) else {
        return Err(ApiError::bad_request("Give the site's domain."));
    };
    let done = state
        .repo
        .admin_suspend_recipe(id, &domain, Utc::now())
        .map_err(internal_error("Could not suspend the recipe."))?;
    if !done {
        return Err(ApiError::bad_request("No such recipe, or it is retired."));
    }
    let _ = state.repo.log_audit("WARN", "ADMIN", &format!("{} suspended recipe #{id} on {domain}", admin.email));
    Ok(Json(json!({ "status": "ok" })))
}

pub async fn api_admin_recipe_retire(
    RequireAdmin(admin): RequireAdmin,
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
) -> JsonResult {
    match state.repo.admin_retire_recipe(id, Utc::now()).map_err(internal_error("Could not retire the recipe."))? {
        AdminRetire::Retired => {
            let _ = state.repo.log_audit("WARN", "ADMIN", &format!("{} retired recipe #{id}; it cannot come back", admin.email));
            Ok(Json(json!({ "status": "ok" })))
        }
        AdminRetire::NotFound => Err(ApiError::bad_request("No such recipe.")),
        AdminRetire::Shipped => Err(ApiError::bad_request("A built-in recipe cannot be retired; suspend it on a site instead.")),
        AdminRetire::AlreadyRetired => Err(ApiError::bad_request("This recipe is already retired.")),
    }
}

/// The live admin and model recipes, for another installation. Never a built-in one.
pub async fn api_admin_recipes_export(RequireAdmin(_): RequireAdmin, State(state): State<AppState>) -> JsonResult {
    let rows = state.repo.export_recipes().map_err(internal_error("Could not export the recipes."))?;
    let recipes: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "scope_kind": r.scope_kind,
                "scope_key": r.scope_key,
                "family": r.family,
                // The stored text is canonical JSON; fall back to a string rather than drop the entry.
                "recipe": serde_json::from_str::<Value>(&r.recipe_json).unwrap_or(Value::String(r.recipe_json)),
            })
        })
        .collect();
    Ok(Json(json!({ "recipes": recipes })))
}

#[derive(Deserialize)]
pub struct ImportRecipes {
    recipes: Vec<Value>,
}

/// One import entry that parsed and validated, ready to insert.
struct Checked {
    label: String,
    new: NewRecipe,
}

fn check_entry(index: usize, entry: &Value) -> Result<Checked, String> {
    let at = format!("Entry {}", index + 1);
    let Some(obj) = entry.as_object() else { return Err(format!("{at}: not an object")) };
    let text = |key: &str| obj.get(key).and_then(Value::as_str).map(|s| s.trim().to_string());
    let kind = text("scope_kind").ok_or_else(|| format!("{at}: scope_kind is missing"))?;
    let key = text("scope_key").ok_or_else(|| format!("{at}: scope_key is missing"))?;
    let at = format!("{at} ({kind} {key})");
    let families: Vec<&str> = omni_browser::player_family::family_names().collect();
    let family = match obj.get("family") {
        None | Some(Value::Null) => None,
        Some(Value::String(f)) if f.trim().is_empty() => None,
        Some(Value::String(f)) => Some(f.trim().to_string()),
        Some(_) => return Err(format!("{at}: family must be text")),
    };
    if let Some(f) = &family {
        if !families.contains(&f.as_str()) {
            return Err(format!("{at}: \"{f}\" is not a known player family"));
        }
    }
    let scope = match kind.as_str() {
        "site" => {
            let registrable = omni_core::urlnorm::registrable_domain(&format!("https://{key}/"));
            if key != key.to_lowercase() || registrable.as_deref() != Some(key.as_str()) || clean_domain(&key).is_none() {
                return Err(format!("{at}: scope_key must be a site's registrable domain in lowercase, such as example.gr"));
            }
            ScopeKind::Site
        }
        "player" => {
            if !families.contains(&key.as_str()) {
                return Err(format!("{at}: \"{key}\" is not a known player family ({})", families.join(", ")));
            }
            ScopeKind::Player
        }
        _ => return Err(format!("{at}: scope_kind must be \"site\" or \"player\"")),
    };
    let Some(recipe_value) = obj.get("recipe") else { return Err(format!("{at}: recipe is missing")) };
    let recipe = omni_browser::recipe::Recipe::from_json(&recipe_value.to_string()).map_err(|e| format!("{at}: {e}"))?;
    let canonical = recipe.canonical_json();
    if canonical.len() > MAX_RECIPE_JSON {
        return Err(format!("{at}: the recipe is {} bytes, the limit is {MAX_RECIPE_JSON}", canonical.len()));
    }
    Ok(Checked {
        label: format!("{kind} {key}"),
        new: NewRecipe {
            scope_kind: scope,
            scope_key: key.clone(),
            family: if scope == ScopeKind::Player { Some(key.clone()) } else { family },
            hash: omni_browser::recipe::scope_hash(&kind, &key, &recipe),
            steps_hash: omni_browser::recipe::steps_hash(&recipe),
            dsl_version: recipe.dsl_version,
            recipe_json: canonical,
            origin: Origin::Admin,
        },
    })
}

/// All or nothing: every entry must parse and validate, or none is stored and
/// every problem is listed. Stored recipes are admin recipes in the `candidate`
/// state, so the videos they find are still held for MCR.
pub async fn api_admin_recipes_import(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<ImportRecipes>,
) -> JsonResult {
    if payload.recipes.is_empty() {
        return Err(ApiError::bad_request("The file was not imported: it has no recipes."));
    }
    if payload.recipes.len() > MAX_IMPORT_ENTRIES {
        return Err(ApiError::bad_request(format!("The file was not imported: it has more than {MAX_IMPORT_ENTRIES} recipes.")));
    }
    let mut checked = Vec::new();
    let mut problems = Vec::new();
    for (i, entry) in payload.recipes.iter().enumerate() {
        match check_entry(i, entry) {
            Ok(c) => checked.push(c),
            Err(p) => problems.push(p),
        }
    }
    if !problems.is_empty() {
        return Err(ApiError::bad_request(format!("The file was not imported:\n- {}", problems.join("\n- "))));
    }
    // No more than one scope can hold, counted by distinct recipe: the rest could only evict each other.
    let mut per_scope: std::collections::HashMap<(String, String), std::collections::HashSet<&str>> = std::collections::HashMap::new();
    for c in &checked {
        per_scope.entry((c.new.scope_kind.as_str().to_string(), c.new.scope_key.clone())).or_default().insert(c.new.hash.as_str());
    }
    let mut crowded: Vec<String> = per_scope
        .iter()
        .filter(|(_, hashes)| hashes.len() > SCOPE_CAP)
        .map(|((kind, key), hashes)| format!("{kind} {key}: {} recipes, but a scope holds at most {SCOPE_CAP}", hashes.len()))
        .collect();
    crowded.sort();
    if !crowded.is_empty() {
        return Err(ApiError::bad_request(format!("The file was not imported:\n- {}", crowded.join("\n- "))));
    }
    let entries: Vec<NewRecipe> = checked.iter().map(|c| c.new.clone()).collect();
    // One transaction: an error stores nothing.
    let done = state.repo.import_recipes(&entries, Utc::now()).map_err(internal_error("Could not store the recipes; nothing was imported."))?;
    let mut results = Vec::new();
    let (mut added, mut other) = (0, 0);
    for (i, (c, res)) in checked.iter().zip(done).enumerate() {
        let (outcome, detail) = match res.outcome {
            InsertOutcome::Inserted(_) => ("inserted", None),
            InsertOutcome::Merged(_) => ("merged", None),
            InsertOutcome::Revived(_) => ("revived", None),
            InsertOutcome::Refused(why) => ("refused", Some(why)),
            InsertOutcome::OverCap => ("over_cap", None),
        };
        if outcome == "inserted" || outcome == "revived" {
            added += 1;
        } else {
            other += 1;
        }
        let evicted: Vec<Value> = res
            .evicted
            .iter()
            .map(|e| json!({ "id": e.id, "scope": format!("{} {}", e.scope_kind, e.scope_key), "why": e.why }))
            .collect();
        results.push(json!({ "index": i + 1, "scope": c.label, "outcome": outcome, "detail": detail, "evicted": evicted }));
    }
    let _ = state.repo.log_audit(
        "INFO",
        "ADMIN",
        &format!("{} imported recipes: {added} stored, {other} merged, refused or not stored", admin.email),
    );
    Ok(Json(json!({ "status": "ok", "results": results })))
}

// ----------------------------------------------------------------- report

#[derive(Deserialize)]
pub struct ReportQuery {
    days: Option<i64>,
}

fn report_json(r: &SiteReport) -> Value {
    json!({
        "domain": r.domain,
        "attempts": r.attempts,
        "shadow_attempts": r.shadow_attempts,
        "hold_attempts": r.hold_attempts,
        "found": r.found,
        "held": r.held,
        "approved": r.approved,
        "rejected_wrong_video": r.rejected_wrong_video,
        "rejected_other": r.rejected_other,
        "expired": r.expired,
        "next_choices": r.next_choices,
        "length_stated": r.length_stated,
        "length_stated_rate": r.length_stated_rate(),
        "image_known": r.image_known,
        "image_matched": r.image_matched,
        "image_match_rate": r.image_match_rate(),
        "llm_calls": r.llm_calls,
        "tokens_in": r.tokens_in,
        "tokens_out": r.tokens_out,
        "median_duration_ms": r.median_duration_ms,
        "heal_runs": r.heal_runs,
        "heal_found": r.heal_found,
    })
}

pub async fn api_admin_repair_report(
    RequireAdmin(_): RequireAdmin,
    Query(q): Query<ReportQuery>,
    State(state): State<AppState>,
) -> JsonResult {
    let days = q.days.unwrap_or(DEFAULT_REPORT_DAYS);
    if !(1..=MAX_REPORT_DAYS).contains(&days) {
        return Err(ApiError::bad_request(format!("days must be between 1 and {MAX_REPORT_DAYS}.")));
    }
    let report = state
        .repo
        .repair_report(Utc::now() - Duration::days(days))
        .map_err(internal_error("Could not build the repair report."))?;
    let mode = state.config.read().await.repair.mode;
    Ok(Json(json!({
        "days": days,
        "repair_mode": mode,
        "image_match_max": omni_core::repository::IMAGE_MATCH_MAX,
        "sites": report.iter().map(report_json).collect::<Vec<_>>(),
    })))
}

// -------------------------------------------------------------- test url

/// Longest address the Test URL accepts.
const MAX_TEST_URL: usize = 2048;

/// What the Test URL card needs to know before a run: whether it can run and
/// what is left of today's model calls (a ticked run counts toward the cap).
pub async fn api_admin_test_url_info(RequireAdmin(_): RequireAdmin, State(state): State<AppState>) -> JsonResult {
    let cfg = state.config.read().await;
    let model_set = cfg.repair.llm_model.as_deref().is_some_and(|m| !m.trim().is_empty());
    let (cap, available) = (cfg.repair.llm_max_calls_per_day, cfg.repair.llm_enabled && model_set);
    drop(cfg);
    let day = chrono::Local::now().format("%Y-%m-%d").to_string();
    let used = state.repo.llm_usage_for(&day).map_err(internal_error("Could not read the model's calls."))?.0;
    Ok(Json(json!({
        "can_run": state.url_tester.is_some(),
        "llm_available": available,
        "llm_cap": cap,
        "llm_left": (i64::from(cap) - used).max(0),
    })))
}

#[derive(Deserialize)]
pub struct TestUrlPayload {
    url: String,
    #[serde(default)]
    use_llm: bool,
}

/// «Run»: the dry run of the normal path and the repair ladder on one public
/// address. Downloads, queues and delivers nothing. 409 while another test runs.
pub async fn api_admin_test_url(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<TestUrlPayload>,
) -> JsonResult {
    let url = payload.url.trim();
    if url.is_empty() || url.len() > MAX_TEST_URL {
        return Err(ApiError::bad_request("Enter one address (http or https)."));
    }
    let (_, host) = omni_core::net::parse_public_url(url)
        .map_err(|_| ApiError::bad_request("Only a public http or https address can be tested."))?;
    let Some(tester) = state.url_tester.as_ref() else {
        return Err(ApiError::new(StatusCode::CONFLICT, "UNAVAILABLE", "The test is not available here."));
    };
    let _ = state.repo.log_audit("INFO", "ADMIN", &format!("{} ran Test URL on {host}", admin.email));
    match tester.test_url(url, payload.use_llm).await {
        Ok(report) => Ok(Json(report)),
        Err(TestUrlError::Busy) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "BUSY",
            "Another test is running. Wait for it to finish (up to three minutes).",
        )),
        Err(TestUrlError::Refused(why)) => Err(ApiError::bad_request(format!("This address cannot be tested: {why}."))),
    }
}
