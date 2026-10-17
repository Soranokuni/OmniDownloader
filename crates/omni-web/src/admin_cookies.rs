//! The admin's Cookies card (plan P3.5): one cookie jar per login-walled site
//! (X, Facebook, Instagram). Upload, list, delete, test. Admin only.
//!
//! A cookie jar is a login. No response here carries a cookie name or value,
//! no audit line or log line quotes one, and a rejected upload says how many
//! lines were unusable, never what they held.

use axum::extract::{Path as AxumPath, State};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use omni_core::cookies::{self, CookieError, MAX_JARS};

use crate::auth::{internal_error, ApiError, RequireAdmin};
use crate::state::AppState;

type JsonResult = Result<Json<Value>, ApiError>;

/// The domain as the panel sends it, as a jar domain (`twitter.com` is kept as `x.com`).
fn jar_domain(raw: &str) -> Result<String, ApiError> {
    let d = cookies::canonical_jar_domain(raw.trim());
    if cookies::is_registrable_domain(&d) {
        Ok(d)
    } else {
        Err(ApiError::bad_request("Enter the site's own domain, for example x.com (no www., no path)."))
    }
}

/// 400, not 404: the route exists, the site just has no jar.
fn no_jar() -> ApiError {
    ApiError::bad_request("There is no cookie jar for this site.")
}

/// Recompute the «Cookies» health line after a change.
fn refresh_health(state: &AppState) {
    cookies::refresh_health(&state.repo, &state.health);
}

pub async fn api_admin_cookies(RequireAdmin(_): RequireAdmin, State(state): State<AppState>) -> JsonResult {
    let jars = state.repo.list_cookie_jars().map_err(internal_error("Could not read the cookie jars."))?;
    let rows: Vec<Value> = jars
        .iter()
        .map(|j| {
            json!({
                "domain": j.domain,
                "cookie_count": j.cookie_count,
                "expires_min": j.expires_min,
                "updated_at": j.updated_at,
                "updated_by": j.updated_by,
                "last_test_at": j.last_test_at,
                "last_test_ok": j.last_test_ok,
                "last_test_detail": j.last_test_detail,
                // Host and path only: the query of a link is not shown.
                "sample": j.last_sample_url.as_deref().map(cookies::host_and_path),
            })
        })
        .collect();
    Ok(Json(json!({
        "jars": rows,
        "max_jars": MAX_JARS,
        "max_kib": cookies::MAX_JAR_BYTES / 1024,
        "can_test": state.cookie_tester.is_some(),
    })))
}

#[derive(Deserialize)]
pub struct UploadPayload {
    domain: String,
    netscape: String,
}

/// Upload a Netscape `cookies.txt`; replaces the site's jar if there is one.
pub async fn api_admin_cookies_put(
    RequireAdmin(admin): RequireAdmin,
    State(state): State<AppState>,
    Json(payload): Json<UploadPayload>,
) -> JsonResult {
    let domain = jar_domain(&payload.domain)?;
    let parsed = cookies::parse_netscape(&domain, &payload.netscape).map_err(|e| match e {
        CookieError::TooLarge | CookieError::NoUsableLine { .. } => ApiError::bad_request(e.to_string()),
    })?;
    let known = state.repo.has_cookie_jar(&domain).map_err(internal_error("Could not read the cookie jars."))?;
    if !known {
        let n = state.repo.list_cookie_jars().map_err(internal_error("Could not read the cookie jars."))?.len();
        if n >= MAX_JARS {
            return Err(ApiError::bad_request(format!("At most {MAX_JARS} cookie jars can be stored. Delete one first.")));
        }
    }
    state
        .repo
        .put_cookie_jar(&domain, &parsed, &admin.email, Utc::now())
        .map_err(internal_error("Could not store the cookie jar."))?;
    // The new values must never reach the log, from this moment on.
    if let Some(r) = &state.redactions {
        r.extend(parsed.values.clone());
    }
    let _ = state.repo.log_audit(
        "INFO",
        "ADMIN",
        &format!("{} uploaded {} cookie(s) for {domain}", admin.email, parsed.count),
    );
    refresh_health(&state);
    Ok(Json(json!({
        "domain": domain,
        "cookie_count": parsed.count,
        "expires_min": parsed.expires_min,
        "skipped_other_site": parsed.other_domain,
        "skipped_malformed": parsed.malformed,
        "replaced": known,
    })))
}

pub async fn api_admin_cookies_delete(
    RequireAdmin(admin): RequireAdmin,
    AxumPath(domain): AxumPath<String>,
    State(state): State<AppState>,
) -> JsonResult {
    let domain = jar_domain(&domain)?;
    let removed = state.repo.delete_cookie_jar(&domain).map_err(internal_error("Could not delete the cookie jar."))?;
    if !removed {
        return Err(no_jar());
    }
    let _ = state.repo.log_audit("INFO", "ADMIN", &format!("{} deleted the cookies for {domain}", admin.email));
    refresh_health(&state);
    Ok(Json(json!({ "deleted": domain })))
}

#[derive(Deserialize)]
pub struct TestPayload {
    url: String,
}

/// «Test»: `yt-dlp --cookies ... --skip-download --dump-json` on a public
/// sample link of the site. The outcome (ok, or the first error line) is stored.
pub async fn api_admin_cookies_test(
    RequireAdmin(admin): RequireAdmin,
    AxumPath(domain): AxumPath<String>,
    State(state): State<AppState>,
    Json(payload): Json<TestPayload>,
) -> JsonResult {
    let domain = jar_domain(&domain)?;
    if !state.repo.has_cookie_jar(&domain).map_err(internal_error("Could not read the cookie jars."))? {
        return Err(no_jar());
    }
    let url = cookies::validate_sample_url(&domain, &payload.url).map_err(ApiError::bad_request)?;
    let Some(tester) = state.cookie_tester.as_ref() else {
        return Err(ApiError::new(axum::http::StatusCode::CONFLICT, "UNAVAILABLE", "The test is not available here."));
    };
    let _ = state.repo.log_audit(
        "INFO",
        "ADMIN",
        &format!("{} tested the cookies for {domain} on {}", admin.email, cookies::host_and_path(&url)),
    );
    let result = tester.test(&domain, &url).await;
    refresh_health(&state);
    match result {
        Ok((ok, detail)) => Ok(Json(json!({ "ok": ok, "detail": detail }))),
        Err(why) => Err(ApiError::bad_request(why)),
    }
}
