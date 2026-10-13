//! Recipe language v1 (P10.8): validation, canonical hash, interpreter, host rule.
//! All snapshots are invented.

use async_trait::async_trait;
use omni_browser::recipe::{action_key, live_actions, run, run_after_live, Fetcher, LiveAction, Recipe, RecipeError, RecipeOutcome};
use omni_browser::{NetworkEntry, PageSnapshot};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct Fake {
    body: Option<String>,
    calls: AtomicUsize,
}

impl Fake {
    fn none() -> Self {
        Fake { body: None, calls: AtomicUsize::new(0) }
    }
    fn body(b: &str) -> Self {
        Fake { body: Some(b.to_string()), calls: AtomicUsize::new(0) }
    }
}

#[async_trait]
impl Fetcher for Fake {
    async fn get(&self, _url: &str, _deadline: Instant) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.body.clone().ok_or_else(|| anyhow::anyhow!("no body"))
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

fn snap(html: &str) -> PageSnapshot {
    let mut s = PageSnapshot::new("https://news.example.gr/story/1");
    s.html = Some(html.to_string());
    s
}

fn recipe(json: &str) -> Recipe {
    Recipe::from_json(json).unwrap_or_else(|e| panic!("recipe should be valid: {e}"))
}

async fn go(r: &Recipe, s: &PageSnapshot) -> RecipeOutcome {
    run(r, s, &Fake::none(), deadline()).await
}

fn media(url: &str) -> RecipeOutcome {
    RecipeOutcome::Media { url: url.to_string(), referer: Some("https://news.example.gr/story/1".to_string()) }
}

const MEDIA1: &str = r#"{"op":"media","from":0,"referer":"page"}"#;

fn one(step: &str) -> Recipe {
    recipe(&format!(r#"{{"dsl_version":1,"steps":[{step},{MEDIA1}]}}"#))
}

// ---- steps ----

#[tokio::test]
async fn json_step_reads_next_data_pointer() {
    let r = one(r##"{"op":"json","css":"script#__NEXT_DATA__","ptr":"/props/pageProps/video/hls"}"##);
    let page = snap(
        r#"<html><body><script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"video":{"hls":"https://cdn.example.gr/a/index.m3u8"}}}}</script></body></html>"#,
    );
    assert_eq!(go(&r, &page).await, media("https://cdn.example.gr/a/index.m3u8"));
    let none = snap(r#"<script id="__NEXT_DATA__">{"props":{}}</script>"#);
    assert_eq!(go(&r, &none).await, RecipeOutcome::NoValue { at_step: 0 });
}

#[tokio::test]
async fn attr_and_abs_resolve_a_relative_source() {
    let r = recipe(
        r#"{"dsl_version":1,"steps":[{"op":"attr","css":"video source","attr":"src"},{"op":"abs","from":0},{"op":"media","from":1,"referer":"none"}]}"#,
    );
    let page = snap(r#"<video><source src="/media/clip.mp4"></video>"#);
    assert_eq!(
        go(&r, &page).await,
        RecipeOutcome::Media { url: "https://news.example.gr/media/clip.mp4".to_string(), referer: None }
    );
    let none = snap("<video></video>");
    assert_eq!(go(&r, &none).await, RecipeOutcome::NoValue { at_step: 0 });
}

#[tokio::test]
async fn ldjson_reads_content_url_also_inside_graph() {
    let r = one(r#"{"op":"ldjson","field":"contentUrl"}"#);
    let mut page = snap("<p></p>");
    page.ld_json = vec![
        r#"{"@type":"WebSite"}"#.to_string(),
        r#"{"@graph":[{"@type":"Thing"},{"@type":"VideoObject","contentUrl":"https://cdn.example.gr/v.mp4"}]}"#.to_string(),
    ];
    assert_eq!(go(&r, &page).await, media("https://cdn.example.gr/v.mp4"));
    page.ld_json = vec![r#"{"@type":"WebSite"}"#.to_string(), "not json".to_string()];
    assert_eq!(go(&r, &page).await, RecipeOutcome::NoValue { at_step: 0 });
}

#[tokio::test]
async fn net_step_takes_first_good_matching_entry() {
    let r = one(r#"{"op":"net","url_re":"\\.m3u8","mime":"application/"}"#);
    let mut page = snap("<p></p>");
    let entry = |url: &str, status: Option<i64>, mime: &str| NetworkEntry {
        url: url.to_string(),
        status,
        mime: Some(mime.to_string()),
        ..Default::default()
    };
    page.push_network(entry("https://cdn.example.gr/bad.m3u8", Some(404), "application/vnd.apple.mpegurl"));
    page.push_network(entry("https://cdn.example.gr/wrongmime.m3u8", Some(200), "text/html"));
    page.push_network(entry("https://cdn.example.gr/ok.m3u8", Some(206), "application/vnd.apple.mpegurl"));
    assert_eq!(go(&r, &page).await, media("https://cdn.example.gr/ok.m3u8"));
    let empty = snap("<p></p>");
    assert_eq!(go(&r, &empty).await, RecipeOutcome::NoValue { at_step: 0 });
}

#[tokio::test]
async fn re_step_over_a_player_setup_script() {
    let r = one(r#"{"op":"re","re":"file:\\s*\"([^\"]+\\.m3u8)\""}"#);
    let mut page = snap("<p></p>");
    page.scripts = vec!["var x = 1;".to_string(), r#"jwplayer("p").setup({file: "https://cdn.example.gr/live/a.m3u8"});"#.to_string()];
    assert_eq!(go(&r, &page).await, media("https://cdn.example.gr/live/a.m3u8"));
    page.scripts = vec!["var x = 1;".to_string()];
    assert_eq!(go(&r, &page).await, RecipeOutcome::NoValue { at_step: 0 });

    let scoped = one(r#"{"op":"re","css":"script.cfg","re":"src=(\\S+)"}"#);
    let page = snap(r#"<script>src=https://x.example.gr/no</script><script class="cfg">src=https://cdn.example.gr/yes.mp4</script>"#);
    assert_eq!(go(&scoped, &page).await, media("https://cdn.example.gr/yes.mp4"));
}

#[tokio::test]
async fn template_builds_from_a_data_attribute() {
    let r = recipe(
        r#"{"dsl_version":1,"steps":[{"op":"attr","css":"div[data-video-id]","attr":"data-video-id"},{"op":"template","t":"https://cdn.example.gr/v/{0}/master.m3u8"},{"op":"media","from":1,"referer":"page"}]}"#,
    );
    let page = snap(r#"<div data-video-id="abc123"></div>"#);
    assert_eq!(go(&r, &page).await, media("https://cdn.example.gr/v/abc123/master.m3u8"));
    let none = snap("<div></div>");
    assert_eq!(go(&r, &none).await, RecipeOutcome::NoValue { at_step: 0 });
}

#[tokio::test]
async fn unescape_undoes_json_escapes_and_entities() {
    let r = recipe(
        r#"{"dsl_version":1,"steps":[{"op":"re","re":"u=\"([^\"]+)\""},{"op":"unescape","from":0},{"op":"media","from":1,"referer":"page"}]}"#,
    );
    let mut page = snap("<p></p>");
    page.scripts = vec![r#"u="https:\/\/cdn.example.gr\/a.mp4?k=1\u0026x=2&amp;y=3""#.to_string()];
    assert_eq!(go(&r, &page).await, media("https://cdn.example.gr/a.mp4?k=1&x=2&y=3"));
    page.scripts = vec![];
    assert_eq!(go(&r, &page).await, RecipeOutcome::NoValue { at_step: 0 });
}

#[tokio::test]
async fn fetch_reads_a_json_body_or_a_capture() {
    let by_ptr = recipe(
        r#"{"dsl_version":1,"steps":[{"op":"attr","css":"div#p","attr":"data-api"},{"op":"fetch","from":0,"ptr":"/data/url"},{"op":"media","from":1,"referer":"page"}]}"#,
    );
    let page = snap(r#"<div id="p" data-api="https://api.example.gr/v/1"></div>"#);
    let f = Fake::body(r#"{"data":{"url":"https://cdn.example.gr/z.m3u8"}}"#);
    assert_eq!(run(&by_ptr, &page, &f, deadline()).await, media("https://cdn.example.gr/z.m3u8"));
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);

    let by_re = recipe(
        r#"{"dsl_version":1,"steps":[{"op":"attr","css":"div#p","attr":"data-api"},{"op":"fetch","from":0,"re":"src=(\\S+)"},{"op":"media","from":1,"referer":"page"}]}"#,
    );
    let f = Fake::body("src=https://cdn.example.gr/q.mp4 end");
    assert_eq!(run(&by_re, &page, &f, deadline()).await, media("https://cdn.example.gr/q.mp4"));

    // no value: body lacks the pointer, and a failing fetcher
    let f = Fake::body(r#"{"data":{}}"#);
    assert_eq!(run(&by_ptr, &page, &f, deadline()).await, RecipeOutcome::NoValue { at_step: 1 });
    assert_eq!(run(&by_ptr, &page, &Fake::none(), deadline()).await, RecipeOutcome::NoValue { at_step: 1 });
}

// ---- validation ----

fn err(json: &str) -> RecipeError {
    let r: Recipe = serde_json::from_str(json).expect("shape parses");
    r.validate().expect_err("should be invalid")
}

fn with(step: &str) -> String {
    format!(r#"{{"dsl_version":1,"steps":[{step},{MEDIA1}]}}"#)
}

#[test]
fn validation_rejects_each_rule() {
    use RecipeError::*;
    assert!(matches!(err(&format!(r#"{{"dsl_version":2,"steps":[{{"op":"ldjson","field":"a"}},{MEDIA1}]}}"#)), Version(2)));
    assert!(matches!(err(r#"{"dsl_version":1,"steps":[]}"#), StepCount(0)));
    let nine = format!("[{}{MEDIA1}]", r#"{"op":"ldjson","field":"a"},"#.repeat(8));
    assert!(matches!(err(&format!(r#"{{"dsl_version":1,"steps":{nine}}}"#)), StepCount(9)));
    assert!(matches!(err(r#"{"dsl_version":1,"steps":[{"op":"ldjson","field":"a"}]}"#), MediaPlacement));
    assert!(matches!(
        err(&format!(r#"{{"dsl_version":1,"steps":[{MEDIA1},{{"op":"ldjson","field":"a"}}]}}"#)),
        MediaPlacement
    ));
    assert!(matches!(
        err(&format!(r#"{{"dsl_version":1,"steps":[{{"op":"ldjson","field":"a"}},{MEDIA1},{MEDIA1}]}}"#)),
        MediaPlacement
    ));
    let two_fetch = r#"{"dsl_version":1,"steps":[{"op":"ldjson","field":"a"},{"op":"fetch","from":0,"ptr":""},{"op":"fetch","from":1,"ptr":""},{"op":"media","from":2,"referer":"page"}]}"#;
    assert!(matches!(err(two_fetch), TooManyFetch));
    let three_live = r#"{"dsl_version":1,"steps":[{"op":"wait","ms":1},{"op":"wait","ms":1},{"op":"wait","ms":1},{"op":"ldjson","field":"a"},{"op":"media","from":3,"referer":"page"}]}"#;
    assert!(matches!(err(three_live), TooManyLiveActions));
    let live_late = r#"{"dsl_version":1,"steps":[{"op":"ldjson","field":"a"},{"op":"click","css":"a"},{"op":"media","from":0,"referer":"page"}]}"#;
    assert!(matches!(err(live_late), LiveAfterSource));
    let long = "a".repeat(201);
    assert!(matches!(err(&with(&format!(r#"{{"op":"ldjson","field":"{long}"}}"#))), TooLong { field: "field", .. }));
    assert!(matches!(err(&with(&format!(r#"{{"op":"attr","css":"{long}","attr":"x"}}"#))), TooLong { field: "css", .. }));
    assert!(matches!(err(&with(&format!(r#"{{"op":"attr","css":"a","attr":"{long}"}}"#))), TooLong { field: "attr", .. }));
    assert!(matches!(err(&with(&format!(r#"{{"op":"json","css":"a","ptr":"/{long}"}}"#))), TooLong { field: "ptr", .. }));
    assert!(matches!(err(&with(&format!(r#"{{"op":"net","url_re":"{long}"}}"#))), TooLong { field: "url_re", .. }));
    let long_t = format!("https://a.example.gr/{}{{0}}", "a".repeat(300));
    let t = format!(
        r#"{{"dsl_version":1,"steps":[{{"op":"ldjson","field":"a"}},{{"op":"template","t":"{long_t}"}},{{"op":"media","from":1,"referer":"page"}}]}}"#
    );
    assert!(matches!(err(&t), TooLong { field: "template", .. }));
    let wait = r#"{"dsl_version":1,"steps":[{"op":"wait","ms":8001},{"op":"ldjson","field":"a"},{"op":"media","from":1,"referer":"page"}]}"#;
    assert!(matches!(err(wait), WaitTooLong { step: 0 }));
    assert!(matches!(err(&with(r#"{"op":"net","url_re":"("}"#)), BadRegex { .. }));
    // over the compiled-size limit
    assert!(matches!(err(&with(r#"{"op":"net","url_re":"(a{1000}){1000}"}"#)), BadRegex { .. }));
    assert!(matches!(err(&with(r#"{"op":"re","re":"abc"}"#)), CaptureGroups { .. }));
    assert!(matches!(err(&with(r#"{"op":"re","re":"(a)(b)"}"#)), CaptureGroups { .. }));
    assert!(matches!(err(&with(r#"{"op":"attr","css":"a,,b","attr":"x"}"#)), BadCss { .. }));
    assert!(matches!(err(&with(r#"{"op":"json","css":"script","ptr":"a/b"}"#)), BadPointer { .. }));
    assert!(matches!(err(&with(r#"{"op":"json","css":"script","ptr":"/a~2"}"#)), BadPointer { .. }));
    // from: itself, later, or a non-value step
    let from_self = r#"{"dsl_version":1,"steps":[{"op":"abs","from":0},{"op":"media","from":0,"referer":"page"}]}"#;
    assert!(matches!(err(from_self), BadFrom { step: 0 }));
    let from_live = r#"{"dsl_version":1,"steps":[{"op":"wait","ms":1},{"op":"abs","from":0},{"op":"media","from":1,"referer":"page"}]}"#;
    assert!(matches!(err(from_live), BadFrom { step: 1 }));
    let media_from_later = r#"{"dsl_version":1,"steps":[{"op":"ldjson","field":"a"},{"op":"media","from":5,"referer":"page"}]}"#;
    assert!(matches!(err(media_from_later), BadFrom { step: 1 }));
    // templates
    let tpl = |t: &str| {
        format!(
            r#"{{"dsl_version":1,"steps":[{{"op":"ldjson","field":"a"}},{{"op":"template","t":"{t}"}},{{"op":"media","from":1,"referer":"page"}}]}}"#
        )
    };
    assert!(matches!(err(&tpl("https://a.example.gr/{1}")), BadTemplate { step: 1 }));
    assert!(matches!(err(&tpl("https://a.example.gr/{x}")), BadTemplate { .. }));
    assert!(matches!(err(&tpl("https://a.example.gr/{0")), BadTemplate { .. }));
    assert!(matches!(err(&tpl("https://a.example.gr/}")), BadTemplate { .. }));
    // fetch needs exactly one of ptr/re
    let f = |extra: &str| {
        format!(
            r#"{{"dsl_version":1,"steps":[{{"op":"ldjson","field":"a"}},{{"op":"fetch","from":0{extra}}},{{"op":"media","from":1,"referer":"page"}}]}}"#
        )
    };
    assert!(matches!(err(&f("")), FetchArgs { .. }));
    assert!(matches!(err(&f(r#","ptr":"","re":"(a)""#)), FetchArgs { .. }));
}

#[test]
fn unknown_fields_and_ops_do_not_parse() {
    // top-level: a valid two-step recipe plus one extra key, parsed without validation
    let extra = r#"{"dsl_version":1,"steps":[{"op":"ldjson","field":"a"},{"op":"media","from":0,"referer":"page"}],"extra":1}"#;
    assert!(serde_json::from_str::<Recipe>(extra).is_err());
    let without = r#"{"dsl_version":1,"steps":[{"op":"ldjson","field":"a"},{"op":"media","from":0,"referer":"page"}]}"#;
    assert!(serde_json::from_str::<Recipe>(without).is_ok());
    assert!(Recipe::from_json(r#"{"dsl_version":1,"steps":[{"op":"eval","js":"1"}]}"#).is_err());
    assert!(Recipe::from_json(&with(r#"{"op":"ldjson","field":"a","also":1}"#)).is_err());
}

// ---- canonical hash ----

#[test]
fn hash_ignores_key_order_and_whitespace_but_not_content() {
    let a = recipe(r#"{"dsl_version":1,"steps":[{"op":"ldjson","field":"contentUrl"},{"op":"media","from":0,"referer":"page"}]}"#);
    let b = recipe(
        r#"{
          "steps": [ {"field":"contentUrl", "op":"ldjson"},
                     {"referer":"page","from":0,"op":"media"} ],
          "dsl_version": 1
        }"#,
    );
    assert_eq!(a.hash(), b.hash());
    assert_eq!(a.hash().len(), 64);
    assert!(a.hash().chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    let c = recipe(r#"{"dsl_version":1,"steps":[{"op":"ldjson","field":"contentURL"},{"op":"media","from":0,"referer":"page"}]}"#);
    assert_ne!(a.hash(), c.hash());
}

// ---- host rule ----

#[tokio::test]
async fn host_rule_rejects_unseen_private_and_non_http() {
    let tpl = |t: &str| {
        recipe(&format!(
            r#"{{"dsl_version":1,"steps":[{{"op":"attr","css":"div","attr":"data-id"}},{{"op":"template","t":"{t}"}},{{"op":"media","from":1,"referer":"page"}}]}}"#
        ))
    };
    let page = snap(r#"<div data-id="x1"></div>"#);
    // unobserved host from a template
    let out = go(&tpl("https://evil.example.org/{0}.m3u8"), &page).await;
    match out {
        RecipeOutcome::Rejected(m) => {
            assert!(m.contains("evil.example.org"));
            assert!(!m.contains("x1"), "message must not carry the url");
        }
        o => panic!("expected Rejected, got {o:?}"),
    }
    // same registrable domain is fine
    assert_eq!(
        go(&tpl("https://cdn.news.example.gr/{0}.m3u8"), &page).await,
        media("https://cdn.news.example.gr/x1.m3u8")
    );
    for bad in ["http://10.0.0.5/{0}.mp4", "http://127.0.0.1/{0}.mp4", "http://169.254.1.1/{0}", "http://100.64.0.1/{0}", "http://[::1]/{0}", "http://[fd00::1]/{0}", "file:///etc/{0}"] {
        assert!(matches!(go(&tpl(bad), &page).await, RecipeOutcome::Rejected(_)), "{bad}");
    }
}

#[tokio::test]
async fn private_literal_is_rejected_even_when_the_page_saw_it() {
    let r = one(r#"{"op":"net","url_re":"\\.mp4"}"#);
    let mut page = snap("<p></p>");
    page.push_network(NetworkEntry { url: "http://10.0.0.5/a.mp4".to_string(), ..Default::default() });
    assert!(matches!(go(&r, &page).await, RecipeOutcome::Rejected(_)));
    let mut page = snap("<p></p>");
    page.push_network(NetworkEntry { url: "http://127.0.0.1:8080/a.mp4".to_string(), ..Default::default() });
    assert!(matches!(go(&r, &page).await, RecipeOutcome::Rejected(_)));
}

#[tokio::test]
async fn media_to_file_scheme_is_rejected() {
    let r = one(r#"{"op":"attr","css":"a","attr":"href"}"#);
    let page = snap(r#"<a href="file:///C:/secret.mp4">x</a>"#);
    assert!(matches!(go(&r, &page).await, RecipeOutcome::Rejected(_)));
}

#[tokio::test]
async fn rejected_fetch_makes_no_call() {
    let r = recipe(
        r#"{"dsl_version":1,"steps":[{"op":"attr","css":"a","attr":"href"},{"op":"fetch","from":0,"ptr":"/u"},{"op":"media","from":1,"referer":"page"}]}"#,
    );
    let f = Fake::body(r#"{"u":"https://cdn.example.gr/a.mp4"}"#);
    for href in ["http://192.168.1.10/api", "http://127.0.0.1/api", "https://unseen.example.org/api", "file:///x"] {
        let page = snap(&format!(r#"<a href="{href}">x</a>"#));
        assert!(matches!(run(&r, &page, &f, deadline()).await, RecipeOutcome::Rejected(_)), "{href}");
    }
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn live_actions_return_needs_live_page_without_running() {
    let r = recipe(
        r#"{"dsl_version":1,"steps":[{"op":"click","css":"button.play"},{"op":"wait","ms":500},{"op":"attr","css":"a","attr":"href"},{"op":"fetch","from":2,"ptr":"/u"},{"op":"media","from":3,"referer":"page"}]}"#,
    );
    let page = snap(r#"<a href="https://cdn.example.gr/a">x</a>"#);
    let f = Fake::body(r#"{"u":"https://cdn.example.gr/a.mp4"}"#);
    assert_eq!(run(&r, &page, &f, deadline()).await, RecipeOutcome::NeedsLivePage);
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_expired_deadline_stops_the_run() {
    let r = one(r#"{"op":"ldjson","field":"contentUrl"}"#);
    let mut page = snap("<p></p>");
    page.ld_json = vec![r#"{"contentUrl":"https://cdn.example.gr/v.mp4"}"#.to_string()];
    let past = Instant::now() - Duration::from_millis(1);
    assert!(matches!(run(&r, &page, &Fake::none(), past).await, RecipeOutcome::Rejected(_)));
}

// ---- robustness ----

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() as usize) % xs.len()]
    }
}

fn synthetic_snapshot() -> PageSnapshot {
    let mut html = String::from("<html><body>");
    let mut i = 0;
    while html.len() < 400 * 1024 {
        html.push_str(&format!("<div class=\"c{}\"><p>paragraph {i} text</p></div>", i % 7));
        i += 1;
    }
    html.push_str(r#"<video><source src="/m/v.mp4"></video><div data-video-id="v9"></div><script id="__NEXT_DATA__">{"props":{"u":"https://cdn.example.gr/n.m3u8"}}</script>"#);
    html.push_str("</body></html>");
    let mut s = snap(&html);
    s.scripts = vec![format!("{}file: \"https://cdn.example.gr/s.m3u8\"", "x".repeat(100 * 1024))];
    s.ld_json = vec![r#"{"contentUrl":"https://cdn.example.gr/l.mp4"}"#.to_string()];
    s.push_network(NetworkEntry { url: "https://cdn.example.gr/net.m3u8".to_string(), ..Default::default() });
    s
}

#[tokio::test]
async fn ten_thousand_generated_recipes_never_panic_and_run_fast() {
    let page = synthetic_snapshot();
    let mut rng = Lcg(0x5eed);
    let sources = [
        r#"{"op":"net","url_re":"m3u8"}"#,
        r#"{"op":"net","url_re":"(","mime":"video"}"#,
        r#"{"op":"attr","css":"video source","attr":"src"}"#,
        r#"{"op":"attr","css":"div[data-video-id]","attr":"data-video-id"}"#,
        r#"{"op":"attr","css":"div[","attr":"src"}"#,
        r#"{"op":"json","css":"script#__NEXT_DATA__","ptr":"/props/u"}"#,
        r#"{"op":"json","css":"script#__NEXT_DATA__","ptr":"props"}"#,
        r#"{"op":"ldjson","field":"contentUrl"}"#,
        r#"{"op":"re","re":"file: \"([^\"]+)\""}"#,
        r#"{"op":"re","css":"p","re":"(\\d+)"}"#,
        r#"{"op":"re","re":"nogroup"}"#,
        r#"{"op":"attr","css":"div:not(.c1) > p","attr":"class"}"#,
        r#"{"op":"attr","css":"div:nth-child(2n+1) p:first-child","attr":"class"}"#,
        r#"{"op":"attr","css":"video source, a[href*='a+b']","attr":"src"}"#,
        r#"{"op":"attr","css":"b * * * * * * *","attr":"src"}"#,
        r#"{"op":"attr","css":"div:not(:not(.c1))","attr":"src"}"#,
        r#"{"op":"re","css":"div > div p","re":"(paragraph [0-9]+)"}"#,
        r#"{"op":"click","css":"button"}"#,
        r#"{"op":"wait","ms":99999}"#,
    ];
    let transforms = [
        r#"{"op":"abs","from":0}"#,
        r#"{"op":"abs","from":7}"#,
        r#"{"op":"unescape","from":0}"#,
        r#"{"op":"template","t":"https://cdn.example.gr/{0}"}"#,
        r#"{"op":"template","t":"{9}"}"#,
        r#"{"op":"fetch","from":0,"ptr":"/a"}"#,
        r#"{"op":"fetch","from":0}"#,
    ];
    let medias = [MEDIA1, r#"{"op":"media","from":1,"referer":"none"}"#, r#"{"op":"media","from":3,"referer":"page"}"#];
    let fetcher = Fake::body(r#"{"a":"https://cdn.example.gr/f.mp4"}"#);
    let mut valid = 0;
    let mut max = Duration::ZERO;
    let mut slow = 0;
    for _ in 0..10_000 {
        let n = 1 + (rng.next() % 3) as usize;
        let mut steps: Vec<String> = Vec::new();
        for k in 0..n {
            let pool: &[&str] = if k == 0 { &sources } else { &transforms };
            steps.push(rng.pick(pool).to_string());
        }
        steps.push(rng.pick(&medias).to_string());
        let json = format!(r#"{{"dsl_version":1,"steps":[{}]}}"#, steps.join(","));
        let Ok(r) = serde_json::from_str::<Recipe>(&json) else { continue };
        if r.validate().is_err() {
            continue;
        }
        valid += 1;
        let t = Instant::now();
        let _ = run(&r, &page, &fetcher, Instant::now() + Duration::from_secs(5)).await;
        let took = t.elapsed();
        max = max.max(took);
        if took >= Duration::from_millis(500) {
            slow += 1;
        }
    }
    assert!(valid > 500, "generator should produce many valid recipes, got {valid}");
    // This catches a pathological blow-up (seconds), not machine load: a single
    // scheduler stall under a loaded gate once made one run take 657 ms, so a
    // few slow runs are allowed and only a run over 5 s, or many slow ones, fail.
    assert!(max < Duration::from_secs(5), "slowest run took {max:?}");
    assert!(slow <= 5, "{slow} runs took 500 ms or more (slowest {max:?})");
}

// ---- review fixes ----

#[tokio::test]
async fn media_output_is_the_serialized_url_not_the_raw_text() {
    let r = one(r#"{"op":"attr","css":"a","attr":"href"}"#);
    let page = snap(r#"<a href="http://cdn.example.gr\@10.0.0.1/a.mp4">x</a>"#);
    assert_eq!(go(&r, &page).await, media("http://cdn.example.gr/@10.0.0.1/a.mp4"));
    // the same through a template
    let t = recipe(
        r#"{"dsl_version":1,"steps":[{"op":"attr","css":"a","attr":"href"},{"op":"template","t":"{0}"},{"op":"media","from":1,"referer":"page"}]}"#,
    );
    assert_eq!(go(&t, &page).await, media("http://cdn.example.gr/@10.0.0.1/a.mp4"));
}

#[tokio::test]
async fn observed_but_non_public_hosts_are_rejected() {
    let r = one(r#"{"op":"attr","css":"a","attr":"href"}"#);
    for bad in [
        "http://2130706433/",
        "http://0x7f.1/",
        "http://cdn.example.gr@10.0.0.1/",
        "http://[::ffff:10.0.0.5]/",
        "http://[fe80::1]/",
        "http://100.64.0.1/",
        "http://[::127.0.0.1]/",
        "http://localhost/",
        "http://localhost./",
        "http://LOCALHOST/",
        "http://x.localhost./",
        "http://tv.localhost/",
        "http://[64:ff9b::a00:1]/",
        "http://[2002:7f00:1::1]/",
    ] {
        let mut page = snap(&format!(r#"<a href="{bad}">x</a>"#));
        page.push_network(NetworkEntry { url: bad.to_string(), ..Default::default() });
        for h in ["10.0.0.1", "127.0.0.1", "localhost", "localhost.", "x.localhost.", "tv.localhost"] {
            page.push_network(NetworkEntry { url: format!("http://{h}/x"), ..Default::default() });
        }
        assert!(matches!(go(&r, &page).await, RecipeOutcome::Rejected(_)), "{bad}");
    }
}

#[tokio::test]
async fn explicit_port_needs_to_have_been_seen() {
    let r = one(r#"{"op":"attr","css":"a","attr":"href"}"#);
    let mut page = snap(r#"<a href="https://cdn.example.gr:8443/a.mp4">x</a>"#);
    assert!(matches!(go(&r, &page).await, RecipeOutcome::Rejected(_)));
    page.push_network(NetworkEntry { url: "https://cdn.example.gr:9000/other".to_string(), ..Default::default() });
    assert!(matches!(go(&r, &page).await, RecipeOutcome::Rejected(_)));
    page.push_network(NetworkEntry { url: "https://cdn.example.gr:8443/seg.ts".to_string(), ..Default::default() });
    assert_eq!(go(&r, &page).await, media("https://cdn.example.gr:8443/a.mp4"));
    // a default port written out is the same as none
    let page = snap(r#"<a href="https://cdn.example.gr:443/a.mp4">x</a>"#);
    assert_eq!(go(&r, &page).await, media("https://cdn.example.gr/a.mp4"));
}

#[test]
fn validation_rejects_costly_selectors() {
    let bad = [
        "div:has(p)",
        "div:HAS(p)",
        "a ~ b",
        "a + b",
        "a~b",
        "a+b",
        "div:is(:has(p))",
        "d\\69v",
        "div:/**/has(p)",
        "a/*\"*/~b/*\"*/",
        "div:is(x\"\n,b ~ c,y\")",
        "div:is(a)",
        "div:where(a)",
        "p::before",
        "a:hover",
        "div:not(a + b)",
        "div:not(:has(p))",
        "a[href=\"x\\\"]",
        "a[b='c\"d']",
        "a[b=\"c'd\"]",
        "div\u{0}",
        "caf\u{e9}",
        "a[b~c]",
        "a:nth-child(2 of b + c)",
        "a:nth-child(2",
        "a)",
        "a:",
        "li:nth-of-type(2)",
        "li:nth-last-child(2)",
        "p:first-of-type",
        "p:last-of-type",
        "div[x*=\"q\" i] p",
        "div[x*=\"q\" s]",
        "div[x*=q i]",
    ];
    for css in bad {
        let step = format!(r#"{{"op":"attr","css":{},"attr":"x"}}"#, serde_json::to_string(css).unwrap());
        assert!(matches!(err(&with(&step)), RecipeError::SelectorNotAllowed { .. }), "{css:?}");
    }
    let good = [
        "a[rel~=\"x\"]",
        "div > p",
        "script#__NEXT_DATA__",
        "div:nth-child(2)",
        "a[href*='a+b']",
        "div:nth-child(2n+1)",
        "li:nth-child(n+2)",
        "meta[property=\"og:video\"]",
        "video > source[src]",
        "p:first-child",
        "div:not(.ad)",
        "div:not(:first-child)",
        "a[href^='https://x.example/a~b']",
        "h1, h2",
    ];
    for css in good {
        let step = format!(r#"{{"op":"attr","css":{},"attr":"x"}}"#, serde_json::to_string(css).unwrap());
        assert!(Recipe::from_json(&with(&step)).is_ok(), "{css:?}");
    }
}

#[tokio::test]
async fn deeply_nested_dom_with_css_regex_returns_by_the_deadline() {
    let r = one(r#"{"op":"re","css":"div","re":"(zzz[0-9]+)"}"#);
    let mut html = String::from("<html><body>");
    for _ in 0..3000 {
        html.push_str(&format!("<div>{}", "t".repeat(200)));
    }
    let mut page = snap(&html);
    page.html = Some(html);
    let budget = Duration::from_secs(2);
    let t = Instant::now();
    let out = run(&r, &page, &Fake::none(), t + budget).await;
    assert!(t.elapsed() < budget + Duration::from_millis(500), "took {:?}", t.elapsed());
    assert!(matches!(out, RecipeOutcome::NoValue { .. } | RecipeOutcome::Rejected(_)), "{out:?}");
}

// ---- hostile and realistic pages ----

async fn rejected_as_complex(css_step: &str, html: String) -> Duration {
    let r = one(css_step);
    let page = snap(&html);
    let t = Instant::now();
    let out = go(&r, &page).await;
    let took = t.elapsed();
    assert_eq!(out, RecipeOutcome::Rejected("page too complex".to_string()));
    took
}

#[tokio::test]
async fn hostile_pages_are_rejected_before_they_can_cost_much() {
    // a 1 MiB attribute under many descendants
    let attr = "q".repeat(1 << 20);
    let html = format!(r#"<div x="{attr}"><p>a</p><p>b</p></div>"#);
    rejected_as_complex(r#"{"op":"attr","css":"div[x*=q] p","attr":"id"}"#, html).await;

    // 200 000 class tokens
    let classes = "zz ".repeat(200_000);
    let html = format!(r#"<div class="{classes}"><p>a</p></div>"#);
    rejected_as_complex(r#"{"op":"attr","css":".zz p","attr":"id"}"#, html).await;

    // 400 000 nested divs, refused by the pre-scan in well under a second
    let took = rejected_as_complex(r#"{"op":"attr","css":"div","attr":"id"}"#, "<div>".repeat(400_000)).await;
    assert!(took < Duration::from_secs(1), "took {took:?}");

    // too many elements
    let html = "<p>x</p>".repeat(30_001);
    rejected_as_complex(r#"{"op":"attr","css":"p","attr":"id"}"#, html).await;

    // a class list of 65 tokens
    let html = format!(r#"<div class="{}"></div>"#, "c ".repeat(65));
    rejected_as_complex(r#"{"op":"attr","css":"div","attr":"id"}"#, html).await;

    // attribute bytes along the ancestor path: each page limit is respected on its own
    // (250 levels, 8000 bytes each) but the sum over the path is not
    let mut html = String::new();
    for _ in 0..250 {
        html.push_str(&format!(r#"<div x="{}">"#, "v".repeat(8000)));
    }
    rejected_as_complex(r#"{"op":"attr","css":"div","attr":"id"}"#, html).await;
}

#[tokio::test]
async fn stray_end_tags_do_not_hide_a_deep_page() {
    let html = format!("{}{}", "<div>".repeat(300), "</p>".repeat(300));
    rejected_as_complex(r#"{"op":"attr","css":"div","attr":"id"}"#, html).await;
}

#[tokio::test]
async fn a_realistic_news_page_still_runs() {
    let mut html = String::from("<!DOCTYPE html><html><head><title>Story</title><meta charset=\"utf-8\"></head><body>");
    for d in 0..40 {
        html.push_str(&format!(r#"<div class="wrap w{d}" data-depth="{d}">"#));
    }
    for i in 0..1500 {
        html.push_str(&format!(
            r#"<article class="card c{} k"><img src="/i/{i}.jpg" srcset="/i/{i}-1.jpg 320w, /i/{i}-2.jpg 640w, /i/{i}-3.jpg 960w"><p>Paragraph {i}<br>text</p></article>"#,
            i % 9
        ));
    }
    let big = "x".repeat(200 * 1024);
    html.push_str(&format!(
        r#"<script id="__NEXT_DATA__" type="application/json">{{"pad":"{big}","props":{{"video":{{"hls":"https://cdn.example.gr/news/index.m3u8"}}}}}}</script>"#
    ));
    for _ in 0..40 {
        html.push_str("</div>");
    }
    html.push_str("</body></html>");
    let r = one(r##"{"op":"json","css":"script#__NEXT_DATA__","ptr":"/props/video/hls"}"##);
    let page = snap(&html);
    assert_eq!(go(&r, &page).await, media("https://cdn.example.gr/news/index.m3u8"));
    // unclosed optional end tags do not count as nesting
    let li = format!("<ul>{}</ul><video><source src=\"https://cdn.example.gr/v.mp4\"></video>", "<li>item".repeat(2000));
    let r = one(r#"{"op":"attr","css":"video source","attr":"src"}"#);
    assert_eq!(go(&r, &snap(&li)).await, media("https://cdn.example.gr/v.mp4"));
}

#[tokio::test]
async fn a_large_data_attribute_on_a_leaf_is_read_in_full() {
    let url = format!("https://cdn.example.gr/a.mp4?cfg={}", "p".repeat(40 * 1024));
    let html = format!(
        r#"<html><body><div class="wrap"><section><div data-player="{url}"></div></section></div></body></html>"#
    );
    let r = one(r#"{"op":"attr","css":"div[data-player]","attr":"data-player"}"#);
    assert_eq!(go(&r, &snap(&html)).await, media(&url));
}

#[test]
fn live_actions_come_out_in_step_order_and_only_the_live_ones() {
    let r = recipe(
        r##"{"dsl_version":1,"steps":[{"op":"click","css":"button.play"},{"op":"wait","ms":500},{"op":"net","url_re":"[.]m3u8"},{"op":"media","from":2,"referer":"page"}]}"##,
    );
    assert_eq!(live_actions(&r), vec![LiveAction::Click("button.play".into()), LiveAction::Wait(500)]);
    let r = recipe(
        r##"{"dsl_version":1,"steps":[{"op":"scroll_to","css":"#lazy"},{"op":"click","css":"#go"},{"op":"net","url_re":"[.]m3u8"},{"op":"media","from":2,"referer":"page"}]}"##,
    );
    assert_eq!(live_actions(&r), vec![LiveAction::ScrollTo("#lazy".into()), LiveAction::Click("#go".into())]);
    let passive = one(r#"{"op":"ldjson","field":"contentUrl"}"#);
    assert!(live_actions(&passive).is_empty());
}

#[test]
fn action_key_is_equal_exactly_for_equal_actions() {
    use LiveAction::*;
    let a = [Click("#go".into()), Wait(500)];
    assert_eq!(action_key(&a), action_key(&[Click("#go".into()), Wait(500)]));
    assert_ne!(action_key(&a), action_key(&[Click("#go".into()), Wait(501)]));
    assert_ne!(action_key(&a), action_key(&[Click("#gx".into()), Wait(500)]));
    assert_ne!(action_key(&a), action_key(&[ScrollTo("#go".into()), Wait(500)]));
    assert_ne!(action_key(&a), action_key(&[Wait(500), Click("#go".into())]));
    assert_ne!(action_key(&[Click("a".into())]), action_key(&[]));
    // a selector cannot imitate the separator between two actions
    assert_ne!(action_key(&[Click("a\",[\"wait\",5]".into())]), action_key(&[Click("a".into()), Wait(5)]));
}

#[tokio::test]
async fn run_after_live_reads_the_net_value_of_a_recipe_with_a_click() {
    let r = recipe(
        r##"{"dsl_version":1,"steps":[{"op":"click","css":"#load"},{"op":"net","url_re":"master[.]m3u8"},{"op":"media","from":1,"referer":"page"}]}"##,
    );
    let mut page = snap("<p></p>");
    let f = Fake::none();
    // Before the click the page shows no stream: the recipe has no value.
    assert_eq!(run(&r, &page, &f, deadline()).await, RecipeOutcome::NeedsLivePage);
    assert_eq!(run_after_live(&r, &page, &f, deadline()).await, RecipeOutcome::NoValue { at_step: 1 });
    page.network.push(NetworkEntry {
        url: "https://news.example.gr/v/master.m3u8".to_string(),
        method: Some("GET".into()),
        status: Some(200),
        mime: None,
        resource_type: None,
        frame_id: None,
    });
    assert_eq!(
        run_after_live(&r, &page, &f, deadline()).await,
        RecipeOutcome::Media {
            url: "https://news.example.gr/v/master.m3u8".to_string(),
            referer: Some("https://news.example.gr/story/1".to_string())
        }
    );
}

#[tokio::test]
async fn run_after_live_on_a_passive_recipe_equals_run() {
    let r = one(r#"{"op":"ldjson","field":"contentUrl"}"#);
    let mut page = snap("<p></p>");
    let f = Fake::none();
    assert_eq!(run(&r, &page, &f, deadline()).await, run_after_live(&r, &page, &f, deadline()).await);
    page.ld_json = vec![r#"{"contentUrl":"https://cdn.example.gr/v.mp4"}"#.to_string()];
    let a = run(&r, &page, &f, deadline()).await;
    assert!(matches!(a, RecipeOutcome::Media { .. }));
    assert_eq!(a, run_after_live(&r, &page, &f, deadline()).await);
}
