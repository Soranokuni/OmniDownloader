use axum::http::header::COOKIE;
use axum::http::{HeaderMap, HeaderValue};
use omni_core::models::User;
use omni_core::repository::Repository;

pub fn extract_session_token(headers: &HeaderMap) -> Option<String> {
    if let Some(cookie_hdr) = headers.get(COOKIE) {
        if let Ok(cookie_str) = cookie_hdr.to_str() {
            for part in cookie_str.split(';') {
                let trimmed = part.trim();
                if let Some(stripped) = trimmed.strip_prefix("omni_session=") {
                    return Some(stripped.to_string());
                }
            }
        }
    }
    None
}

pub fn get_authenticated_user(headers: &HeaderMap, repo: &Repository) -> Option<User> {
    if let Some(token) = extract_session_token(headers) {
        if let Ok(Some(user)) = repo.get_user_by_session_token(&token) {
            return Some(user);
        }
    }
    None
}

pub fn build_session_cookie(token: &str, max_age_days: i64) -> HeaderValue {
    let max_age_secs = max_age_days * 86400;
    let cookie_val = format!(
        "omni_session={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        token, max_age_secs
    );
    HeaderValue::from_str(&cookie_val).unwrap()
}

pub fn build_logout_cookie() -> HeaderValue {
    HeaderValue::from_static("omni_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}
