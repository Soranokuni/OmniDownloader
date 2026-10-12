//! Recipe language v1 (plan P10.8): a small, closed, data-only description of
//! where a page's video address is, with a validator and a deterministic
//! interpreter that runs on a saved [`PageSnapshot`].
//!
//! A recipe is data, never code: no step evaluates JavaScript, the page's or
//! the recipe's. The only network access is `Fetch`, through an injected
//! [`Fetcher`]. Live actions (`Click`, `Wait`, `ScrollTo`) are validated here
//! and run later against a live page; a recipe that has them returns
//! [`RecipeOutcome::NeedsLivePage`] from [`run`].

use crate::evidence::PageSnapshot;
use async_trait::async_trait;
use regex::{Regex, RegexBuilder};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Instant;
use tracing::debug;

pub const DSL_VERSION: u32 = 1;
const MAX_STEPS: usize = 8;
const MAX_FIELD_CHARS: usize = 200;
const MAX_TEMPLATE_CHARS: usize = 300;
const MAX_WAIT_MS: u32 = 8000;
const MAX_LIVE_ACTIONS: usize = 2;
const REGEX_SIZE_LIMIT: usize = 1 << 16;
/// Total text a `Re` step with a css selector may collect from matched elements.
/// Largest fetched body that is read.
const MAX_BODY: usize = 2 << 20;
/// Descendant spaces and `>` allowed in one selector.
const MAX_COMBINATORS: usize = 6;
/// Comma-separated selectors allowed in one css field.
const MAX_SELECTOR_LIST: usize = 4;
/// Deepest page the interpreter will run selectors on.
const MAX_DOM_DEPTH: usize = 256;
/// Page limits checked on the raw text before any HTML parse.
const MAX_ELEMENTS: usize = 30_000;
const MAX_ATTR_BYTES: usize = 256 << 10;
const MAX_CLASS_TOKENS: usize = 64;
/// Sum over elements of the attribute bytes on their ancestor path.
const MAX_PATH_ATTR_BYTES: u64 = 16 << 20;
const TEXT_BUDGET: usize = 2 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub dsl_version: u32,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Referer {
    Page,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /// First network entry (2xx or no status) whose url matches; value = url.
    Net {
        url_re: String,
        #[serde(default)]
        mime: Option<String>,
    },
    /// Attribute of the first element of the saved html matching `css`.
    Attr { css: String, attr: String },
    /// Text of the first element matching `css`, parsed as JSON; the string at `ptr`.
    Json { css: String, ptr: String },
    /// First string under `field` in the page's ld+json blocks.
    Ldjson { field: String },
    /// First capture group of `re` over the matched elements' text, or over html and scripts.
    Re {
        #[serde(default)]
        css: Option<String>,
        re: String,
    },
    /// GET the url of step `from`; value = `ptr` into the JSON body or first capture of `re`.
    Fetch {
        from: usize,
        #[serde(default)]
        ptr: Option<String>,
        #[serde(default)]
        re: Option<String>,
    },
    /// Resolve the value of `from` against the page address.
    Abs { from: usize },
    /// Undo JSON string escapes and HTML entities.
    Unescape { from: usize },
    /// `{n}` slots bound to earlier step values.
    Template { t: String },
    Click { css: String },
    Wait { ms: u32 },
    ScrollTo { css: String },
    /// Output; the last step, exactly one.
    Media { from: usize, referer: Referer },
}

impl Step {
    fn is_live(&self) -> bool {
        matches!(self, Step::Click { .. } | Step::Wait { .. } | Step::ScrollTo { .. })
    }
    fn produces_value(&self) -> bool {
        !self.is_live() && !matches!(self, Step::Media { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecipeError {
    #[error("recipe is not valid JSON of the expected shape: {0}")]
    Parse(String),
    #[error("unsupported dsl_version {0}")]
    Version(u32),
    #[error("a recipe has 1 to 8 steps, found {0}")]
    StepCount(usize),
    #[error("a recipe needs exactly one media step, as the last step")]
    MediaPlacement,
    #[error("at most one fetch step is allowed")]
    TooManyFetch,
    #[error("at most two live actions are allowed")]
    TooManyLiveActions,
    #[error("live actions must come before any source step")]
    LiveAfterSource,
    #[error("step {step}: {field} is longer than {max} characters")]
    TooLong { step: usize, field: &'static str, max: usize },
    #[error("step {step}: wait is longer than 8000 ms")]
    WaitTooLong { step: usize },
    #[error("step {step}: regular expression does not compile")]
    BadRegex { step: usize },
    #[error("step {step}: the regular expression needs exactly one capture group")]
    CaptureGroups { step: usize },
    #[error("step {step}: css selector does not parse")]
    BadCss { step: usize },
    #[error("step {step}: css selector uses a construct that is not allowed (:has, ~, +, escapes)")]
    SelectorNotAllowed { step: usize },
    #[error("step {step}: not a valid JSON pointer")]
    BadPointer { step: usize },
    #[error("step {step}: refers to a step that is not an earlier value step")]
    BadFrom { step: usize },
    #[error("step {step}: bad template")]
    BadTemplate { step: usize },
    #[error("step {step}: fetch needs exactly one of ptr and re")]
    FetchArgs { step: usize },
}

fn build_regex(s: &str) -> Option<Regex> {
    RegexBuilder::new(s).size_limit(REGEX_SIZE_LIMIT).build().ok()
}

/// Only a small, plain subset of CSS is accepted: no sibling combinators, no
/// `:has`, `:is`, `:where`, pseudo-elements, comments or escapes. Anything the
/// scanner does not recognise is refused, so a clever spelling cannot slip by.
fn selector_allowed(v: &str) -> bool {
    if !v.is_ascii() || v.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return false;
    }
    let c: Vec<char> = v.chars().collect();
    let nth = Regex::new(r"^\s*([+-]?\d*[nN](\s*[+-]\s*\d+)?|[+-]?\d+|even|odd)\s*$").expect("static regex");
    let mut quote: Option<char> = None;
    let mut bracket = false;
    let mut not_depth = 0usize;
    let mut combinators = 0usize;
    let mut commas = 0usize;
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else if ch == '\\' || ch == '"' || ch == '\'' {
                return false;
            }
            i += 1;
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            _ if ch.is_ascii_alphanumeric() => {}
            '_' | '#' | '.' | '-' | '*' | '^' | '$' | '|' | '=' => {}
            // Inside :not( only a single compound is allowed, so matching stays linear.
            ' ' | ',' | '>' if not_depth > 0 && !bracket => return false,
            // in a bracket a space may only precede the closing `]` (no ` i` / ` s` flags)
            ' ' if bracket => {
                if c[i + 1..].iter().find(|x| **x != ' ') != Some(&']') {
                    return false;
                }
            }
            ' ' => {
                let prev = c[..i].iter().rev().find(|x| **x != ' ');
                let next = c[i + 1..].iter().find(|x| **x != ' ');
                let joins = |x: Option<&char>| matches!(x, None | Some('>') | Some(','));
                if !joins(prev) && !joins(next) {
                    combinators += 1;
                }
            }
            '>' => combinators += 1,
            ',' => {
                commas += 1;
                combinators = 0;
            }
            '[' => bracket = true,
            ']' => bracket = false,
            '~' => {
                if !(bracket && c.get(i + 1) == Some(&'=')) {
                    return false;
                }
            }
            ')' => {
                if not_depth == 0 {
                    return false;
                }
                not_depth -= 1;
            }
            ':' => {
                if c.get(i + 1) == Some(&':') {
                    return false;
                }
                let mut j = i + 1;
                while j < c.len() && (c[j].is_ascii_alphanumeric() || c[j] == '-') {
                    j += 1;
                }
                let ident: String = c[i + 1..j].iter().collect();
                match ident.as_str() {
                    "first-child" | "last-child" => i = j - 1,
                    "not" if c.get(j) == Some(&'(') && not_depth == 0 => {
                        not_depth += 1;
                        i = j;
                    }
                    "nth-child" if c.get(j) == Some(&'(') => {
                        let Some(k) = c[j..].iter().position(|x| *x == ')').map(|k| j + k) else {
                            return false;
                        };
                        let inner: String = c[j + 1..k].iter().collect();
                        if !nth.is_match(&inner) {
                            return false;
                        }
                        i = k;
                    }
                    _ => return false,
                }
            }
            _ => return false,
        }
        if combinators > MAX_COMBINATORS || commas >= MAX_SELECTOR_LIST {
            return false;
        }
        i += 1;
    }
    quote.is_none() && !bracket && not_depth == 0
}

fn valid_pointer(p: &str) -> bool {
    if p.is_empty() {
        return true;
    }
    if !p.starts_with('/') {
        return false;
    }
    let mut chars = p.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '~' && !matches!(chars.peek(), Some('0') | Some('1')) {
            return false;
        }
    }
    true
}

/// Template slot numbers in order, or None when it has a stray brace.
fn template_slots(t: &str) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    let mut chars = t.chars();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                let mut digits = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(d) if d.is_ascii_digit() => digits.push(d),
                        _ => return None,
                    }
                }
                out.push(digits.parse().ok()?);
            }
            '}' => return None,
            _ => {}
        }
    }
    Some(out)
}

impl Recipe {
    pub fn from_json(s: &str) -> Result<Recipe, RecipeError> {
        let r: Recipe = serde_json::from_str(s).map_err(|e| RecipeError::Parse(e.to_string()))?;
        r.validate()?;
        Ok(r)
    }

    pub fn validate(&self) -> Result<(), RecipeError> {
        if self.dsl_version != DSL_VERSION {
            return Err(RecipeError::Version(self.dsl_version));
        }
        let n = self.steps.len();
        if n == 0 || n > MAX_STEPS {
            return Err(RecipeError::StepCount(n));
        }
        let media = self.steps.iter().filter(|s| matches!(s, Step::Media { .. })).count();
        if media != 1 || !matches!(self.steps.last(), Some(Step::Media { .. })) {
            return Err(RecipeError::MediaPlacement);
        }
        if self.steps.iter().filter(|s| matches!(s, Step::Fetch { .. })).count() > 1 {
            return Err(RecipeError::TooManyFetch);
        }
        if self.steps.iter().filter(|s| s.is_live()).count() > MAX_LIVE_ACTIONS {
            return Err(RecipeError::TooManyLiveActions);
        }
        let mut seen_source = false;
        for s in &self.steps {
            if s.is_live() {
                if seen_source {
                    return Err(RecipeError::LiveAfterSource);
                }
            } else if s.produces_value() {
                seen_source = true;
            }
        }
        for (i, s) in self.steps.iter().enumerate() {
            self.validate_step(i, s)?;
        }
        Ok(())
    }

    fn validate_step(&self, i: usize, s: &Step) -> Result<(), RecipeError> {
        let len = |field: &'static str, v: &str| {
            if v.chars().count() > MAX_FIELD_CHARS {
                Err(RecipeError::TooLong { step: i, field, max: MAX_FIELD_CHARS })
            } else {
                Ok(())
            }
        };
        let css = |v: &str| {
            len("css", v)?;
            if !selector_allowed(v) {
                return Err(RecipeError::SelectorNotAllowed { step: i });
            }
            Selector::parse(v).map(|_| ()).map_err(|_| RecipeError::BadCss { step: i })
        };
        let ptr = |v: &str| {
            len("ptr", v)?;
            if valid_pointer(v) {
                Ok(())
            } else {
                Err(RecipeError::BadPointer { step: i })
            }
        };
        let from_ok = |f: usize| {
            if f < i && self.steps[f].produces_value() {
                Ok(())
            } else {
                Err(RecipeError::BadFrom { step: i })
            }
        };
        let one_group = |field: &'static str, v: &str| {
            len(field, v)?;
            let r = build_regex(v).ok_or(RecipeError::BadRegex { step: i })?;
            if r.captures_len() != 2 {
                return Err(RecipeError::CaptureGroups { step: i });
            }
            Ok(())
        };
        match s {
            Step::Net { url_re, mime } => {
                len("url_re", url_re)?;
                if let Some(m) = mime {
                    len("mime", m)?;
                }
                build_regex(url_re).ok_or(RecipeError::BadRegex { step: i })?;
            }
            Step::Attr { css: c, attr } => {
                css(c)?;
                len("attr", attr)?;
            }
            Step::Json { css: c, ptr: p } => {
                css(c)?;
                ptr(p)?;
            }
            Step::Ldjson { field } => len("field", field)?,
            Step::Re { css: c, re } => {
                if let Some(c) = c {
                    css(c)?;
                }
                one_group("re", re)?;
            }
            Step::Fetch { from, ptr: p, re } => {
                from_ok(*from)?;
                match (p, re) {
                    (Some(p), None) => ptr(p)?,
                    (None, Some(re)) => one_group("re", re)?,
                    _ => return Err(RecipeError::FetchArgs { step: i }),
                }
            }
            Step::Abs { from } | Step::Unescape { from } => from_ok(*from)?,
            Step::Template { t } => {
                if t.chars().count() > MAX_TEMPLATE_CHARS {
                    return Err(RecipeError::TooLong { step: i, field: "template", max: MAX_TEMPLATE_CHARS });
                }
                let slots = template_slots(t).ok_or(RecipeError::BadTemplate { step: i })?;
                for k in slots {
                    from_ok(k).map_err(|_| RecipeError::BadTemplate { step: i })?;
                }
            }
            Step::Click { css: c } | Step::ScrollTo { css: c } => css(c)?,
            Step::Wait { ms } => {
                if *ms > MAX_WAIT_MS {
                    return Err(RecipeError::WaitTooLong { step: i });
                }
            }
            Step::Media { from, .. } => from_ok(*from)?,
        }
        Ok(())
    }

    /// Serde form with fixed field order; the same recipe always gives the same text.
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Lowercase hex SHA-256 of the canonical form.
    pub fn hash(&self) -> String {
        let d = Sha256::digest(self.canonical_json().as_bytes());
        d.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Hash of a recipe in one scope (`site`, `player` or `generic`): the same
/// steps in another scope are another recipe in the store.
pub fn scope_hash(scope_kind: &str, scope_key: &str, recipe: &Recipe) -> String {
    let text = format!(
        r#"{{"scope_kind":{},"scope_key":{},"recipe":{}}}"#,
        serde_json::to_string(scope_kind).unwrap_or_default(),
        serde_json::to_string(scope_key).unwrap_or_default(),
        recipe.canonical_json()
    );
    let d = Sha256::digest(text.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// The steps alone, without a scope; one set of steps can serve many sites.
pub fn steps_hash(recipe: &Recipe) -> String {
    recipe.hash()
}

/// First http(s) video address in a text: a `.m3u8` or `.mp4` file, JSON-escaped
/// slashes allowed (`Unescape` follows).
const MEDIA_URL_RE: &str = r#"https?:(?:\\?/){2}[^"'\s<>]+?\.(?:m3u8|mp4)(?:\?[^"'\s<>]*)?"#;

fn media_page(from: usize) -> Step {
    Step::Media { from, referer: Referer::Page }
}

fn attr_recipe(css: &str, attr: &str) -> Recipe {
    Recipe {
        dsl_version: DSL_VERSION,
        steps: vec![
            Step::Attr { css: css.to_string(), attr: attr.to_string() },
            Step::Abs { from: 0 },
            media_page(1),
        ],
    }
}

fn re_recipe(css: Option<&str>, re: String) -> Recipe {
    Recipe {
        dsl_version: DSL_VERSION,
        steps: vec![Step::Re { css: css.map(str::to_string), re }, Step::Unescape { from: 0 }, media_page(1)],
    }
}

/// The generic recipes that ship with the daemon: the usual places a news page
/// puts its video. Built in code so they are checked by the compiler's tests,
/// and always active (they carry no learned score of their own).
pub fn shipped() -> Vec<(&'static str, Recipe)> {
    vec![
        (
            "ldjson_content_url",
            Recipe {
                dsl_version: DSL_VERSION,
                steps: vec![Step::Ldjson { field: "contentUrl".to_string() }, Step::Abs { from: 0 }, media_page(1)],
            },
        ),
        ("og_video", attr_recipe(r#"meta[property="og:video"]"#, "content")),
        ("og_video_url", attr_recipe(r#"meta[property="og:video:url"]"#, "content")),
        ("og_video_secure_url", attr_recipe(r#"meta[property="og:video:secure_url"]"#, "content")),
        ("next_data_media", re_recipe(Some("script#__NEXT_DATA__"), format!("({MEDIA_URL_RE})"))),
        ("nuxt_media", re_recipe(None, format!(r"__NUXT__[\s\S]*?({MEDIA_URL_RE})"))),
        (
            "jwplayer_sources",
            re_recipe(
                None,
                format!(r#"sources["']?\s*:[\s\S]*?["']?file["']?\s*:\s*["']({MEDIA_URL_RE})["']"#),
            ),
        ),
        ("videojs_source", attr_recipe("video source[src]", "src")),
        ("data_src", attr_recipe("video[data-src], source[data-src]", "data-src")),
        ("data_hls", attr_recipe("[data-hls]", "data-hls")),
        ("data_video_url", attr_recipe("[data-video-url]", "data-video-url")),
    ]
}

/// `shipped()` in the form the recipe store syncs: scope `generic`, the name as key.
pub fn shipped_for_store() -> Vec<omni_core::recipe_score::ShippedRecipe> {
    shipped()
        .into_iter()
        .map(|(name, r)| omni_core::recipe_score::ShippedRecipe {
            name: name.to_string(),
            hash: scope_hash("generic", name, &r),
            steps_hash: steps_hash(&r),
            recipe_json: r.canonical_json(),
            dsl_version: r.dsl_version,
        })
        .collect()
}

/// Fetches one address for a `Fetch` step. P10.8 ships no network version.
#[async_trait]
pub trait Fetcher: Send + Sync {
    async fn get(&self, url: &str, deadline: Instant) -> anyhow::Result<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeOutcome {
    Media { url: String, referer: Option<String> },
    NoValue { at_step: usize },
    NeedsLivePage,
    Rejected(String),
}

enum Stop {
    NoValue(usize),
    Rejected(String),
}

enum Flow {
    Continue,
    Media { url: String, referer: Option<String> },
}

/// The host rule: http or https, a host the page saw or one of the page's own
/// registrable domain, never `localhost` or a private address literal, and no
/// explicit port the page did not use. The caller must use the returned,
/// serialized URL, never the raw string: the parser and other URL readers can
/// disagree about the host of the raw text. The error holds the host only.
fn check_url(snap: &PageSnapshot, raw: &str) -> Result<reqwest::Url, String> {
    use reqwest::Url;
    let (url, host) = omni_core::net::parse_public_url(raw)?;
    if let Some(port) = url.port() {
        let seen = snap
            .network
            .iter()
            .filter_map(|n| Url::parse(&n.url).ok())
            .any(|u| u.port() == Some(port) && u.host_str().map(|h| h.to_ascii_lowercase()) == url.host_str().map(|h| h.to_ascii_lowercase()));
        if !seen {
            return Err(format!("host {host} port {port} was not seen on the page"));
        }
    }
    // IPv6 literals are bracketed in observed_hosts as host_str() gives them.
    let key = url.host_str().unwrap_or("").to_ascii_lowercase();
    if snap.observed_hosts().contains(&key) {
        return Ok(url);
    }
    let reg = omni_core::urlnorm::registrable_domain(&format!("{}://{}/", url.scheme(), key));
    let own = [Some(snap.page_url.as_str()), snap.final_url.as_deref()]
        .into_iter()
        .flatten()
        .filter_map(omni_core::urlnorm::registrable_domain)
        .any(|d| Some(&d) == reg.as_ref());
    if own {
        Ok(url)
    } else {
        Err(format!("host {host} was not seen on the page"))
    }
}

fn selector(s: &str) -> Option<Selector> {
    Selector::parse(s).ok()
}

/// First capture over the text of the matched elements; stops at the deadline
/// and after `budget` bytes of collected text.
fn re_in_elements(doc: &Html, sel: &Selector, re: &Regex, mut budget: usize, deadline: Instant) -> Option<String> {
    for el in doc.select(sel) {
        if Instant::now() > deadline {
            return None;
        }
        let mut text = String::new();
        for piece in el.text() {
            if piece.len() > budget {
                return None;
            }
            budget -= piece.len();
            text.push_str(piece);
        }
        if let Some(c) = first_capture(re, &text) {
            return Some(c);
        }
    }
    None
}

fn first_capture(re: &Regex, hay: &str) -> Option<String> {
    re.captures(hay).and_then(|c| c.get(1)).map(|m| m.as_str().to_string())
}

fn json_string_at(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn ldjson_find(v: &Value, field: &str, depth: usize) -> Option<String> {
    if depth > 4 {
        return None;
    }
    match v {
        Value::Object(map) => {
            if let Some(f) = map.get(field) {
                match f {
                    Value::String(s) => return Some(s.clone()),
                    Value::Array(a) => {
                        if let Some(s) = a.iter().find_map(|x| x.as_str()) {
                            return Some(s.to_string());
                        }
                    }
                    _ => {}
                }
            }
            map.get("@graph").and_then(|g| ldjson_find(g, field, depth + 1))
        }
        Value::Array(a) => a.iter().find_map(|x| ldjson_find(x, field, depth + 1)),
        _ => None,
    }
}

/// JSON string escapes, then HTML entities.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let hex4 = |at: usize| -> Option<u32> {
        let h: String = chars.get(at..at + 4)?.iter().collect();
        if h.chars().all(|c| c.is_ascii_hexdigit()) {
            u32::from_str_radix(&h, 16).ok()
        } else {
            None
        }
    };
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\\' || i + 1 >= chars.len() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let simple = match chars[i + 1] {
            '/' => Some('/'),
            '\\' => Some('\\'),
            '"' => Some('"'),
            'n' => Some('\n'),
            't' => Some('\t'),
            'r' => Some('\r'),
            'b' => Some('\u{8}'),
            'f' => Some('\u{c}'),
            _ => None,
        };
        if let Some(c) = simple {
            out.push(c);
            i += 2;
            continue;
        }
        if chars[i + 1] != 'u' {
            out.push('\\');
            i += 1;
            continue;
        }
        match hex4(i + 2) {
            Some(hi) if (0xD800..0xDC00).contains(&hi) => {
                let lo = if chars.get(i + 6) == Some(&'\\') && chars.get(i + 7) == Some(&'u') {
                    hex4(i + 8).filter(|l| (0xDC00..0xE000).contains(l))
                } else {
                    None
                };
                match lo {
                    Some(lo) => {
                        let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                        out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                        i += 12;
                    }
                    None => {
                        out.push('\u{fffd}');
                        i += 6;
                    }
                }
            }
            Some(cp) => {
                out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                i += 6;
            }
            None => {
                out.push('\\');
                i += 1;
            }
        }
    }
    decode_entities(&out)
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let decoded = rest.find(';').filter(|e| *e <= 10).and_then(|e| {
            let name = &rest[1..e];
            let ch = match name {
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "nbsp" => Some('\u{a0}'),
                _ => name.strip_prefix('#').and_then(|n| {
                    let cp = match n.strip_prefix(['x', 'X']) {
                        Some(h) => u32::from_str_radix(h, 16).ok()?,
                        None => n.parse().ok()?,
                    };
                    char::from_u32(cp)
                }),
            };
            ch.map(|c| (c, e + 1))
        });
        match decoded {
            Some((c, used)) => {
                out.push(c);
                rest = &rest[used..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn net_value(snap: &PageSnapshot, url_re: &str, mime: Option<&str>) -> Option<String> {
    let re = build_regex(url_re)?;
    let want = mime.map(|m| m.to_ascii_lowercase());
    snap.network
        .iter()
        .filter(|n| n.status.map_or(true, |s| (200..300).contains(&s)))
        .filter(|n| match &want {
            Some(m) => n.mime.as_deref().is_some_and(|x| x.to_ascii_lowercase().starts_with(m.as_str())),
            None => true,
        })
        .find(|n| re.is_match(&n.url))
        .map(|n| n.url.clone())
}

/// The parsed page, built the first time a step needs it.
fn page<'a>(doc: &'a mut Option<Html>, snap: &PageSnapshot) -> Option<&'a Html> {
    if doc.is_none() {
        *doc = Some(Html::parse_document(snap.html.as_deref()?));
    }
    doc.as_ref()
}

/// Evaluate the source and transform steps in `range` (no `Fetch`, no live
/// actions). Holds the parsed page only inside this call.
fn eval_range(
    recipe: &Recipe,
    snap: &PageSnapshot,
    values: &mut [Option<String>],
    range: std::ops::Range<usize>,
    deadline: Instant,
) -> Result<Flow, Stop> {
    let mut doc: Option<Html> = None;
    let needs_page = range.clone().any(|i| {
        matches!(&recipe.steps[i], Step::Attr { .. } | Step::Json { .. } | Step::Re { css: Some(_), .. })
    });
    if needs_page {
        if let Some(h) = snap.html.as_deref() {
            if !prescan(h) {
                return Err(Stop::Rejected("page too complex".to_string()));
            }
            let parsed = Html::parse_document(h);
            let (depth, path_bytes) = dom_cost(&parsed, MAX_DOM_DEPTH, MAX_PATH_ATTR_BYTES);
            if depth > MAX_DOM_DEPTH || path_bytes > MAX_PATH_ATTR_BYTES {
                return Err(Stop::Rejected("page too complex".to_string()));
            }
            doc = Some(parsed);
        }
    }
    for i in range {
        if Instant::now() > deadline {
            return Err(Stop::Rejected("deadline exceeded".to_string()));
        }
        let value: Option<String> = match &recipe.steps[i] {
            Step::Net { url_re, mime } => net_value(snap, url_re, mime.as_deref()),
            Step::Attr { css, attr } => (|| {
                let sel = selector(css)?;
                let el = page(&mut doc, snap)?.select(&sel).next()?;
                el.value().attr(attr).map(|s| s.to_string())
            })(),
            Step::Json { css, ptr } => (|| {
                let sel = selector(css)?;
                let text: String = page(&mut doc, snap)?.select(&sel).next()?.text().collect();
                let v: Value = serde_json::from_str(text.trim()).ok()?;
                json_string_at(&v, ptr)
            })(),
            Step::Ldjson { field } => snap
                .ld_json
                .iter()
                .filter_map(|b| serde_json::from_str::<Value>(b).ok())
                .find_map(|v| ldjson_find(&v, field, 0)),
            Step::Re { css, re } => (|| {
                let re = build_regex(re)?;
                match css {
                    Some(css) => {
                        let sel = selector(css)?;
                        re_in_elements(page(&mut doc, snap)?, &sel, &re, TEXT_BUDGET, deadline)
                    }
                    None => snap.html.iter().chain(snap.scripts.iter()).find_map(|h| first_capture(&re, h)),
                }
            })(),
            Step::Fetch { .. } => None,
            Step::Abs { from } => values[*from].as_deref().and_then(|v| {
                let base = snap.final_url.as_deref().unwrap_or(&snap.page_url);
                let base = reqwest::Url::parse(base).ok()?;
                base.join(v).ok().map(|u| u.to_string())
            }),
            Step::Unescape { from } => values[*from].as_deref().map(unescape),
            Step::Template { t } => {
                let mut out = String::new();
                let mut missing = false;
                let mut rest = t.as_str();
                while let Some(p) = rest.find('{') {
                    out.push_str(&rest[..p]);
                    let end = rest[p..].find('}').map(|e| p + e);
                    let slot = end.and_then(|e| rest[p + 1..e].parse::<usize>().ok());
                    match (end, slot.and_then(|k| values.get(k).and_then(|v| v.as_deref()))) {
                        (Some(e), Some(v)) => {
                            out.push_str(v);
                            rest = &rest[e + 1..];
                        }
                        _ => {
                            missing = true;
                            break;
                        }
                    }
                }
                if missing {
                    None
                } else {
                    out.push_str(rest);
                    let url = check_url(snap, &out).map_err(Stop::Rejected)?;
                    Some(url.to_string())
                }
            }
            // Live steps only matter to `run_after_live`, where they are done already.
            Step::Click { .. } | Step::Wait { .. } | Step::ScrollTo { .. } => continue,
            Step::Media { from, referer } => {
                let Some(v) = values[*from].clone() else {
                    return Err(Stop::NoValue(i));
                };
                let v = check_url(snap, &v).map_err(Stop::Rejected)?.to_string();
                let referer = match referer {
                    Referer::Page => Some(snap.final_url.clone().unwrap_or_else(|| snap.page_url.clone())),
                    Referer::None => None,
                };
                return Ok(Flow::Media { url: v, referer });
            }
        };
        match value {
            Some(v) => values[i] = Some(v),
            None => return Err(Stop::NoValue(i)),
        }
    }
    Ok(Flow::Continue)
}

/// One recipe evaluation thread at a time in the whole process: an abandoned
/// (timed-out) thread keeps its permit until it really ends, so stuck threads
/// cannot pile up.
fn eval_gate() -> std::sync::Arc<tokio::sync::Semaphore> {
    static GATE: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1))).clone()
}

async fn eval_permit(deadline: Instant) -> Option<tokio::sync::OwnedSemaphorePermit> {
    let fut = eval_gate().acquire_owned();
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), fut).await.ok()?.ok()
}

/// Greatest element depth of the page and the sum over elements of the
/// attribute bytes on their ancestor path, counted without recursion; stops
/// early once either is past its limit.
fn dom_cost(doc: &Html, depth_limit: usize, path_limit: u64) -> (usize, u64) {
    let mut seen: std::collections::HashMap<_, (usize, u64)> = std::collections::HashMap::new();
    let (mut max, mut total) = (0usize, 0u64);
    for node in doc.tree.nodes() {
        let own: u64 = node
            .value()
            .as_element()
            .map(|e| e.attrs().map(|(k, v)| (k.len() + v.len()) as u64).sum())
            .unwrap_or(0);
        let (d, path) = match node.parent() {
            Some(p) => {
                // A parent created later than its children (the HTML parser's
                // adoption agency clones formatting elements) is not seen
                // yet: count its whole path rather than as free.
                let (pd, pp) = seen.get(&p.id()).copied().unwrap_or_else(|| {
                    let path: u64 = std::iter::once(p)
                        .chain(p.ancestors())
                        .map(|n| {
                            n.value()
                                .as_element()
                                .map(|e| e.attrs().map(|(k, v)| (k.len() + v.len()) as u64).sum::<u64>())
                                .unwrap_or(0)
                        })
                        .sum();
                    (p.ancestors().count(), path)
                });
                (pd + 1, pp + own)
            }
            None => (0, own),
        };
        seen.insert(node.id(), (d, path));
        max = max.max(d);
        if node.value().is_element() {
            total += path;
        }
        if max > depth_limit || total > path_limit {
            break;
        }
    }
    (max, total)
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta" | "source" | "track" | "wbr"
    )
}

/// Elements whose end tag may be left out; a new one of the same kind closes the open one.
fn closes_itself(name: &str) -> bool {
    matches!(name, "p" | "li" | "dt" | "dd" | "tr" | "td" | "th" | "option")
}

/// One linear pass over the raw page, before any HTML parse: counts elements,
/// follows nesting with a stack of open names, and checks attribute and
/// class-list sizes. False means the page is too complex to parse.
fn prescan(html: &str) -> bool {
    let b = html.as_bytes();
    let mut stack: Vec<String> = Vec::new();
    let mut elements = 0usize;
    let mut i = 0;
    while let Some(off) = b[i..].iter().position(|x| *x == b'<') {
        i += off;
        let rest = &b[i..];
        if rest.starts_with(b"<!--") {
            match find_from(b, i + 4, b"-->") {
                Some(e) => i = e + 3,
                None => return true,
            }
            continue;
        }
        if rest.starts_with(b"<!") || rest.starts_with(b"<?") {
            match b[i..].iter().position(|x| *x == b'>') {
                Some(e) => i += e + 1,
                None => return true,
            }
            continue;
        }
        let closing = rest.get(1) == Some(&b'/');
        let name_start = i + if closing { 2 } else { 1 };
        let mut j = name_start;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'-' || b[j] == b':') {
            j += 1;
        }
        if j == name_start || !b[name_start].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let name = html[name_start..j].to_ascii_lowercase();
        if closing {
            match b[j..].iter().position(|x| *x == b'>') {
                Some(e) => i = j + e + 1,
                None => return true,
            }
            let from = stack.len().saturating_sub(8);
            if let Some(pos) = stack[from..].iter().rposition(|n| *n == name) {
                stack.truncate(from + pos);
            }
            continue;
        }
        elements += 1;
        if elements > MAX_ELEMENTS {
            return false;
        }
        // attributes, up to the closing '>'
        let mut k = j;
        let mut self_closing = false;
        loop {
            while k < b.len() && (b[k].is_ascii_whitespace() || b[k] == b'/') {
                self_closing = b[k] == b'/';
                k += 1;
            }
            if k >= b.len() {
                return true;
            }
            if b[k] == b'>' {
                k += 1;
                break;
            }
            self_closing = false;
            let ns = k;
            while k < b.len() && !b[k].is_ascii_whitespace() && !matches!(b[k], b'=' | b'>' | b'/') {
                k += 1;
            }
            let attr = html[ns..k].to_ascii_lowercase();
            while k < b.len() && b[k].is_ascii_whitespace() {
                k += 1;
            }
            if b.get(k) != Some(&b'=') {
                continue;
            }
            k += 1;
            while k < b.len() && b[k].is_ascii_whitespace() {
                k += 1;
            }
            let (vs, ve) = match b.get(k) {
                Some(&q) if q == b'"' || q == b'\'' => {
                    let vs = k + 1;
                    match b[vs..].iter().position(|x| *x == q) {
                        Some(e) => {
                            k = vs + e + 1;
                            (vs, vs + e)
                        }
                        None => return b.len() - vs <= MAX_ATTR_BYTES,
                    }
                }
                _ => {
                    let vs = k;
                    while k < b.len() && !b[k].is_ascii_whitespace() && b[k] != b'>' {
                        k += 1;
                    }
                    (vs, k)
                }
            };
            if ve - vs > MAX_ATTR_BYTES {
                return false;
            }
            if attr == "class" && html[vs..ve].split_ascii_whitespace().count() > MAX_CLASS_TOKENS {
                return false;
            }
        }
        i = k;
        if matches!(name.as_str(), "script" | "style" | "textarea" | "title") {
            // raw text: skip to the matching end tag
            match find_ci(b, i, format!("</{name}").as_bytes()) {
                Some(e) => i = e,
                None => return true,
            }
            continue;
        }
        if is_void(&name) || self_closing {
            continue;
        }
        if closes_itself(&name) && stack.last() == Some(&name) {
            stack.pop();
        }
        stack.push(name);
        if stack.len() > MAX_DOM_DEPTH {
            return false;
        }
    }
    true
}

fn find_from(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    b.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|p| from + p)
}

/// ASCII case-insensitive search; `needle` is lowercase.
fn find_ci(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    b.get(from..)?
        .windows(needle.len())
        .position(|w| w.iter().zip(needle).all(|(x, y)| x.to_ascii_lowercase() == *y))
        .map(|p| from + p)
}

/// Value from a fetched body: at most `MAX_BODY` bytes are read.
fn extract_fetched(mut body: String, ptr: Option<&str>, re: Option<&str>) -> Option<String> {
    if body.len() > MAX_BODY {
        let mut end = MAX_BODY;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
    }
    match (ptr, re) {
        (Some(p), _) => serde_json::from_str::<Value>(&body).ok().and_then(|v| json_string_at(&v, p)),
        (None, Some(r)) => build_regex(r).and_then(|r| first_capture(&r, &body)),
        _ => None,
    }
}

/// `eval_range` on a blocking thread, abandoned at the deadline. The thread
/// also checks the deadline itself, so it ends soon after.
async fn eval_blocking(
    recipe: &Recipe,
    snap: &PageSnapshot,
    values: &mut Vec<Option<String>>,
    range: std::ops::Range<usize>,
    deadline: Instant,
) -> Result<Flow, Stop> {
    let Some(permit) = eval_permit(deadline).await else {
        return Err(Stop::Rejected("busy".to_string()));
    };
    let (r, s) = (recipe.clone(), snap.clone());
    let mut owned = std::mem::take(values);
    let job = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let flow = eval_range(&r, &s, &mut owned, range, deadline);
        (owned, flow)
    });
    match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), job).await {
        Ok(Ok((v, flow))) => {
            *values = v;
            flow
        }
        Ok(Err(_)) => Err(Stop::Rejected("evaluation failed".to_string())),
        Err(_) => Err(Stop::Rejected("deadline exceeded".to_string())),
    }
}

fn stop_outcome(s: Stop) -> RecipeOutcome {
    match s {
        Stop::NoValue(i) => RecipeOutcome::NoValue { at_step: i },
        Stop::Rejected(m) => RecipeOutcome::Rejected(m),
    }
}

/// One live step of a recipe, as the browser executes it (P10.11c).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveAction {
    Click(String),
    Wait(u32),
    ScrollTo(String),
}

/// The recipe's live steps, in step order.
pub fn live_actions(recipe: &Recipe) -> Vec<LiveAction> {
    recipe
        .steps
        .iter()
        .filter_map(|s| match s {
            Step::Click { css } => Some(LiveAction::Click(css.clone())),
            Step::Wait { ms } => Some(LiveAction::Wait(*ms)),
            Step::ScrollTo { css } => Some(LiveAction::ScrollTo(css.clone())),
            _ => None,
        })
        .collect()
}

/// A canonical text for an action list, equal exactly when the lists are
/// equal (JSON strings, so no selector can imitate a separator).
pub fn action_key(actions: &[LiveAction]) -> String {
    let parts: Vec<Value> = actions
        .iter()
        .map(|a| match a {
            LiveAction::Click(c) => serde_json::json!(["click", c]),
            LiveAction::Wait(ms) => serde_json::json!(["wait", ms]),
            LiveAction::ScrollTo(c) => serde_json::json!(["scroll_to", c]),
        })
        .collect();
    Value::Array(parts).to_string()
}

/// Run a recipe on a saved page. Pure apart from the fetcher and the deadline.
pub async fn run(recipe: &Recipe, snap: &PageSnapshot, fetcher: &dyn Fetcher, deadline: Instant) -> RecipeOutcome {
    if recipe.validate().is_ok() && recipe.steps.iter().any(|s| s.is_live()) {
        return RecipeOutcome::NeedsLivePage;
    }
    run_value_steps(recipe, snap, fetcher, deadline).await
}

/// `run` on a snapshot taken after the recipe's live steps were executed in
/// the browser. All live actions count as done before any value step runs:
/// live steps produce no value and no value step can refer to one, so the
/// order between them cannot matter. A recipe without live steps gives the
/// same outcome as `run`.
pub async fn run_after_live(
    recipe: &Recipe,
    snap: &PageSnapshot,
    fetcher: &dyn Fetcher,
    deadline: Instant,
) -> RecipeOutcome {
    run_value_steps(recipe, snap, fetcher, deadline).await
}

async fn run_value_steps(recipe: &Recipe, snap: &PageSnapshot, fetcher: &dyn Fetcher, deadline: Instant) -> RecipeOutcome {
    if let Err(e) = recipe.validate() {
        return RecipeOutcome::Rejected(format!("invalid recipe: {e}"));
    }
    let n = recipe.steps.len();
    let mut values: Vec<Option<String>> = vec![None; n];
    let fetch_at = recipe.steps.iter().position(|s| matches!(s, Step::Fetch { .. }));
    let flow = match eval_blocking(recipe, snap, &mut values, 0..fetch_at.unwrap_or(n), deadline).await {
        Ok(f) => f,
        Err(s) => return stop_outcome(s),
    };
    let flow = match (flow, fetch_at) {
        (Flow::Continue, Some(f)) => {
            let Step::Fetch { from, ptr, re } = &recipe.steps[f] else {
                return RecipeOutcome::NoValue { at_step: f };
            };
            let Some(src) = values[*from].clone() else {
                return RecipeOutcome::NoValue { at_step: f };
            };
            let url = match check_url(snap, &src) {
                Ok(u) => u,
                Err(m) => return RecipeOutcome::Rejected(m),
            };
            let host = url.host_str().unwrap_or("").to_string();
            let fetched =
                tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), fetcher.get(url.as_str(), deadline))
                    .await;
            let body = match fetched {
                Ok(Ok(b)) => b,
                Ok(Err(_)) => {
                    debug!(host = %host, "recipe fetch failed");
                    return RecipeOutcome::NoValue { at_step: f };
                }
                Err(_) => return RecipeOutcome::Rejected("deadline exceeded".to_string()),
            };
            let Some(permit) = eval_permit(deadline).await else {
                return RecipeOutcome::Rejected("busy".to_string());
            };
            let (ptr, re) = (ptr.clone(), re.clone());
            let job = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                extract_fetched(body, ptr.as_deref(), re.as_deref())
            });
            let value = match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), job).await {
                Ok(Ok(v)) => v,
                Ok(Err(_)) => return RecipeOutcome::Rejected("evaluation failed".to_string()),
                Err(_) => return RecipeOutcome::Rejected("deadline exceeded".to_string()),
            };
            match value {
                Some(v) => values[f] = Some(v),
                None => return RecipeOutcome::NoValue { at_step: f },
            }
            match eval_blocking(recipe, snap, &mut values, f + 1..n, deadline).await {
                Ok(f) => f,
                Err(s) => return stop_outcome(s),
            }
        }
        (flow, _) => flow,
    };
    match flow {
        Flow::Media { url, referer } => RecipeOutcome::Media { url, referer },
        Flow::Continue => RecipeOutcome::NoValue { at_step: n.saturating_sub(1) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn deep_dom(depth: usize) -> Html {
        let mut h = String::from("<html><body>");
        for _ in 0..depth {
            h.push_str(&format!("<div>{}", "t".repeat(100)));
        }
        Html::parse_document(&h)
    }

    #[test]
    fn element_scan_stops_at_an_expired_deadline() {
        let doc = deep_dom(5000);
        let sel = Selector::parse("div").unwrap();
        let re = build_regex("([a-z]+[0-9])").unwrap();
        let t = Instant::now();
        let out = re_in_elements(&doc, &sel, &re, usize::MAX, Instant::now() - Duration::from_millis(1));
        assert_eq!(out, None);
        assert!(t.elapsed() < Duration::from_millis(300), "took {:?}", t.elapsed());
    }

    #[test]
    fn element_scan_stops_at_the_text_budget() {
        let doc = deep_dom(5000);
        let sel = Selector::parse("div").unwrap();
        let re = build_regex("([a-z]+[0-9])").unwrap();
        let t = Instant::now();
        let out = re_in_elements(&doc, &sel, &re, 64 * 1024, Instant::now() + Duration::from_secs(60));
        assert_eq!(out, None);
        assert!(t.elapsed() < Duration::from_millis(300), "took {:?}", t.elapsed());
    }

    #[test]
    fn eval_range_returns_rejected_when_the_deadline_has_passed() {
        let r = Recipe::from_json(
            r#"{"dsl_version":1,"steps":[{"op":"re","css":"div","re":"(q[0-9])"},{"op":"media","from":0,"referer":"page"}]}"#,
        )
        .unwrap();
        let mut snap = PageSnapshot::new("https://news.example.gr/a");
        snap.html = Some("<div>x</div>".to_string());
        let mut values = vec![None; 2];
        let past = Instant::now() - Duration::from_millis(1);
        assert!(matches!(eval_range(&r, &snap, &mut values, 0..2, past), Err(Stop::Rejected(_))));
    }

    #[test]
    fn fetched_body_is_truncated_before_it_is_read() {
        let mut body = String::from("x".repeat(MAX_BODY));
        body.push_str("src=https://cdn.example.gr/late.mp4");
        assert_eq!(extract_fetched(body, None, Some("src=([^ ]+)")), None);
        assert_eq!(
            extract_fetched("src=https://cdn.example.gr/a.mp4".to_string(), None, Some("src=([^ ]+)")),
            Some("https://cdn.example.gr/a.mp4".to_string())
        );
        // a multibyte character on the cut is not split
        let body = "é".repeat(MAX_BODY);
        assert_eq!(extract_fetched(body, Some("/a"), None), None);
    }

    #[test]
    fn allowed_worst_case_selectors_stay_fast_on_a_deep_dom() {
        let doc = deep_dom(3000);
        for css in [
            "b * * * * * *".to_string(),
            "p > p > p > p > p > p > p".to_string(),
            vec!["b *"; 4].join(", "),
            "div:not(.x) * * * * * *".to_string(),
        ] {
            assert!(selector_allowed(&css), "{css}");
            let sel = Selector::parse(&css).unwrap();
            let t = Instant::now();
            let n = doc.select(&sel).count();
            assert!(t.elapsed() < Duration::from_secs(2), "{css}: {n} matches in {:?}", t.elapsed());
        }
    }

    #[test]
    fn nesting_and_counts_in_selectors_are_capped() {
        let mut l = "b *".to_string();
        for _ in 0..2 {
            l = format!(":not(:not({l})) *");
        }
        assert!(!selector_allowed(&l));
        assert!(!selector_allowed("div:not(:not(.a))"));
        assert!(!selector_allowed("div:not(a b)"));
        assert!(!selector_allowed("div:not(a > b)"));
        assert!(!selector_allowed("div:not(a, b)"));
        assert!(selector_allowed("div:not(.a):not(.b)"));
        assert!(!selector_allowed("a b c d e f g h"));
        assert!(selector_allowed("a b c d e f g"));
        assert!(!selector_allowed("a > b > c > d > e > f > g > h"));
        assert!(selector_allowed("a > b > c > d > e > f > g"));
        assert!(selector_allowed(&vec!["a"; 4].join(",")));
        assert!(!selector_allowed(&vec!["a"; 5].join(",")));
    }

    fn page_with(html: String) -> PageSnapshot {
        let mut s = PageSnapshot::new("https://news.example.gr/a");
        s.html = Some(html);
        s
    }

    #[test]
    fn a_page_deeper_than_the_limit_is_rejected_before_selectors_run() {
        let r = Recipe::from_json(
            r#"{"dsl_version":1,"steps":[{"op":"attr","css":"a","attr":"href"},{"op":"media","from":0,"referer":"page"}]}"#,
        )
        .unwrap();
        let deep = page_with(format!("{}<a href=\"https://cdn.example.gr/a.mp4\">x</a>", "<div>".repeat(400)));
        let mut values = vec![None; 2];
        let far = Instant::now() + Duration::from_secs(30);
        match eval_range(&r, &deep, &mut values, 0..2, far) {
            Err(Stop::Rejected(m)) => assert_eq!(m, "page too complex"),
            _ => panic!("expected page too complex"),
        }
        let ok = page_with(format!("{}<a href=\"https://cdn.example.gr/a.mp4\">x</a>", "<div>".repeat(100)));
        assert!(matches!(eval_range(&r, &ok, &mut values, 0..2, far), Ok(Flow::Media { .. })));
    }

    #[tokio::test]
    async fn a_second_evaluation_is_busy_while_the_gate_is_held() {
        let r = Recipe::from_json(
            r#"{"dsl_version":1,"steps":[{"op":"ldjson","field":"a"},{"op":"media","from":0,"referer":"page"}]}"#,
        )
        .unwrap();
        let snap = PageSnapshot::new("https://news.example.gr/a");
        struct NoFetch;
        #[async_trait]
        impl Fetcher for NoFetch {
            async fn get(&self, _: &str, _: Instant) -> anyhow::Result<String> {
                anyhow::bail!("no")
            }
        }
        let held = eval_gate().acquire_owned().await.unwrap();
        let out = run(&r, &snap, &NoFetch, Instant::now() + Duration::from_millis(200)).await;
        assert_eq!(out, RecipeOutcome::Rejected("busy".to_string()));
        drop(held);
        let out = run(&r, &snap, &NoFetch, Instant::now() + Duration::from_secs(5)).await;
        assert_eq!(out, RecipeOutcome::NoValue { at_step: 0 });
    }

    #[test]
    fn prescan_bounds_the_page_without_parsing() {
        assert!(!prescan(&"<div>".repeat(400_000)));
        assert!(!prescan(&"<p>".repeat(300).replace("<p>", "<span>")));
        assert!(prescan("<ul><li>a<li>b<li>c</ul><br><img src=x><div/>"));
        assert!(prescan("<script>var s = '<div><div><div>';</script><!-- <div> --><p>x</p>"));
        assert!(!prescan(&format!("<a href=\"{}\">", "h".repeat(MAX_ATTR_BYTES + 1))));
        assert!(prescan(&format!("<a href=\"{}\">", "h".repeat(MAX_ATTR_BYTES))));
        assert!(!prescan(&format!("<i class='{}'>", "a ".repeat(MAX_CLASS_TOKENS + 1))));
        assert!(prescan(&format!("<i class='{}'>", "a ".repeat(MAX_CLASS_TOKENS))));
    }

    // ---- scope hashes and the shipped recipes (P10.10) ----

    struct NeverFetch;
    #[async_trait]
    impl Fetcher for NeverFetch {
        async fn get(&self, _: &str, _: Instant) -> anyhow::Result<String> {
            anyhow::bail!("shipped recipes make no request")
        }
    }

    async fn outcome(name: &str, snap: &PageSnapshot) -> RecipeOutcome {
        let (_, r) = shipped().into_iter().find(|(n, _)| *n == name).expect("shipped recipe");
        run(&r, snap, &NeverFetch, Instant::now() + Duration::from_secs(30)).await
    }

    fn media(url: &str) -> RecipeOutcome {
        RecipeOutcome::Media { url: url.to_string(), referer: Some("https://news.example.gr/a".to_string()) }
    }

    fn page_html(html: &str) -> PageSnapshot {
        let mut s = PageSnapshot::new("https://news.example.gr/a");
        s.html = Some(html.to_string());
        s
    }

    #[test]
    fn scope_hash_covers_scope_and_steps_in_a_fixed_form() {
        let r = Recipe::from_json(
            r#"{"dsl_version":1,"steps":[{"op":"ldjson","field":"contentUrl"},{"op":"media","from":0,"referer":"page"}]}"#,
        )
        .unwrap();
        let text = format!(r#"{{"scope_kind":"site","scope_key":"a.example","recipe":{}}}"#, r.canonical_json());
        let want: String = Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(scope_hash("site", "a.example", &r), want);
        assert_eq!(scope_hash("site", "a.example", &r), scope_hash("site", "a.example", &r));
        assert_ne!(scope_hash("site", "b.example", &r), want, "the key is part of the hash");
        assert_ne!(scope_hash("player", "a.example", &r), want, "the kind is part of the hash");
        assert_eq!(steps_hash(&r), r.hash());
        assert_ne!(want, r.hash(), "a scoped hash is not the steps hash");
        let mut other = r.clone();
        other.steps[0] = Step::Ldjson { field: "embedUrl".to_string() };
        assert_ne!(scope_hash("site", "a.example", &other), want);
        // a key with characters JSON must escape cannot forge another field
        assert_ne!(scope_hash("site", "a\",\"x", &r), scope_hash("site", "a", &r));
    }

    #[test]
    fn shipped_recipes_are_valid_few_and_uniquely_named() {
        let list = shipped();
        assert!(!list.is_empty() && list.len() <= 16, "{} shipped recipes", list.len());
        let mut names: Vec<&str> = list.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), list.len(), "names must be unique");
        let snake = Regex::new("^[a-z][a-z0-9]*(_[a-z0-9]+)*$").unwrap();
        for (name, r) in &list {
            assert!(snake.is_match(name), "{name} is not snake_case");
            r.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(&Recipe::from_json(&r.canonical_json()).unwrap(), r, "{name} survives its canonical form");
            assert!(r.canonical_json().len() <= 4096, "{name} is too big to store");
            assert!(r.steps.iter().all(|s| !s.is_live()), "{name}: shipped recipes need no live page");
        }
        let store = shipped_for_store();
        assert_eq!(store.len(), list.len());
        let mut hashes: Vec<&str> = store.iter().map(|s| s.hash.as_str()).collect();
        hashes.sort_unstable();
        hashes.dedup();
        assert_eq!(hashes.len(), store.len(), "store hashes are unique");
        for (s, (name, r)) in store.iter().zip(&list) {
            assert_eq!(s.name, *name);
            assert_eq!(s.hash, scope_hash("generic", name, r));
            assert_eq!(s.steps_hash, r.hash());
            assert_eq!(s.recipe_json, r.canonical_json());
            assert_eq!(s.dsl_version, DSL_VERSION);
        }
    }

    #[tokio::test]
    async fn shipped_recipes_find_their_source() {
        let mut ld = PageSnapshot::new("https://news.example.gr/a");
        ld.ld_json.push(r#"{"@type":"VideoObject","contentUrl":"https://cdn.example.gr/v/a.mp4"}"#.to_string());
        assert_eq!(outcome("ldjson_content_url", &ld).await, media("https://cdn.example.gr/v/a.mp4"));

        let og = |prop: &str, v: &str| {
            page_html(&format!(r#"<html><head><meta property="{prop}" content="{v}"></head><body></body></html>"#))
        };
        assert_eq!(
            outcome("og_video", &og("og:video", "https://cdn.example.gr/v/b.mp4")).await,
            media("https://cdn.example.gr/v/b.mp4")
        );
        assert_eq!(outcome("og_video", &og("og:video", "/v/b.mp4")).await, media("https://news.example.gr/v/b.mp4"));
        assert_eq!(
            outcome("og_video_url", &og("og:video:url", "https://cdn.example.gr/v/b2.mp4")).await,
            media("https://cdn.example.gr/v/b2.mp4")
        );
        assert_eq!(
            outcome("og_video_secure_url", &og("og:video:secure_url", "https://cdn.example.gr/v/b3.m3u8")).await,
            media("https://cdn.example.gr/v/b3.m3u8")
        );

        let next = page_html(
            r#"<html><body><script id="__NEXT_DATA__" type="application/json">{"props":{"poster":"https:\/\/cdn.example.gr\/p.jpg","video":{"src":"https:\/\/cdn.example.gr\/v\/c.m3u8?t=1"}}}</script></body></html>"#,
        );
        assert_eq!(outcome("next_data_media", &next).await, media("https://cdn.example.gr/v/c.m3u8?t=1"));

        let nuxt = page_html(
            r#"<html><body><script>window.__NUXT__={state:{video:{url:"https://cdn.example.gr/v/d.mp4"}}}</script></body></html>"#,
        );
        assert_eq!(outcome("nuxt_media", &nuxt).await, media("https://cdn.example.gr/v/d.mp4"));

        let jw = page_html(
            r#"<html><body><script>jwplayer("p").setup({title:"x",sources:[{file:"https://cdn.example.gr/v/e.m3u8"}]});</script></body></html>"#,
        );
        assert_eq!(outcome("jwplayer_sources", &jw).await, media("https://cdn.example.gr/v/e.m3u8"));
        let mut jw_json = PageSnapshot::new("https://news.example.gr/a");
        jw_json.scripts.push(r#"{"playlist":[{"sources":[{"file":"https:\/\/cdn.example.gr\/v\/e2.mp4"}]}]}"#.to_string());
        assert_eq!(outcome("jwplayer_sources", &jw_json).await, media("https://cdn.example.gr/v/e2.mp4"));

        let vjs = page_html(r#"<html><body><video controls><source src="/v/f.mp4" type="video/mp4"></video></body></html>"#);
        assert_eq!(outcome("videojs_source", &vjs).await, media("https://news.example.gr/v/f.mp4"));

        let ds = page_html(r#"<html><body><video data-src="https://cdn.example.gr/v/g.mp4"></video></body></html>"#);
        assert_eq!(outcome("data_src", &ds).await, media("https://cdn.example.gr/v/g.mp4"));
        let ds2 = page_html(r#"<html><body><video><source data-src="https://cdn.example.gr/v/g2.mp4"></video></body></html>"#);
        assert_eq!(outcome("data_src", &ds2).await, media("https://cdn.example.gr/v/g2.mp4"));
        let hls = page_html(r#"<html><body><div class="player" data-hls="https://cdn.example.gr/v/h.m3u8"></div></body></html>"#);
        assert_eq!(outcome("data_hls", &hls).await, media("https://cdn.example.gr/v/h.m3u8"));
        let vu = page_html(r#"<html><body><div data-video-url="https://cdn.example.gr/v/i.mp4"></div></body></html>"#);
        assert_eq!(outcome("data_video_url", &vu).await, media("https://cdn.example.gr/v/i.mp4"));
    }

    #[tokio::test]
    async fn shipped_recipes_give_nothing_on_a_page_without_their_source() {
        // a page with no video, and decoys that look close: an image where a
        // video address would be, a data-src on an <img>, a jwplayer with no file
        let mut blank = page_html(
            r#"<html><head><meta property="og:image" content="https://cdn.example.gr/p.jpg"><meta property="og:title" content="t"></head>
               <body><p>text</p><img data-src="https://cdn.example.gr/i.mp4"><div class="x" data-id="3"></div>
               <script id="__NEXT_DATA__" type="application/json">{"poster":"https:\/\/cdn.example.gr\/p.jpg"}</script>
               <script>window.__NUXT__={img:"https://cdn.example.gr/a.jpg"};jwplayer("p").setup({sources:[{label:"x"}]});</script></body></html>"#,
        );
        blank.ld_json.push(r#"{"@type":"NewsArticle","headline":"h"}"#.to_string());
        blank.scripts.push("var sources = 1; var file = 'a.jpg';".to_string());
        let nothing = PageSnapshot::new("https://news.example.gr/a");
        for (name, _) in shipped() {
            for snap in [&blank, &nothing] {
                match outcome(name, snap).await {
                    RecipeOutcome::NoValue { .. } => {}
                    other => panic!("{name} found {other:?} on a page without its source"),
                }
            }
        }
    }
}
