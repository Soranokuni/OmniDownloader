//! The verification gate every proposed video address passes BEFORE anything
//! is downloaded (plan P10.9).
//!
//! A repair rung (a recipe, the LLM) only proposes addresses. The gate decides,
//! deterministically, which of them are worth a download: a public address the
//! page itself used, not an ad, a real non-live video of plausible length that
//! matches the length the page states. Nothing here changes a job; the caller
//! (P10.11) acts on the result.
//!
//! The browser-side facts come in as plain data ([`GateFacts`]) because this
//! crate does not depend on `omni-browser`. Check texts name hosts only, never
//! a full address or a query string.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use omni_core::net::{is_non_public, parse_public_url, resolve_public, EgressError};
use omni_core::process::{run, RunOpts};
use tokio::time::Instant as TokioInstant;
use tracing::debug;

use crate::downloader::Downloader;

/// Most candidates probed at once.
const PROBE_PARALLEL: usize = 3;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const MIN_HEIGHT: u32 = 240;
/// Stand-in place of a candidate with none (a page has at most 20 players):
/// after every real player, in both
/// the ordering and the score.
const NO_POSITION: usize = 50;
const MIN_SECS: f64 = 5.0;
/// Below this a video is suspected to be an ad.
const SHORT_SECS: f64 = 20.0;
/// Details of the checks that say "not looked at" or "ran out of time" rather
/// than "looked and it failed"; see [`Rejected::is_conclusive`].
const NOT_CHECKED: &str = "Δεν ελέγχθηκε: υπάρχει ήδη αποδεκτό βίντεο";
const PROBE_TIMEOUT_DETAIL: &str = "Έληξε ο χρόνος ελέγχου";
const RESOLVE_TIMEOUT_DETAIL: &str = "Λήξη χρόνου στην ανάλυση";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub url: String,
    pub referer: Option<String>,
    /// `recipe:<hash8>` or `llm:<hash8>`.
    pub origin: String,
    /// Place among the page's players, document order.
    pub position: Option<usize>,
    pub in_article: Option<bool>,
}

/// What the page showed, as plain data.
#[derive(Debug, Clone, Default)]
pub struct GateFacts {
    pub observed_hosts: BTreeSet<String>,
    /// Registrable domain of the page.
    pub page_domain: Option<String>,
    /// Media files ad servers named.
    pub ad_media: BTreeSet<String>,
    pub expected_secs: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    /// Greek, short, hosts only.
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeKind {
    /// A media file or manifest read by ffprobe.
    Direct,
    /// A page or platform link read by yt-dlp.
    Platform,
}

#[derive(Debug, Clone)]
pub struct Verified {
    pub candidate: Candidate,
    pub duration_secs: f64,
    pub height: u32,
    pub kind: ProbeKind,
    pub checks: Vec<Check>,
    pub score: i64,
}

#[derive(Debug, Clone)]
pub struct Rejected {
    pub candidate: Candidate,
    pub checks: Vec<Check>,
}

impl Rejected {
    /// The candidate was really looked at and failed a check. `false` when it
    /// was not checked at all (a better video was already accepted) or the
    /// check ran out of time: that says nothing about the candidate.
    pub fn is_conclusive(&self) -> bool {
        match self.checks.last() {
            Some(c) if !c.ok => {
                c.detail != NOT_CHECKED
                    && !c.detail.starts_with(PROBE_TIMEOUT_DETAIL)
                    && !c.detail.starts_with(RESOLVE_TIMEOUT_DETAIL)
            }
            _ => false,
        }
    }
}

/// What a probe found.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeInfo {
    pub kind: ProbeKind,
    pub video_streams: u32,
    pub height: u32,
    pub duration_secs: Option<f64>,
    pub live: bool,
}

#[async_trait]
pub trait Prober: Send + Sync {
    /// `Err` is a short reason (no address in it).
    async fn probe(&self, cand: &Candidate) -> Result<ProbeInfo, String>;
}

#[async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, EgressError>;
}

/// Name resolution through the system, refusing a non-public answer.
pub struct SystemResolver;

#[async_trait]
impl Resolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, EgressError> {
        Ok(resolve_public(host, port, RESOLVE_TIMEOUT).await?.into_iter().map(|a| a.ip()).collect())
    }
}

fn check(name: &'static str, ok: bool, detail: impl Into<String>) -> Check {
    Check { name, ok, detail: detail.into() }
}

fn host_of(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| "—".to_string())
}

// ---------------------------------------------------------------- ad keys

fn decode_entities(s: &str) -> String {
    let mut out = s.to_string();
    for (from, to) in [
        ("&#38;", "&"),
        ("&#x26;", "&"),
        ("&quot;", "\""),
        ("&#34;", "\""),
        ("&#39;", "'"),
        ("&#x27;", "'"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&amp;", "&"),
    ] {
        out = out.replace(from, to);
    }
    out
}

/// Host and path of an address, lowercase host, query and fragment ignored,
/// entities decoded: the key an ad file is compared by.
pub fn ad_key(url: &str) -> String {
    let decoded = decode_entities(url.trim());
    match url::Url::parse(&decoded) {
        Ok(u) => format!("{}{}", u.host_str().unwrap_or("").trim_end_matches('.').to_ascii_lowercase(), u.path()),
        Err(_) => decoded.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase(),
    }
}

// ---------------------------------------------------------------- ffprobe

/// A recognisable media file or manifest address (read by ffprobe); anything
/// else is a page or platform link (read by yt-dlp).
fn is_direct_media(url: &url::Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    ["mp4", "m4v", "mov", "webm", "mkv", "ts", "m3u8", "mpd", "flv", "avi", "mpg", "mpeg"]
        .iter()
        .any(|e| path.ends_with(&format!(".{e}")))
}

/// ffprobe `-show_format -show_streams` JSON to what the gate needs.
pub fn parse_ffprobe(json: &str) -> Result<ProbeInfo, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|_| "ffprobe gave no JSON".to_string())?;
    let streams = v.get("streams").and_then(|s| s.as_array()).cloned().unwrap_or_default();
    let video: Vec<&serde_json::Value> = streams
        .iter()
        .filter(|s| s.get("codec_type").and_then(|c| c.as_str()) == Some("video"))
        .filter(|s| s.pointer("/disposition/attached_pic").and_then(|a| a.as_i64()) != Some(1))
        .collect();
    let height = video.iter().filter_map(|s| s.get("height").and_then(|h| h.as_u64())).max().unwrap_or(0) as u32;
    let num = |x: Option<&serde_json::Value>| {
        x.and_then(|d| d.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| d.as_f64())).filter(|d| d.is_finite() && *d > 0.0)
    };
    let duration = num(v.pointer("/format/duration")).or_else(|| video.iter().find_map(|s| num(s.get("duration"))));
    Ok(ProbeInfo { kind: ProbeKind::Direct, video_streams: video.len() as u32, height, duration_secs: duration, live: false })
}

/// The real prober: ffprobe for a media address, yt-dlp for a page or platform
/// link. Both run through `omni_core::process::run`.
///
/// Known, accepted limits: the tools resolve the host again themselves (DNS
/// rebinding after the gate's own check), and an HLS/DASH manifest may name
/// segment addresses on private hosts that ffprobe then reads. The protocol
/// whitelist only stops local files and unusual schemes.
pub struct ToolProber {
    ffprobe: PathBuf,
    downloader: Option<Downloader>,
}

impl ToolProber {
    pub fn new(ffprobe: PathBuf, ytdl: Option<PathBuf>) -> Self {
        Self { ffprobe, downloader: ytdl.map(Downloader::new) }
    }

    async fn ffprobe(&self, cand: &Candidate, url: &url::Url) -> Result<ProbeInfo, String> {
        let mut args: Vec<String> = ["-v", "error", "-print_format", "json", "-show_format", "-show_streams"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Only the network protocols a web video needs: never a local file.
        args.extend(["-protocol_whitelist", "http,https,tcp,tls,crypto", "-user_agent", USER_AGENT].map(String::from));
        if let Some(r) = cand.referer.as_deref().filter(|r| !r.contains(['\r', '\n'])) {
            args.push("-headers".into());
            args.push(format!("Referer: {r}\r\n"));
        }
        args.push(url.as_str().to_string());
        let out = run(&self.ffprobe, &args, RunOpts::new(PROBE_TIMEOUT).capture_stdout())
            .await
            .map_err(|e| format!("could not run ffprobe: {e:#}"))?;
        if out.timed_out {
            return Err("ffprobe did not answer in time".into());
        }
        if !out.success {
            return Err("ffprobe could not read the address".into());
        }
        parse_ffprobe(&out.stdout)
    }
}

#[async_trait]
impl Prober for ToolProber {
    async fn probe(&self, cand: &Candidate) -> Result<ProbeInfo, String> {
        let url = url::Url::parse(&cand.url).map_err(|_| "bad address".to_string())?;
        if is_direct_media(&url) {
            return self.ffprobe(cand, &url).await;
        }
        let dl = self.downloader.as_ref().ok_or_else(|| "yt-dlp is not available".to_string())?;
        let p = dl.probe_media(url.as_str(), PROBE_TIMEOUT).await?;
        Ok(ProbeInfo {
            kind: ProbeKind::Platform,
            video_streams: 1,
            height: p.height.unwrap_or(0),
            duration_secs: p.duration_secs,
            live: p.live,
        })
    }
}

// ---------------------------------------------------------------- the gate

struct Survivor {
    candidate: Candidate,
    info: ProbeInfo,
    duration: f64,
    matched_expected: bool,
    checks: Vec<Check>,
}

fn expected_tolerance(expected: f64) -> f64 {
    (expected * 0.05).max(3.0)
}

/// One candidate through egress, ad and probe checks, then the page's stated
/// length. `Err` carries every check run so far, the last one failed.
async fn check_one(
    cand: &Candidate,
    facts: &GateFacts,
    is_ad: &(dyn Fn(&str) -> bool + Sync),
    prober: &dyn Prober,
    resolver: &dyn Resolver,
    deadline: TokioInstant,
    allow_loopback: bool,
) -> Result<Survivor, Vec<Check>> {
    let mut checks = Vec::new();

    // 1. Egress.
    let (url, host) = match parse_public_url(&cand.url) {
        Ok(p) => p,
        Err(why) => {
            checks.push(check("egress", false, format!("Μη επιτρεπτή διεύθυνση: {why}")));
            return Err(checks);
        }
    };
    let key = url.host_str().unwrap_or("").trim_end_matches('.').to_ascii_lowercase();
    // An explicit port only when the page itself used that host and port
    // (`observed_hosts` carries "host:port" for those, e.g. a Wowza :1935).
    if let Some(port) = url.port() {
        if port != 80 && port != 443 && !facts.observed_hosts.contains(&format!("{key}:{port}")) {
            checks.push(check("egress", false, format!("Ασυνήθιστη θύρα στο {host}")));
            return Err(checks);
        }
    }
    let own_domain = facts.page_domain.is_some()
        && omni_core::urlnorm::registrable_domain(&format!("{}://{}/", url.scheme(), key)) == facts.page_domain;
    if !facts.observed_hosts.contains(&key) && !own_domain {
        checks.push(check("egress", false, format!("Ο κόμβος {host} δεν χρησιμοποιήθηκε από τη σελίδα")));
        return Err(checks);
    }
    if host.parse::<IpAddr>().is_err() {
        let port = url.port_or_known_default().unwrap_or(443);
        match tokio::time::timeout_at(deadline, resolver.resolve(&host, port)).await {
            Err(_) => {
                checks.push(check("egress", false, format!("{RESOLVE_TIMEOUT_DETAIL} του {host}")));
                return Err(checks);
            }
            Ok(Err(_)) => {
                checks.push(check("egress", false, format!("Το {host} δεν αναλύθηκε ή δείχνει σε μη δημόσια διεύθυνση")));
                return Err(checks);
            }
            Ok(Ok(addrs)) => {
                let bad = addrs.is_empty() || addrs.iter().any(|a| is_non_public(*a) && !(allow_loopback && a.is_loopback()));
                if bad {
                    checks.push(check("egress", false, format!("Το {host} δείχνει σε μη δημόσια διεύθυνση")));
                    return Err(checks);
                }
            }
        }
    }
    checks.push(check("egress", true, format!("Δημόσιος κόμβος {host} της σελίδας")));

    // 2. Not an ad.
    let is_listed = facts.ad_media.iter().any(|a| ad_key(a) == ad_key(&cand.url));
    if is_listed || is_ad(&cand.url) {
        checks.push(check("ad", false, format!("Διαφήμιση ({host})")));
        return Err(checks);
    }
    checks.push(check("ad", true, "Δεν είναι διαφήμιση"));

    // 3. Probe.
    let info = match tokio::time::timeout_at(deadline, prober.probe(cand)).await {
        Err(_) => {
            checks.push(check("probe", false, format!("{PROBE_TIMEOUT_DETAIL} ({host})")));
            return Err(checks);
        }
        Ok(Err(why)) => {
            debug!("repair gate: probe of {host} failed");
            let _ = why;
            checks.push(check("probe", false, format!("Ο έλεγχος του {host} απέτυχε")));
            return Err(checks);
        }
        Ok(Ok(i)) => i,
    };
    if info.live {
        checks.push(check("probe", false, format!("Ζωντανή μετάδοση ({host})")));
        return Err(checks);
    }
    let Some(duration) = info.duration_secs.filter(|d| d.is_finite() && *d > 0.0) else {
        checks.push(check("probe", false, format!("Ζωντανή μετάδοση ή άγνωστη διάρκεια ({host})")));
        return Err(checks);
    };
    if info.video_streams < 1 {
        checks.push(check("probe", false, format!("Δεν υπάρχει εικόνα ({host})")));
        return Err(checks);
    }
    if info.height < MIN_HEIGHT {
        checks.push(check("probe", false, format!("Πολύ μικρή ανάλυση: {}p ({host})", info.height)));
        return Err(checks);
    }
    if duration < MIN_SECS {
        checks.push(check("probe", false, format!("Πολύ μικρή διάρκεια: {duration:.0} δλ ({host})")));
        return Err(checks);
    }
    checks.push(check("probe", true, format!("{duration:.0} δλ, {}p ({host})", info.height)));

    // 4a. The page's stated length.
    let mut matched_expected = false;
    if let Some(expected) = facts.expected_secs {
        if (duration - expected).abs() > expected_tolerance(expected) {
            checks.push(check("length", false, format!("Η διάρκεια {duration:.0} δλ δεν ταιριάζει με τα {expected:.0} δλ της σελίδας")));
            return Err(checks);
        }
        matched_expected = true;
        checks.push(check("length", true, format!("Ταιριάζει με τα {expected:.0} δλ της σελίδας")));
    }
    Ok(Survivor { candidate: cand.clone(), info, duration, matched_expected, checks })
}

/// Ranking before probing; referer and origin last, so the same inputs in any
/// order keep the same copy of a repeated address.
fn order_key(c: &Candidate) -> (bool, usize, String, Option<String>, String) {
    (
        c.in_article != Some(true),
        c.position.unwrap_or(NO_POSITION).min(NO_POSITION),
        c.url.clone(),
        c.referer.clone(),
        c.origin.clone(),
    )
}

fn score(c: &Candidate, matched_expected: bool) -> i64 {
    let mut s = 0i64;
    if matched_expected {
        s += 1000;
    }
    if c.in_article == Some(true) {
        s += 100;
    }
    s - c.position.unwrap_or(NO_POSITION).min(NO_POSITION) as i64
}

/// Check every candidate; `(verified best first, rejected)`. The call ends by
/// `deadline`; whatever was not finished is rejected with a timeout check.
pub async fn verify(
    cands: Vec<Candidate>,
    facts: &GateFacts,
    is_ad: &(dyn Fn(&str) -> bool + Sync),
    prober: &dyn Prober,
    resolver: &dyn Resolver,
    deadline: std::time::Instant,
) -> (Vec<Verified>, Vec<Rejected>) {
    verify_inner(cands, facts, is_ad, prober, resolver, deadline, false).await
}

/// `verify` for tests that serve media from 127.0.0.1: a loopback answer from
/// the resolver is accepted. Not reachable from configuration; production code
/// calls [`verify`].
#[doc(hidden)]
pub async fn verify_allowing_loopback_for_tests(
    cands: Vec<Candidate>,
    facts: &GateFacts,
    is_ad: &(dyn Fn(&str) -> bool + Sync),
    prober: &dyn Prober,
    resolver: &dyn Resolver,
    deadline: std::time::Instant,
) -> (Vec<Verified>, Vec<Rejected>) {
    verify_inner(cands, facts, is_ad, prober, resolver, deadline, true).await
}

async fn verify_inner(
    mut cands: Vec<Candidate>,
    facts: &GateFacts,
    is_ad: &(dyn Fn(&str) -> bool + Sync),
    prober: &dyn Prober,
    resolver: &dyn Resolver,
    deadline: std::time::Instant,
    allow_loopback: bool,
) -> (Vec<Verified>, Vec<Rejected>) {
    let deadline = TokioInstant::from_std(deadline);
    // Everything below works on the serialized address: the parser and other
    // URL readers can disagree about the host of the raw text (`a.example\.0.0.1`).
    for c in cands.iter_mut() {
        if let Ok((u, _)) = parse_public_url(&c.url) {
            c.url = u.to_string();
        }
    }
    cands.sort_by_key(order_key);
    let mut seen = std::collections::HashSet::new();
    cands.retain(|c| seen.insert(c.url.clone()));

    let mut survivors: Vec<Survivor> = Vec::new();
    let mut rejected: Vec<Rejected> = Vec::new();
    let mut next = 0;
    while next < cands.len() && TokioInstant::now() < deadline {
        let end = (next + PROBE_PARALLEL).min(cands.len());
        let batch = &cands[next..end];
        let one = |i: usize| {
            let c = batch.get(i);
            async move {
                match c {
                    Some(c) => Some(check_one(c, facts, is_ad, prober, resolver, deadline, allow_loopback).await),
                    None => None,
                }
            }
        };
        let (a, b, c) = tokio::join!(one(0), one(1), one(2));
        for (cand, res) in batch.iter().zip([a, b, c]) {
            match res.expect("one result per batch entry") {
                Ok(s) => survivors.push(s),
                Err(checks) => rejected.push(Rejected { candidate: cand.clone(), checks }),
            }
        }
        next = end;
        // Enough: a survivor long enough to be the story.
        if survivors.iter().any(|s| s.duration >= SHORT_SECS) {
            break;
        }
    }
    let timed_out = TokioInstant::now() >= deadline;
    for c in &cands[next..] {
        let detail = if timed_out { PROBE_TIMEOUT_DETAIL } else { NOT_CHECKED };
        rejected.push(Rejected { candidate: c.clone(), checks: vec![check("probe", false, detail)] });
    }

    // 4b. Short videos.
    let has_long = survivors.iter().any(|s| s.duration >= SHORT_SECS);
    let expected_long = facts.expected_secs.is_some_and(|e| e >= SHORT_SECS);
    let mut verified: Vec<Verified> = Vec::new();
    for mut s in survivors {
        // A clip whose length is the one the page states is the page's own
        // video, however short.
        if s.duration < SHORT_SECS && !s.matched_expected {
            if has_long || expected_long {
                s.checks.push(check("short", false, format!("Πολύ σύντομο — πιθανή διαφήμιση ({})", host_of(&s.candidate.url))));
                rejected.push(Rejected { candidate: s.candidate, checks: s.checks });
                continue;
            }
            s.checks.push(check("short", false, "Σύντομο — μπορεί να είναι διαφήμιση"));
        }
        let sc = score(&s.candidate, s.matched_expected);
        verified.push(Verified {
            candidate: s.candidate,
            duration_secs: s.duration,
            height: s.info.height,
            kind: s.info.kind,
            checks: s.checks,
            score: sc,
        });
    }
    verified.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.candidate.url.cmp(&b.candidate.url)));
    (verified, rejected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(checks: Vec<Check>) -> Rejected {
        Rejected { candidate: Candidate { url: "https://a.example/x.mp4".into(), referer: None, origin: "recipe:abcd1234".into(), position: None, in_article: None }, checks }
    }

    /// Only a candidate that was looked at and failed is a verdict on it.
    #[test]
    fn a_rejection_is_conclusive_only_when_the_candidate_really_failed_a_check() {
        assert!(rejected(vec![check("egress", true, "ok"), check("probe", false, "Ο έλεγχος του a.example απέτυχε")]).is_conclusive());
        assert!(rejected(vec![check("egress", false, "Ο κόμβος a.example δεν χρησιμοποιήθηκε από τη σελίδα")]).is_conclusive());
        assert!(!rejected(vec![check("probe", false, NOT_CHECKED)]).is_conclusive(), "not looked at");
        assert!(!rejected(vec![check("probe", false, format!("{PROBE_TIMEOUT_DETAIL} (a.example)"))]).is_conclusive(), "probe timed out");
        assert!(!rejected(vec![check("egress", false, format!("{RESOLVE_TIMEOUT_DETAIL} του a.example"))]).is_conclusive(), "resolve timed out");
        assert!(!rejected(vec![]).is_conclusive());
        assert!(!rejected(vec![check("probe", true, "ok")]).is_conclusive());
    }
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    struct FakeProber {
        calls: AtomicUsize,
        by_url: HashMap<String, ProbeInfo>,
        delay: Duration,
    }

    impl FakeProber {
        fn new(items: &[(&str, f64, u32)]) -> Self {
            let by_url = items
                .iter()
                .map(|(u, d, h)| {
                    (u.to_string(), ProbeInfo { kind: ProbeKind::Direct, video_streams: 1, height: *h, duration_secs: Some(*d), live: false })
                })
                .collect();
            Self { calls: AtomicUsize::new(0), by_url, delay: Duration::ZERO }
        }
    }

    #[async_trait]
    impl Prober for FakeProber {
        async fn probe(&self, cand: &Candidate) -> Result<ProbeInfo, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.by_url.get(&cand.url).cloned().ok_or_else(|| "unknown".to_string())
        }
    }

    struct FakeResolver(HashMap<String, Vec<IpAddr>>);

    #[async_trait]
    impl Resolver for FakeResolver {
        async fn resolve(&self, host: &str, _port: u16) -> Result<Vec<IpAddr>, EgressError> {
            self.0.get(host).cloned().ok_or(EgressError::Unresolved)
        }
    }

    fn public_resolver() -> FakeResolver {
        let ip: IpAddr = "93.184.216.34".parse().unwrap();
        FakeResolver(["cdn.example.org", "other.example.net", "evil.example.org"].iter().map(|h| (h.to_string(), vec![ip])).collect())
    }

    fn cand(url: &str) -> Candidate {
        Candidate { url: url.into(), referer: None, origin: "recipe:abcd1234".into(), position: None, in_article: None }
    }

    fn facts() -> GateFacts {
        GateFacts {
            observed_hosts: ["cdn.example.org", "127.0.0.1", "10.0.0.5", "[::1]", "169.254.1.1", "evil.example.org"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            page_domain: Some("news.example".into()),
            ..Default::default()
        }
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    async fn run_gate(cands: Vec<Candidate>, f: &GateFacts, p: &FakeProber) -> (Vec<Verified>, Vec<Rejected>) {
        verify(cands, f, &|_| false, p, &public_resolver(), soon()).await
    }

    #[tokio::test]
    async fn egress_failures_never_reach_the_prober() {
        let mut resolver = public_resolver();
        resolver.0.insert("sneaky.example.org".into(), vec!["10.1.2.3".parse().unwrap()]);
        resolver.0.insert("mixed.example.org".into(), vec!["93.184.216.34".parse().unwrap(), "192.168.0.9".parse().unwrap()]);
        let mut f = facts();
        f.observed_hosts.insert("sneaky.example.org".into());
        f.observed_hosts.insert("mixed.example.org".into());
        let urls = [
            "http://127.0.0.1/a.mp4",
            "http://10.0.0.5/a.mp4",
            "http://[::1]/a.mp4",
            "http://169.254.1.1/a.mp4",
            "ftp://cdn.example.org/a.mp4",
            "http://user:pw@cdn.example.org/a.mp4",
            "https://unseen.example.com/a.mp4",
            "https://sneaky.example.org/a.mp4",
            "https://mixed.example.org/a.mp4",
            "https://cdn.example.org:8443/a.mp4",
            "http://localhost/a.mp4",
        ];
        let p = FakeProber::new(&[]);
        let (ok, rej) = verify(urls.iter().map(|u| cand(u)).collect(), &f, &|_| false, &p, &resolver, soon()).await;
        assert!(ok.is_empty(), "{ok:?}");
        assert_eq!(rej.len(), urls.len());
        assert_eq!(p.calls.load(Ordering::SeqCst), 0, "a rejected address must never be probed");
        for r in &rej {
            let last = r.checks.last().unwrap();
            assert!(!last.ok && last.name == "egress", "{:?}", r.candidate.url);
            assert!(!last.detail.contains("a.mp4") && !last.detail.contains("pw"), "details name hosts only: {}", last.detail);
        }
    }

    #[tokio::test]
    async fn the_page_own_domain_and_an_observed_host_pass_egress() {
        let mut resolver = public_resolver();
        resolver.0.insert("video.news.example".into(), vec!["93.184.216.34".parse().unwrap()]);
        let p = FakeProber::new(&[("https://video.news.example/a.mp4", 90.0, 720), ("https://cdn.example.org/b.mp4", 95.0, 720)]);
        let (ok, rej) = verify(
            vec![cand("https://video.news.example/a.mp4"), cand("https://cdn.example.org/b.mp4")],
            &facts(),
            &|_| false,
            &p,
            &resolver,
            soon(),
        )
        .await;
        assert_eq!(ok.len(), 2, "{rej:?}");
    }

    #[tokio::test]
    async fn ads_are_rejected_without_probing() {
        let mut f = facts();
        f.ad_media.insert("https://cdn.example.org/ads/spot.mp4?a=1&amp;b=2".into());
        let p = FakeProber::new(&[("https://cdn.example.org/ads/spot.mp4?x=9", 30.0, 720)]);
        let blocked = |u: &str| u.contains("/tracker/");
        let (ok, rej) = verify(
            vec![
                cand("https://cdn.example.org/ads/spot.mp4?x=9"),
                cand("https://CDN.example.org/ads/spot.mp4?a=1&b=2"),
                cand("https://cdn.example.org/tracker/clip.mp4"),
            ],
            &f,
            &blocked,
            &p,
            &public_resolver(),
            soon(),
        )
        .await;
        assert!(ok.is_empty());
        assert_eq!(rej.len(), 3);
        assert_eq!(p.calls.load(Ordering::SeqCst), 0);
        assert!(rej.iter().all(|r| r.checks.last().unwrap().name == "ad"));
    }

    #[tokio::test]
    async fn a_short_clip_loses_to_a_long_one_but_survives_alone() {
        let p = FakeProber::new(&[("https://cdn.example.org/short.mp4", 15.0, 720), ("https://cdn.example.org/long.mp4", 120.0, 720)]);
        let (ok, rej) = run_gate(vec![cand("https://cdn.example.org/short.mp4"), cand("https://cdn.example.org/long.mp4")], &facts(), &p).await;
        assert_eq!(ok.len(), 1);
        assert!(ok[0].candidate.url.ends_with("long.mp4"));
        assert_eq!(rej.len(), 1);
        assert!(rej[0].checks.last().unwrap().name == "short");

        let (ok, rej) = run_gate(vec![cand("https://cdn.example.org/short.mp4")], &facts(), &p).await;
        assert!(rej.is_empty());
        assert_eq!(ok.len(), 1);
        let warn = ok[0].checks.last().unwrap();
        assert!(!warn.ok && warn.detail.starts_with("Σύντομο"), "{warn:?}");
    }

    #[tokio::test]
    async fn a_short_clip_is_rejected_when_the_page_says_the_story_is_long() {
        let mut f = facts();
        f.expected_secs = Some(120.0);
        let p = FakeProber::new(&[("https://cdn.example.org/short.mp4", 15.0, 720)]);
        let (ok, rej) = run_gate(vec![cand("https://cdn.example.org/short.mp4")], &f, &p).await;
        assert!(ok.is_empty());
        assert_eq!(rej.len(), 1);
    }

    /// The page says 21 s and the clip is 19.5 s: that is the page's own
    /// video, not an ad, even though both are under the short-clip line.
    #[tokio::test]
    async fn a_short_clip_of_the_stated_length_is_kept() {
        let mut f = facts();
        f.expected_secs = Some(21.0);
        let p = FakeProber::new(&[("https://cdn.example.org/clip.mp4", 19.5, 720)]);
        let (ok, rej) = run_gate(vec![cand("https://cdn.example.org/clip.mp4")], &f, &p).await;
        assert!(rej.is_empty(), "{rej:?}");
        assert_eq!(ok.len(), 1);
    }

    /// A Wowza-style port the page itself used is allowed; the same host on
    /// a port the page never used is not.
    #[tokio::test]
    async fn a_port_the_page_used_is_allowed_and_no_other() {
        let mut f = facts();
        f.observed_hosts.insert("cdn.example.org:1935".into());
        let p = FakeProber::new(&[("https://cdn.example.org:1935/vod/a.m3u8", 90.0, 720)]);
        let (ok, rej) = run_gate(vec![cand("https://cdn.example.org:1935/vod/a.m3u8")], &f, &p).await;
        assert_eq!(ok.len(), 1, "{rej:?}");
        let (ok, rej) = run_gate(vec![cand("https://cdn.example.org:1936/vod/a.m3u8")], &f, &p).await;
        assert!(ok.is_empty());
        assert_eq!(rej[0].checks.last().unwrap().name, "egress");
    }

    /// One address proposed twice with different origins keeps the same copy
    /// whatever the order of the input.
    #[tokio::test]
    async fn a_repeated_address_keeps_the_same_copy_in_any_order() {
        let p = FakeProber::new(&[("https://cdn.example.org/a.mp4", 90.0, 720)]);
        let a = Candidate { origin: "llm:aaaa1111".into(), ..cand("https://cdn.example.org/a.mp4") };
        let b = Candidate { origin: "recipe:bbbb2222".into(), ..cand("https://cdn.example.org/a.mp4") };
        let (ok1, _) = run_gate(vec![a.clone(), b.clone()], &facts(), &p).await;
        let (ok2, _) = run_gate(vec![b, a], &facts(), &p).await;
        assert_eq!(ok1.len(), 1);
        assert_eq!(ok1[0].candidate.origin, ok2[0].candidate.origin);
    }

    #[tokio::test]
    async fn the_stated_length_must_match_within_the_tolerance() {
        let mut f = facts();
        f.expected_secs = Some(154.0);
        let p = FakeProber::new(&[
            ("https://cdn.example.org/off.mp4", 200.0, 720),
            ("https://cdn.example.org/near.mp4", 157.0, 720),
            ("https://cdn.example.org/far.mp4", 162.5, 720),
            ("https://cdn.example.org/edge.mp4", 161.5, 720),
        ]);
        let urls = ["off", "near", "far", "edge"].map(|n| cand(&format!("https://cdn.example.org/{n}.mp4")));
        let (ok, rej) = run_gate(urls.to_vec(), &f, &p).await;
        let names: Vec<&str> = ok.iter().map(|v| v.candidate.url.rsplit('/').next().unwrap()).collect();
        assert_eq!(names, vec!["edge.mp4", "near.mp4"], "ranked by url on a score tie");
        assert_eq!(rej.len(), 2);
        assert!(ok.iter().all(|v| v.score >= 900));
        // Tolerance is max(3 s, 5 %): 3 s for a short story.
        f.expected_secs = Some(30.0);
        let p = FakeProber::new(&[("https://cdn.example.org/a.mp4", 32.9, 720), ("https://cdn.example.org/b.mp4", 33.5, 720)]);
        let (ok, _) = run_gate(vec![cand("https://cdn.example.org/a.mp4"), cand("https://cdn.example.org/b.mp4")], &f, &p).await;
        assert_eq!(ok.len(), 1);
        assert!(ok[0].candidate.url.ends_with("a.mp4"));
    }

    #[tokio::test]
    async fn live_small_and_tiny_videos_are_rejected() {
        let mut p = FakeProber::new(&[("https://cdn.example.org/small.mp4", 90.0, 180), ("https://cdn.example.org/tiny.mp4", 3.0, 720)]);
        p.by_url.insert(
            "https://cdn.example.org/live.m3u8".into(),
            ProbeInfo { kind: ProbeKind::Direct, video_streams: 1, height: 720, duration_secs: None, live: false },
        );
        p.by_url.insert(
            "https://cdn.example.org/flag.m3u8".into(),
            ProbeInfo { kind: ProbeKind::Platform, video_streams: 1, height: 720, duration_secs: Some(100.0), live: true },
        );
        p.by_url.insert(
            "https://cdn.example.org/audio.mp4".into(),
            ProbeInfo { kind: ProbeKind::Direct, video_streams: 0, height: 0, duration_secs: Some(100.0), live: false },
        );
        let urls = ["small.mp4", "tiny.mp4", "live.m3u8", "flag.m3u8", "audio.mp4"].map(|n| cand(&format!("https://cdn.example.org/{n}")));
        let (ok, rej) = run_gate(urls.to_vec(), &facts(), &p).await;
        assert!(ok.is_empty(), "{ok:?}");
        assert_eq!(rej.len(), 5);
        let live = rej.iter().find(|r| r.candidate.url.ends_with("live.m3u8")).unwrap();
        assert!(live.checks.last().unwrap().detail.contains("Ζωντανή μετάδοση"));
    }

    #[tokio::test]
    async fn ranking_is_deterministic_and_prefers_expected_article_and_early() {
        let mut f = facts();
        f.expected_secs = Some(10.0);
        let items = [
            ("https://cdn.example.org/a.mp4", 10.0, 720),
            ("https://cdn.example.org/b.mp4", 11.0, 720),
            ("https://cdn.example.org/c.mp4", 9.0, 720),
            ("https://cdn.example.org/d.mp4", 10.0, 720),
        ];
        let make = |url: &str, pos: usize, art: bool| Candidate { position: Some(pos), in_article: Some(art), ..cand(url) };
        let cands = vec![
            make("https://cdn.example.org/a.mp4", 3, false),
            make("https://cdn.example.org/b.mp4", 1, true),
            make("https://cdn.example.org/c.mp4", 0, true),
            make("https://cdn.example.org/d.mp4", 2, true),
        ];
        let mut orders = Vec::new();
        for rot in 0..4 {
            let mut c = cands.clone();
            c.rotate_left(rot);
            let (ok, _) = run_gate(c, &f, &FakeProber::new(&items)).await;
            orders.push(ok.iter().map(|v| v.candidate.url.clone()).collect::<Vec<_>>());
        }
        assert!(orders.windows(2).all(|w| w[0] == w[1]), "{orders:?}");
        let names: Vec<&str> = orders[0].iter().map(|u| u.rsplit('/').next().unwrap()).collect();
        assert_eq!(names, vec!["c.mp4", "b.mp4", "d.mp4", "a.mp4"]);
    }

    #[tokio::test]
    async fn the_call_ends_by_the_deadline_with_timeout_rejections() {
        let mut p = FakeProber::new(&[
            ("https://cdn.example.org/a.mp4", 90.0, 720),
            ("https://cdn.example.org/b.mp4", 90.0, 720),
            ("https://cdn.example.org/c.mp4", 90.0, 720),
            ("https://cdn.example.org/d.mp4", 90.0, 720),
        ]);
        p.delay = Duration::from_secs(30);
        let started = Instant::now();
        let urls = ["a", "b", "c", "d"].map(|n| cand(&format!("https://cdn.example.org/{n}.mp4")));
        let (ok, rej) = verify(urls.to_vec(), &facts(), &|_| false, &p, &public_resolver(), Instant::now() + Duration::from_millis(300)).await;
        assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
        assert!(ok.is_empty());
        assert_eq!(rej.len(), 4);
        assert!(rej.iter().all(|r| r.checks.last().unwrap().detail.contains("χρόνος")));
    }

    #[test]
    fn ad_keys_ignore_query_case_of_host_and_entities() {
        assert_eq!(ad_key("https://CDN.x.org/a/b.mp4?q=1&amp;r=2"), ad_key("https://cdn.x.org/a/b.mp4"));
        assert_ne!(ad_key("https://cdn.x.org/a/b.mp4"), ad_key("https://cdn.x.org/a/c.mp4"));
        assert_eq!(ad_key("not a url?x=1"), "not a url");
    }

    #[test]
    fn ffprobe_json_is_read_and_attached_pictures_are_not_video() {
        let j = r#"{"streams":[{"codec_type":"video","height":720,"disposition":{"attached_pic":0}},{"codec_type":"audio"}],"format":{"duration":"12.5"}}"#;
        let i = parse_ffprobe(j).unwrap();
        assert_eq!((i.video_streams, i.height, i.duration_secs), (1, 720, Some(12.5)));
        let cover = r#"{"streams":[{"codec_type":"video","height":1000,"disposition":{"attached_pic":1}}],"format":{"duration":"60"}}"#;
        assert_eq!(parse_ffprobe(cover).unwrap().video_streams, 0);
        let live = r#"{"streams":[{"codec_type":"video","height":720}],"format":{"duration":"N/A"}}"#;
        assert_eq!(parse_ffprobe(live).unwrap().duration_secs, None);
        assert!(parse_ffprobe("nope").is_err());
    }

    #[test]
    fn media_addresses_are_told_from_pages() {
        let u = |s: &str| url::Url::parse(s).unwrap();
        assert!(is_direct_media(&u("https://a.example/v/x.MP4?sig=1")));
        assert!(is_direct_media(&u("https://a.example/live/master.m3u8")));
        assert!(!is_direct_media(&u("https://a.example/watch?v=1")));
    }

    #[tokio::test]
    async fn the_serialized_address_is_what_every_check_and_the_prober_see() {
        let serialized = "http://cdn.example.org/@10.0.0.1/a.mp4";
        let p = FakeProber::new(&[(serialized, 90.0, 720)]);
        let seen_by_ad = std::sync::Mutex::new(Vec::new());
        let ad = |u: &str| {
            seen_by_ad.lock().unwrap().push(u.to_string());
            false
        };
        let raw = r"http://cdn.example.org\@10.0.0.1/a.mp4";
        let (ok, rej) = verify(vec![cand(raw), cand(serialized)], &facts(), &ad, &p, &public_resolver(), soon()).await;
        assert!(rej.is_empty(), "{rej:?}");
        assert_eq!(ok.len(), 1, "the raw and the serialized forms are one candidate");
        assert_eq!(ok[0].candidate.url, serialized);
        assert_eq!(p.calls.load(Ordering::SeqCst), 1);
        assert!(seen_by_ad.lock().unwrap().iter().all(|u| u == serialized));
    }

    #[tokio::test]
    async fn a_listed_ad_is_caught_through_a_different_spelling_of_its_address() {
        let mut f = facts();
        f.ad_media.insert("https://cdn.example.org/ads/spot.mp4".into());
        let p = FakeProber::new(&[]);
        let (ok, rej) = verify(vec![cand(r"https://cdn.example.org\ads/spot.mp4")], &f, &|_| false, &p, &public_resolver(), soon()).await;
        assert!(ok.is_empty());
        assert_eq!(rej[0].candidate.url, "https://cdn.example.org/ads/spot.mp4");
        assert_eq!(rej[0].checks.last().unwrap().name, "ad");
    }

    #[test]
    fn a_missing_position_ranks_after_every_real_one() {
        let with = |p: Option<usize>| Candidate { position: p, ..cand("https://cdn.example.org/a.mp4") };
        assert!(score(&with(Some(5)), false) > score(&with(None), false));
        assert!(score(&with(Some(49)), false) > score(&with(None), false));
        assert!(order_key(&with(Some(5))) < order_key(&with(None)));
    }
}
