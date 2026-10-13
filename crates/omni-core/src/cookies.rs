//! Cookie jars for login-walled sites (plan P3.5): X, Facebook, Instagram.
//!
//! An admin uploads a Netscape `cookies.txt` exported from a browser logged
//! into the station's account; the downloader hands it to yt-dlp for jobs on
//! that site. Everything here is pure: parsing, the domain rules and the
//! health verdict. Storage is `Repository` (sealed with DPAPI); the file yt-dlp
//! reads lives in the downloader.
//!
//! A cookie jar is a login. No function here logs, formats or returns a cookie
//! name or value in an error: a rejected upload says how many lines were
//! unusable, never what they held.

use chrono::{DateTime, Duration, Utc};

use crate::health::Check;

/// Largest jar accepted, in bytes.
pub const MAX_JAR_BYTES: usize = 256 * 1024;
/// Most jars kept at once.
pub const MAX_JARS: usize = 20;
/// A jar whose earliest cookie expires within this many days turns the health line amber.
pub const EXPIRY_WARN_DAYS: i64 = 7;
/// A jar not tested for this many days turns the health line amber.
pub const STALE_TEST_DAYS: i64 = 30;
/// Longest test detail kept.
const MAX_DETAIL_CHARS: usize = 200;

/// The registrable domains a jar for `jar_domain` covers: X answers on both
/// names, so one jar serves `x.com` and `twitter.com` links.
pub fn jar_covers(jar_domain: &str) -> &'static [&'static str] {
    match jar_domain {
        "x.com" | "twitter.com" => &["x.com", "twitter.com"],
        _ => &[],
    }
}

/// The jar a link on `registrable` belongs to: `twitter.com` is kept in the `x.com` jar.
pub fn canonical_jar_domain(registrable: &str) -> String {
    let d = registrable.trim().to_ascii_lowercase();
    if d == "twitter.com" {
        "x.com".to_string()
    } else {
        d
    }
}

/// The jar domain for a link: its registrable domain, with the X alias applied.
pub fn jar_domain_for_url(url: &str) -> Option<String> {
    crate::urlnorm::registrable_domain(url).map(|d| canonical_jar_domain(&d))
}

/// Whether `raw` is a bare registrable domain (`x.com`, `example.co.uk`), the
/// only form a jar may be stored under.
pub fn is_registrable_domain(raw: &str) -> bool {
    let d = raw.trim();
    if d.is_empty() || d.len() > 100 || d != d.to_ascii_lowercase() {
        return false;
    }
    if !d.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') || !d.contains('.') {
        return false;
    }
    if d.starts_with(['.', '-']) || d.ends_with(['.', '-']) || d.contains("..") {
        return false;
    }
    // Not an address.
    if d.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return false;
    }
    crate::urlnorm::registrable_domain(&format!("https://{d}/")).as_deref() == Some(d)
}

/// Whether a cookie's domain field belongs to the jar for `jar_domain`.
fn domain_belongs(cookie_domain: &str, jar_domain: &str) -> bool {
    let host = cookie_domain.trim().trim_start_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    let covered = jar_covers(jar_domain);
    let ours: Vec<&str> = if covered.is_empty() { vec![jar_domain] } else { covered.to_vec() };
    ours.iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// Why an upload was refused. Never carries cookie content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CookieError {
    #[error("The cookie file is larger than {} KiB.", MAX_JAR_BYTES / 1024)]
    TooLarge,
    #[error("The cookie file has no usable line for {domain} ({other_domain} line(s) belong to other sites, {malformed} line(s) are not in the Netscape cookies.txt format).")]
    NoUsableLine { domain: String, other_domain: usize, malformed: usize },
}

/// A validated jar, ready to seal and store.
#[derive(Clone)]
pub struct ParsedJar {
    /// The Netscape text, only the lines for this domain, `\n` line ends.
    pub text: String,
    pub count: usize,
    /// Earliest expiry among the cookies that expire; `None` for session-only jars.
    pub expires_min: Option<DateTime<Utc>>,
    /// Lines that were not Netscape cookie lines.
    pub malformed: usize,
    /// Lines for other sites, dropped.
    pub other_domain: usize,
    /// Every cookie value, for the log redactor.
    pub values: Vec<String>,
}

// Not derived: a Debug print of a jar must not show the cookies.
impl std::fmt::Debug for ParsedJar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedJar").field("count", &self.count).field("expires_min", &self.expires_min).finish_non_exhaustive()
    }
}

/// Parse a Netscape `cookies.txt` for the jar `domain` (already a canonical
/// jar domain). Comments and blank lines are skipped, `#HttpOnly_` lines are
/// cookies, lines for other sites are dropped, malformed lines are dropped and
/// counted. A file with no usable line is an error.
pub fn parse_netscape(domain: &str, raw: &str) -> Result<ParsedJar, CookieError> {
    parse_netscape_at(domain, raw, Utc::now())
}

/// The cookies whose expiry says when the login ends, per site. Everything
/// else in a jar (analytics, consent, preferences) expires on its own schedule
/// and says nothing about whether the account still gets in.
fn login_cookie_names(jar_domain: &str) -> &'static [&'static str] {
    match jar_domain {
        "x.com" | "twitter.com" => &["auth_token", "ct0"],
        "facebook.com" => &["c_user", "xs"],
        "instagram.com" => &["sessionid"],
        _ => &[],
    }
}

/// [`parse_netscape`] with the upload time given.
///
/// `expires_min` is the earliest expiry of the site's login cookies. For other
/// sites, or when none of them is in the file, it is the earliest cookie that
/// does not expire within 24 hours of `now`.
pub fn parse_netscape_at(domain: &str, raw: &str, now: DateTime<Utc>) -> Result<ParsedJar, CookieError> {
    if raw.len() > MAX_JAR_BYTES {
        return Err(CookieError::TooLarge);
    }
    let mut kept: Vec<String> = Vec::new();
    let mut values: Vec<String> = Vec::new();
    // (name, expiry) of every kept cookie; 0 is a session cookie.
    let mut expiries: Vec<(String, i64)> = Vec::new();
    let (mut malformed, mut other_domain) = (0usize, 0usize);

    for line in raw.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            continue;
        }
        let body = match line.strip_prefix("#HttpOnly_") {
            Some(rest) => rest,
            None if line.starts_with('#') => continue,
            None => line,
        };
        let f: Vec<&str> = body.split('\t').collect();
        let well_formed = f.len() == 7
            // yt-dlp refuses a line whose subdomain flag disagrees with the domain's
            // leading dot, and then quotes the whole line, value included, in its error.
            && f[1] == if f[0].starts_with('.') { "TRUE" } else { "FALSE" }
            && f[2].starts_with('/')
            && matches!(f[3], "TRUE" | "FALSE")
            && !f[5].is_empty()
            && !line.chars().any(|c| c.is_control() && c != '\t');
        let expiry = if well_formed { (!f[4].is_empty() && f[4].bytes().all(|b| b.is_ascii_digit())).then(|| f[4].parse::<i64>().ok()).flatten() } else { None };
        let Some(expiry) = expiry else {
            malformed += 1;
            continue;
        };
        if !domain_belongs(f[0], domain) {
            other_domain += 1;
            continue;
        }
        expiries.push((f[5].to_string(), expiry));
        if !f[6].is_empty() {
            values.push(f[6].to_string());
        }
        kept.push(line.to_string());
    }

    if kept.is_empty() {
        return Err(CookieError::NoUsableLine { domain: domain.to_string(), other_domain, malformed });
    }
    let mut text = String::from("# Netscape HTTP Cookie File\n");
    for l in &kept {
        text.push_str(l);
        text.push('\n');
    }
    let login = login_cookie_names(domain);
    let logins: Vec<i64> = expiries.iter().filter(|(n, _)| login.contains(&n.as_str())).map(|(_, e)| *e).collect();
    let expires_min = if !logins.is_empty() {
        // A login cookie that is a session cookie ends with the browser: no date.
        logins.iter().copied().filter(|e| *e > 0).min()
    } else {
        let horizon = now.timestamp() + 24 * 3600;
        expiries.iter().map(|(_, e)| *e).filter(|e| *e > horizon).min()
    };
    Ok(ParsedJar {
        count: kept.len(),
        text,
        expires_min: expires_min.and_then(|e| DateTime::from_timestamp(e, 0)),
        malformed,
        other_domain,
        values,
    })
}

/// Every cookie value in Netscape `text`, for scrubbing yt-dlp's output and the log.
pub fn values_of(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.starts_with('#') || l.starts_with("#HttpOnly_"))
        .filter_map(|l| l.split('\t').nth(6))
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .collect()
}

/// A jar as the admin sees it. No contents.
#[derive(Debug, Clone)]
pub struct CookieJarInfo {
    pub domain: String,
    pub cookie_count: i64,
    pub expires_min: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub updated_by: String,
    pub last_test_at: Option<DateTime<Utc>>,
    pub last_test_ok: Option<bool>,
    pub last_test_detail: Option<String>,
    /// The last address tested; shown as host and path only.
    pub last_sample_url: Option<String>,
}

/// `host/path` of an address: no scheme, query, fragment or credentials.
pub fn host_and_path(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest).to_string()
}

/// One line of yt-dlp output as stored: addresses cut at their query, the
/// jar's own values removed, at most 200 characters.
pub fn scrub_detail(line: &str, values: &[String]) -> String {
    let mut out = line.trim().to_string();
    for v in values.iter().filter(|v| v.len() >= 6) {
        out = out.replace(v.as_str(), "***");
    }
    let cut: Vec<String> = out
        .split(' ')
        .map(|w| match w.find("://") {
            Some(_) => w.split(['?', '#']).next().unwrap_or("").to_string(),
            None => w.to_string(),
        })
        .collect();
    cut.join(" ").chars().take(MAX_DETAIL_CHARS).collect()
}

/// Check a sample address for a jar test: http(s), no credentials, a public
/// host name (not an address or `localhost`) that belongs to the jar's domain.
/// Returns the address trimmed. The reason names no part of the address.
pub fn validate_sample_url(domain: &str, raw: &str) -> Result<String, &'static str> {
    let url = raw.trim();
    if url.is_empty() || url.len() > 500 || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("The sample link is empty, too long or contains spaces.");
    }
    let Some((scheme, rest)) = url.split_once("://") else { return Err("The sample link must start with https://.") };
    if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
        return Err("The sample link must start with https://.");
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err("The sample link must not contain a user name or password.");
    }
    let host = authority.split(':').next().unwrap_or("").to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".local") || host.ends_with(".internal") || host.chars().all(|c| c.is_ascii_digit() || c == '.') || host.contains('[') {
        return Err("The sample link must be a public web address.");
    }
    if jar_domain_for_url(url).as_deref() != Some(canonical_jar_domain(domain).as_str()) {
        return Err("The sample link is not on the cookie jar's site.");
    }
    Ok(url.to_string())
}

/// The «Cookies» health line: amber when any jar's earliest cookie expires
/// within a week (or has), its last test failed, or it has not been tested for
/// 30 days. Names domains only.
pub fn health_check(jars: &[CookieJarInfo], now: DateTime<Utc>) -> Check {
    if jars.is_empty() {
        return Check::ok("No cookie jars");
    }
    let mut problems: Vec<String> = Vec::new();
    for j in jars {
        let mut why: Vec<&str> = Vec::new();
        if j.expires_min.is_some_and(|e| e <= now + Duration::days(EXPIRY_WARN_DAYS)) {
            why.push("expires within 7 days");
        }
        if j.last_test_ok == Some(false) {
            why.push("last test failed");
        }
        // A jar never tested counts from the day it was uploaded.
        let since = j.last_test_at.or(j.updated_at);
        if since.map_or(true, |t| now - t > Duration::days(STALE_TEST_DAYS)) {
            why.push("not tested for 30 days");
        }
        if !why.is_empty() {
            problems.push(format!("{}: {}", j.domain, why.join(", ")));
        }
    }
    if problems.is_empty() {
        Check::ok(format!("{} cookie jar(s) fine", jars.len()))
    } else {
        Check::degraded(problems.join("; "))
    }
}

/// Recompute the «Cookies» line from the stored jars.
pub fn refresh_health(repo: &crate::repository::Repository, health: &crate::health::HealthState) {
    match repo.list_cookie_jars() {
        Ok(jars) => health.set(crate::health::checks::COOKIES, health_check(&jars, Utc::now())),
        Err(e) => health.set(crate::health::checks::COOKIES, Check::degraded(format!("Cookie jars could not be read: {e:#}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const VALUE_A: &str = "tok-secret-value-AAA111";
    const VALUE_B: &str = "tok-secret-value-BBB222";

    fn sample() -> String {
        format!(
            "# Netscape HTTP Cookie File\n\
             \n\
             # a comment\n\
             .x.com\tTRUE\t/\tTRUE\t1893456000\tauth_token\t{VALUE_A}\n\
             #HttpOnly_.x.com\tTRUE\t/\tTRUE\t1800000000\tct0\t{VALUE_B}\n\
             .twitter.com\tTRUE\t/\tTRUE\t0\tguest\tsession-value-1\n\
             .example.org\tTRUE\t/\tFALSE\t1893456000\tother\tforeign-value-1\n\
             this line is garbage\n"
        )
    }

    #[test]
    fn a_valid_file_is_kept_for_its_own_domain_only() {
        let p = parse_netscape("x.com", &sample()).unwrap();
        assert_eq!(p.count, 3, "x.com, #HttpOnly_ x.com and the twitter.com alias");
        assert_eq!(p.other_domain, 1);
        assert_eq!(p.malformed, 1);
        assert!(p.text.contains(VALUE_A) && p.text.contains("#HttpOnly_.x.com"));
        assert!(!p.text.contains("foreign-value-1"), "a foreign site's cookie must not be stored");
        assert!(!p.text.contains("garbage"));
        assert_eq!(p.expires_min, Some(Utc.timestamp_opt(1_800_000_000, 0).unwrap()), "session cookies (0) do not count");
        assert!(p.values.contains(&VALUE_A.to_string()) && p.values.contains(&VALUE_B.to_string()));
    }

    #[test]
    fn a_subdomain_flag_that_disagrees_with_the_domain_is_refused() {
        // yt-dlp rejects these and quotes the line, value and all, in its error text.
        for bad in [
            ".x.com\tFALSE\t/\tTRUE\t0\tn\tvalue-123456\n",
            "x.com\tTRUE\t/\tTRUE\t0\tn\tvalue-123456\n",
        ] {
            assert!(parse_netscape("x.com", bad).is_err(), "{bad:?}");
        }
        assert!(parse_netscape("x.com", "x.com\tFALSE\t/\tTRUE\t0\tn\tvalue-123456\n").is_ok());
        // An expiry is digits only: no sign, no space.
        for e in ["+5", " 5", "5 ", "1e9", ""] {
            let line = format!(".x.com\tTRUE\t/\tTRUE\t{e}\tn\tvalue-123456\n");
            assert!(parse_netscape("x.com", &line).is_err(), "{e:?}");
        }
    }

    #[test]
    fn the_expiry_that_counts_is_the_login_cookies() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let t = now.timestamp();
        let line = |n: &str, e: i64| format!(".x.com\tTRUE\t/\tTRUE\t{e}\t{n}\tvalue-123456\n");
        // A tracking cookie expiring in 2 days must not beat a login valid for a year.
        let raw = format!("{}{}{}", line("_ga", t + 2 * 86400), line("auth_token", t + 365 * 86400), line("ct0", t + 300 * 86400));
        let p = parse_netscape_at("x.com", &raw, now).unwrap();
        assert_eq!(p.expires_min.unwrap().timestamp(), t + 300 * 86400);
        // Facebook and Instagram have their own.
        let fb = format!(".facebook.com\tTRUE\t/\tTRUE\t{}\txs\tvalue-123456\n.facebook.com\tTRUE\t/\tTRUE\t{}\tdatr\tvalue-123456\n", t + 90 * 86400, t + 5 * 86400);
        assert_eq!(parse_netscape_at("facebook.com", &fb, now).unwrap().expires_min.unwrap().timestamp(), t + 90 * 86400);
        let ig = format!(".instagram.com\tTRUE\t/\tTRUE\t{}\tsessionid\tvalue-123456\n.instagram.com\tTRUE\t/\tTRUE\t{}\tmid\tvalue-123456\n", t + 60 * 86400, t + 86400 * 2);
        assert_eq!(parse_netscape_at("instagram.com", &ig, now).unwrap().expires_min.unwrap().timestamp(), t + 60 * 86400);
        // A session login cookie: no date.
        let session = format!("{}{}", line("auth_token", 0), line("_ga", t + 5 * 86400));
        assert_eq!(parse_netscape_at("x.com", &session, now).unwrap().expires_min, None);
        // No login cookie present, or another site: the earliest not expiring within 24 h.
        let other = format!(
            ".news.example\tTRUE\t/\tTRUE\t{}\ta\tvalue-123456\n.news.example\tTRUE\t/\tTRUE\t{}\tb\tvalue-123456\n.news.example\tTRUE\t/\tTRUE\t{}\tc\tvalue-123456\n",
            t + 3600,
            t + 10 * 86400,
            t + 20 * 86400
        );
        assert_eq!(parse_netscape_at("news.example", &other, now).unwrap().expires_min.unwrap().timestamp(), t + 10 * 86400);
        let no_login = format!("{}{}", line("_ga", t + 3600), line("guest_id", t + 40 * 86400));
        assert_eq!(parse_netscape_at("x.com", &no_login, now).unwrap().expires_min.unwrap().timestamp(), t + 40 * 86400);
    }

    #[test]
    fn the_x_alias_works_both_ways_and_other_sites_stay_apart() {
        assert_eq!(canonical_jar_domain("twitter.com"), "x.com");
        assert_eq!(jar_domain_for_url("https://mobile.twitter.com/a/status/1").as_deref(), Some("x.com"));
        assert_eq!(jar_domain_for_url("https://www.facebook.com/watch/?v=1").as_deref(), Some("facebook.com"));
        // A twitter.com jar uploaded under that name covers x.com lines too.
        let tw = ".x.com\tTRUE\t/\tTRUE\t0\tn\tvalue-123456\n";
        assert!(parse_netscape("twitter.com", tw).is_ok());
        // Suffix lookalikes are not the domain.
        let fake = ".notx.com\tTRUE\t/\tTRUE\t0\tn\tvalue-123456\n";
        assert!(matches!(parse_netscape("x.com", fake), Err(CookieError::NoUsableLine { other_domain: 1, .. })));
    }

    #[test]
    fn a_file_with_no_usable_line_is_refused_without_echoing_it() {
        let e = parse_netscape("x.com", "just some text\n.example.org\tTRUE\t/\tFALSE\t0\tsecretname\tsecretvalue\n").unwrap_err();
        let msg = e.to_string();
        assert!(!msg.contains("secretvalue") && !msg.contains("secretname"), "{msg}");
        assert!(matches!(e, CookieError::NoUsableLine { malformed: 1, other_domain: 1, .. }));
        assert!(parse_netscape("x.com", "").is_err());
        assert!(parse_netscape("x.com", "# only a comment\n").is_err());
    }

    #[test]
    fn malformed_fields_are_not_cookies() {
        // 6 fields, bad flag, bad expiry, negative expiry, empty name, relative path.
        for bad in [
            ".x.com\tTRUE\t/\tTRUE\t0\tname\n",
            ".x.com\tYES\t/\tTRUE\t0\tn\tvalue-123456\n",
            ".x.com\tTRUE\t/\tTRUE\tsoon\tn\tvalue-123456\n",
            ".x.com\tTRUE\t/\tTRUE\t-5\tn\tvalue-123456\n",
            ".x.com\tTRUE\t/\tTRUE\t0\t\tvalue-123456\n",
            ".x.com\tTRUE\tpath\tTRUE\t0\tn\tvalue-123456\n",
        ] {
            assert!(parse_netscape("x.com", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn crlf_files_are_accepted_and_stored_with_plain_newlines() {
        let crlf = ".x.com\tTRUE\t/\tTRUE\t0\tn\tvalue-123456\r\n";
        let p = parse_netscape("x.com", crlf).unwrap();
        assert!(!p.text.contains('\r'));
    }

    #[test]
    fn a_jar_over_the_size_cap_is_refused() {
        let big = format!(".x.com\tTRUE\t/\tTRUE\t0\tn\t{}\n", "v".repeat(MAX_JAR_BYTES));
        assert_eq!(parse_netscape("x.com", &big).unwrap_err(), CookieError::TooLarge);
    }

    #[test]
    fn values_are_read_back_from_the_stored_text() {
        let p = parse_netscape("x.com", &sample()).unwrap();
        let v = values_of(&p.text);
        assert!(v.contains(&VALUE_A.to_string()) && v.contains(&VALUE_B.to_string()));
        assert_eq!(v.len(), p.values.len());
    }

    #[test]
    fn debug_of_a_jar_shows_no_cookies() {
        let p = parse_netscape("x.com", &sample()).unwrap();
        let shown = format!("{p:?}");
        assert!(!shown.contains(VALUE_A) && !shown.contains("auth_token"), "{shown}");
    }

    #[test]
    fn only_bare_registrable_domains_are_accepted() {
        for ok in ["x.com", "facebook.com", "instagram.com", "example.co.uk"] {
            assert!(is_registrable_domain(ok), "{ok}");
        }
        for bad in ["", "com", "www.x.com", "X.com", "x.com/path", "https://x.com", "127.0.0.1", "a b.com", ".x.com", "x..com"] {
            assert!(!is_registrable_domain(bad), "{bad}");
        }
    }

    #[test]
    fn details_lose_query_strings_and_cookie_values() {
        let line = format!("ERROR: [twitter] Unable to download https://x.com/a/status/1?s=20&t={VALUE_A} using {VALUE_B}");
        let out = scrub_detail(&line, &[VALUE_A.to_string(), VALUE_B.to_string()]);
        assert!(!out.contains('?') && !out.contains(VALUE_A) && !out.contains(VALUE_B), "{out}");
        assert!(out.contains("https://x.com/a/status/1"));
        assert!(scrub_detail(&"e".repeat(500), &[]).chars().count() <= MAX_DETAIL_CHARS);
        assert_eq!(host_and_path("https://u:p@x.com/a/status/1?s=20#f"), "x.com/a/status/1");
    }

    #[test]
    fn a_sample_link_must_be_public_and_on_the_jars_site() {
        assert!(validate_sample_url("x.com", "https://x.com/a/status/1").is_ok());
        assert!(validate_sample_url("x.com", "https://twitter.com/a/status/1?s=20").is_ok());
        assert!(validate_sample_url("facebook.com", "https://m.facebook.com/watch/?v=1").is_ok());
        for bad in [
            "https://x.com.evil.example/a",
            "https://notx.com/a",
            "http://127.0.0.1/a",
            "https://localhost/a",
            "https://user:pw@x.com/a",
            "ftp://x.com/a",
            "x.com/a",
            "https://x.com/a b",
            "",
        ] {
            assert!(validate_sample_url("x.com", bad).is_err(), "{bad}");
        }
        assert!(validate_sample_url("facebook.com", "https://x.com/a").is_err());
    }

    fn jar(domain: &str) -> CookieJarInfo {
        let now = Utc::now();
        CookieJarInfo {
            domain: domain.into(),
            cookie_count: 3,
            expires_min: Some(now + Duration::days(200)),
            updated_at: Some(now - Duration::days(2)),
            updated_by: "admin".into(),
            last_test_at: Some(now - Duration::hours(5)),
            last_test_ok: Some(true),
            last_test_detail: None,
            last_sample_url: None,
        }
    }

    #[test]
    fn the_health_line_states() {
        use crate::health::Health;
        let now = Utc::now();
        assert_eq!(health_check(&[], now).state, Health::Ok);
        assert_eq!(health_check(&[jar("x.com")], now).state, Health::Ok);

        let mut expiring = jar("x.com");
        expiring.expires_min = Some(now + Duration::days(3));
        let c = health_check(&[expiring], now);
        assert_eq!(c.state, Health::Degraded);
        assert!(c.detail.unwrap().contains("x.com: expires within 7 days"));

        let mut expired = jar("x.com");
        expired.expires_min = Some(now - Duration::days(1));
        assert_eq!(health_check(&[expired], now).state, Health::Degraded);

        let mut failed = jar("facebook.com");
        failed.last_test_ok = Some(false);
        assert_eq!(health_check(&[jar("x.com"), failed], now).state, Health::Degraded);

        let mut stale = jar("x.com");
        stale.last_test_at = Some(now - Duration::days(31));
        assert_eq!(health_check(&[stale], now).state, Health::Degraded);

        // Never tested, uploaded yesterday: fine. Never tested, uploaded 40 days ago: amber.
        let mut fresh = jar("x.com");
        fresh.last_test_at = None;
        fresh.last_test_ok = None;
        assert_eq!(health_check(&[fresh.clone()], now).state, Health::Ok);
        fresh.updated_at = Some(now - Duration::days(40));
        assert_eq!(health_check(&[fresh], now).state, Health::Degraded);

        // Session-only jar: no expiry, no complaint about it.
        let mut session = jar("x.com");
        session.expires_min = None;
        assert_eq!(health_check(&[session], now).state, Health::Ok);
    }
}
