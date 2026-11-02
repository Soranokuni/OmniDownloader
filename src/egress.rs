//! The job path's egress rule, as `run_job` applies it (plan P2.12): no request to a
//! loopback, private, link-local or reserved address, for the job's link, for anything
//! derived from it, or for any stream the sniffer or yt-dlp's page scan finds. The rule
//! itself is `omni_core::net`; this is what a job does with its verdict.

use std::time::Duration;

use anyhow::Result;
use omni_broadcast::errors::ErrorCode;
use omni_core::models::{JobStage, JobStatus};
use omni_core::net::{CidrSet, EgressRefusal};
use omni_core::repository::Repository;
use tracing::warn;

use crate::repair::Landing;

/// How long a name lookup of the egress rule may take on the job path.
pub const EGRESS_LOOKUP: Duration = Duration::from_secs(4);

/// What the desk reads when the rule refused a link that is not a plain web address.
pub const MALFORMED_LINK_EL: &str = "Ο σύνδεσμος δεν είναι απλή διεύθυνση ιστού (π.χ. περιέχει όνομα χρήστη)· δεν κατεβαίνει αυτόματα.";

/// The rule for an address the job path is about to request. The verdict is on the parsed form;
/// `Ok` is the input as written (trimmed, scheme lowercased, nothing percent-encoded), so later
/// readers still see the Greek words in the path. An address with a backslash, a control character
/// or whitespace is refused as ambiguous: parsers disagree on its host. A name that did not resolve, or not in time, is
/// not a verdict: the download fails on it as it would have, and retries as usual. Any other
/// refusal (a scheme that is not http(s), an address that cannot be read) is an `Err`.
pub async fn vet_url(url: &str, allow: &CidrSet) -> Result<String, EgressRefusal> {
    vet_url_with(url, allow, omni_core::net::system_lookup).await
}

pub async fn vet_url_with<R, Fut>(url: &str, allow: &CidrSet, resolve: R) -> Result<String, EgressRefusal>
where
    R: Fn(String, u16) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>>,
{
    // The rule itself refuses a backslash, control character or whitespace as ambiguous.
    let text = url.trim();
    // Only the scheme is lowercased: host case and the path (Greek words included) stay as sent.
    let used = match text.split_once("://") {
        Some((scheme, rest)) => format!("{}://{rest}", scheme.to_ascii_lowercase()),
        None => text.to_string(),
    };
    match omni_core::net::check_public_with(&used, allow, EGRESS_LOOKUP, resolve).await {
        Ok(_) => Ok(used),
        Err(r) if r.is_unknown() => {
            tracing::debug!("egress check inconclusive: {r}");
            Ok(used)
        }
        Err(r) => Err(r),
    }
}

/// Where a page's redirects ended, under the rule: `Some(address)` to use, `None` when the
/// lookup says nothing, `Err` for an internal landing. A hop refused as internal is let
/// through only when the allowlist admits that host.
pub async fn vet_landing_with<R, Fut>(landing: Landing, allow: &CidrSet, resolve: R) -> Result<Option<String>, EgressRefusal>
where
    R: Fn(String, u16) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>>,
{
    match landing {
        Landing::At(url) => vet_url_with(&url, allow, resolve).await.map(Some),
        Landing::Internal(host) => {
            let shown = if host.contains(':') { format!("[{host}]") } else { host };
            vet_url_with(&format!("http://{shown}/"), allow, resolve).await.map(|_| None)
        }
        Landing::Unknown => Ok(None),
    }
}

/// The candidates left after the screen.
#[derive(Debug, PartialEq, Eq)]
pub enum Screened {
    /// Continue with `urls` only; `dropped` are the refused internal hosts (once each) and
    /// `unreadable` the candidates dropped as unreadable, for the job's events.
    Kept { urls: Vec<String>, dropped: Vec<String>, unreadable: usize },
    /// Every candidate was refused as internal (and there was at least one): the job ends, and
    /// no other reader (yt-dlp's own fallback included) is sent to the same address.
    AllInternal { dropped: Vec<String> },
}

pub async fn screen_candidates(urls: Vec<String>, allow: &CidrSet) -> Screened {
    screen_candidates_with(urls, allow, omni_core::net::system_lookup).await
}

pub async fn screen_candidates_with<R, Fut>(urls: Vec<String>, allow: &CidrSet, resolve: R) -> Screened
where
    R: Fn(String, u16) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>> + Send + 'static,
{
    let out = omni_core::net::screen_public_with(urls, allow, EGRESS_LOOKUP, resolve).await;
    // Unreadable candidates are dropped but never end the job: only an internal refusal does.
    // (The sniffer's candidates can be relative; the page scan has its own rule.)
    if out.kept.is_empty() && out.any_non_public {
        Screened::AllInternal { dropped: out.dropped }
    } else {
        Screened::Kept { urls: out.kept, dropped: out.dropped, unreadable: out.unreadable }
    }
}

/// What the page scan's screen decided for `ytdlp_attempt`.
#[derive(Debug, PartialEq, Eq)]
pub enum PageScanVerdict {
    /// Go on with `videos` (the scan's videos that passed); the rest is for the job's events.
    Continue { videos: Vec<String>, dropped: Vec<String>, unreadable: usize },
    /// A stream of the page, or the one yt-dlp would pick, is on an internal address: the job
    /// ends, and yt-dlp does not read the page.
    EndInternal { hosts: Vec<String> },
    /// The page's streams are only unreadable or ambiguous addresses, or the one yt-dlp would
    /// pick is: the job ends, and yt-dlp does not read the page and pick the same address.
    EndUnreadable,
}

pub async fn screen_page_scan(videos: Vec<String>, media_urls: &[String], allow: &CidrSet) -> PageScanVerdict {
    screen_page_scan_with(videos, media_urls, allow, omni_core::net::system_lookup).await
}

/// The page scan's videos and yt-dlp's own pick (`PageScan::fetch_urls`, which yt-dlp downloads on a page
/// with a single video) screened together, so each host is judged once. An internal address among
/// the media ends the job. So does any other refused one. A media address with no verdict does not. Of the
/// videos, ending needs that none is left and something was refused.
pub async fn screen_page_scan_with<R, Fut>(videos: Vec<String>, media_urls: &[String], allow: &CidrSet, resolve: R) -> PageScanVerdict
where
    R: Fn(String, u16) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>> + Send + 'static,
{
    let n = videos.len();
    let all: Vec<String> = videos.iter().chain(media_urls.iter()).cloned().collect();
    if all.is_empty() {
        return PageScanVerdict::Continue { videos, dropped: Vec::new(), unreadable: 0 };
    }
    let out = omni_core::net::screen_public_with(all, allow, EGRESS_LOOKUP, resolve).await;
    let (own, media) = out.refusals.split_at(n);
    if media.iter().flatten().any(|r| matches!(r, EgressRefusal::NonPublic(_))) {
        return PageScanVerdict::EndInternal { hosts: out.dropped };
    }
    // Any other refusal of a media address (ambiguous, user info, one the URL parser cannot read such
    // as a zone id `[fe80::1%25eth0]`) ends it too: yt-dlp's own reader may still accept it. Every
    // media address is absolute http(s) already, so a public page loses nothing here.
    if media.iter().flatten().next().is_some() {
        return PageScanVerdict::EndUnreadable;
    }
    let kept: Vec<String> = videos.into_iter().zip(own).filter(|(_, r)| r.is_none()).map(|(u, _)| u).collect();
    let refused = own.iter().flatten().count();
    if kept.is_empty() && refused > 0 {
        return if out.any_non_public { PageScanVerdict::EndInternal { hosts: out.dropped } } else { PageScanVerdict::EndUnreadable };
    }
    let dropped: Vec<String> = own
        .iter()
        .flatten()
        .filter_map(|r| match r {
            EgressRefusal::NonPublic(h) => Some(h.clone()),
            _ => None,
        })
        .fold(Vec::new(), |mut hosts, h| {
            if !hosts.contains(&h) {
                hosts.push(h);
            }
            hosts
        });
    let internal = own.iter().flatten().filter(|r| matches!(r, EgressRefusal::NonPublic(_))).count();
    PageScanVerdict::Continue { videos: kept, dropped, unreadable: refused - internal }
}

/// What the desk reads for a refusal: the host only, never the path or query; a link that is
/// not a plain web address says so (no credentials are ever echoed).
pub fn refusal_message(why: &EgressRefusal) -> String {
    match why.host() {
        Some(host) => format!("{host}: {}", ErrorCode::NonPublicAddress.hint_el()),
        None => MALFORMED_LINK_EL.to_string(),
    }
}

/// End a job whose address, or a replacement for it, may not be requested: review, no request
/// made, the code NON_PUBLIC_ADDRESS, the same text as the job's event. The lease holder ends it.
fn end_job_non_public(repo: &Repository, job_id: i64, owner: &str, message: &str) -> Result<()> {
    if crate::still_ours(repo, job_id, owner) {
        repo.record_event(job_id, "ERROR", Some(JobStage::Extract), message)?;
    }
    repo.finish(job_id, owner, JobStatus::RequiresReview, Some(ErrorCode::NonPublicAddress.as_str()), Some(message), None)?;
    Ok(())
}

pub fn end_job_internal_address(repo: &Repository, job_id: i64, owner: &str, why: &EgressRefusal) -> Result<()> {
    warn!("Job #{job_id}: the address is refused ({}); not requested", why.host().unwrap_or("not a plain web address"));
    end_job_non_public(repo, job_id, owner, &refusal_message(why))
}

/// Every candidate the job found is on an internal address.
pub fn end_job_internal_hosts(repo: &Repository, job_id: i64, owner: &str, hosts: &[String]) -> Result<()> {
    warn!("Job #{job_id}: every video found is on an internal address ({}); not requested", hosts.join(", "));
    end_job_non_public(repo, job_id, owner, &format!("{}: {}", hosts.join(", "), ErrorCode::NonPublicAddress.hint_el()))
}

/// Every stream the job found, or the one yt-dlp would take, has an address that is not a plain
/// web address (a backslash, user info, whitespace): not called internal, since nothing says it is.
pub fn end_job_unreadable(repo: &Repository, job_id: i64, owner: &str) -> Result<()> {
    warn!("Job #{job_id}: every video found has an unreadable address; not requested");
    end_job_non_public(repo, job_id, owner, MALFORMED_LINK_EL)
}

/// The events for the candidates a screen dropped while others remain, as (level, text): the
/// internal hosts under their own heading, the unreadable ones counted apart.
pub fn dropped_event_texts(hosts: &[String], unreadable: usize) -> Vec<(&'static str, String)> {
    let mut events = Vec::new();
    if !hosts.is_empty() {
        events.push(("WARN", format!("Streams on internal addresses were not used: {}", hosts.join(", "))));
    }
    if unreadable > 0 {
        events.push(("WARN", format!("Streams with an unreadable address were not used: {unreadable}")));
    }
    events
}

pub fn record_dropped_hosts(repo: &Repository, job_id: i64, hosts: &[String], unreadable: usize) -> Result<()> {
    if !hosts.is_empty() {
        warn!("Job #{job_id}: streams on internal addresses dropped: {}", hosts.join(", "));
    }
    if unreadable > 0 {
        warn!("Job #{job_id}: {unreadable} stream(s) with an unreadable address dropped");
    }
    for (level, text) in dropped_event_texts(hosts, unreadable) {
        repo.record_event(job_id, level, Some(JobStage::Extract), &text)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    /// A resolver for names the test makes up: `intra.example` is internal, `cdn.example` public,
    /// anything else does not resolve.
    async fn dns(host: String, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        let ip = match host.as_str() {
            "cdn.example" => "93.184.216.34",
            "intra.example" => "10.0.0.5",
            _ => return Err(std::io::Error::other("no such host")),
        };
        Ok(vec![SocketAddr::new(ip.parse().unwrap(), port)])
    }

    fn none() -> CidrSet {
        CidrSet::default()
    }

    #[tokio::test]
    async fn a_url_is_vetted_by_its_parsed_form_and_used_normalised() {
        // However it is written, an internal address is refused.
        for u in ["HTTP://169.254.169.254/latest", "Http://10.0.0.5/v.mp4", "http://INTRA.example/x"] {
            let r = vet_url_with(u, &none(), dns).await;
            assert!(matches!(r, Err(EgressRefusal::NonPublic(_))), "{u}: {r:?}");
        }
        // Parsers (WHATWG, Python) read a different host from these: refused before any lookup.
        for u in ["http://public.example\\@127.0.0.1/", "https://cdn .example/a", "https://cdn.example/a\tb", "https://cdn.example/a\nb", "https://cdn.example/a\u{7f}"] {
            let r = vet_url_with(u, &none(), dns).await;
            assert!(matches!(r, Err(EgressRefusal::Malformed("ambiguous"))), "{u:?}: {r:?}");
        }
        // A space in the path cannot move the host: the link passes as written.
        assert_eq!(vet_url_with("https://cdn.example/clip 1.mp4", &none(), dns).await.unwrap(), "https://cdn.example/clip 1.mp4");
        // A scheme that is not http(s) is refused, not skipped.
        assert!(matches!(vet_url_with("ftp://cdn.example/v", &none(), dns).await, Err(EgressRefusal::Malformed(_))));
        // Public: the input comes back, trimmed, with only the scheme lowercased.
        assert_eq!(vet_url_with(" HTTPS://cdn.example/a ", &none(), dns).await.unwrap(), "https://cdn.example/a");
        assert_eq!(vet_url_with("HTTP://Host.Example/ΑΒΓ", &none(), dns).await.unwrap(), "http://Host.Example/ΑΒΓ");
        // A lookup that failed is not a verdict.
        assert!(vet_url_with("http://gone.example/", &none(), dns).await.is_ok());
    }

    #[tokio::test]
    async fn a_greek_path_comes_back_unencoded_so_the_annotation_is_still_found() {
        let u = "https://cdn.example/arthro/-ΒΙΝΤΕΟ";
        let got = vet_url_with(u, &none(), dns).await.unwrap();
        assert_eq!(got, u);
        assert!(!got.contains('%'), "{got}");
        assert_eq!(omni_email::parser::without_annotation(&got), "https://cdn.example/arthro/");
    }

    #[tokio::test]
    async fn a_replacement_address_is_judged_like_the_job_address() {
        // The dead-link repair, the site API's answer and the landing all go through `vet_url`.
        assert!(matches!(vet_url_with("http://intra.example/fixed", &none(), dns).await, Err(EgressRefusal::NonPublic(_))));
        let allow = CidrSet::parse_lossy(&["10.0.0.0/8".to_string()]);
        assert!(vet_url_with("http://intra.example/fixed", &allow, dns).await.is_ok());
    }

    #[tokio::test]
    async fn an_internal_landing_is_refused_a_failed_lookup_is_not() {
        let r = vet_landing_with(Landing::Internal("127.0.0.1".into()), &none(), dns).await;
        assert!(matches!(r, Err(EgressRefusal::NonPublic(ref h)) if h == "127.0.0.1"), "{r:?}");
        let r = vet_landing_with(Landing::Internal("::1".into()), &none(), dns).await;
        assert!(matches!(r, Err(EgressRefusal::NonPublic(_))), "{r:?}");
        let r = vet_landing_with(Landing::At("http://10.0.0.5/story".into()), &none(), dns).await;
        assert!(matches!(r, Err(EgressRefusal::NonPublic(_))), "{r:?}");
        assert_eq!(vet_landing_with(Landing::Unknown, &none(), dns).await.unwrap(), None);
        assert_eq!(
            vet_landing_with(Landing::At("https://cdn.example/story".into()), &none(), dns).await.unwrap().as_deref(),
            Some("https://cdn.example/story")
        );
        // Allowlisted: the refusal does not stand.
        let allow = CidrSet::parse_lossy(&["127.0.0.0/8".to_string()]);
        assert_eq!(vet_landing_with(Landing::Internal("127.0.0.1".into()), &allow, dns).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_screen_keeps_the_public_ends_on_nothing_public_and_never_empties_by_unknown() {
        let urls = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Mixed: only the public (and the unresolved) remain; the internal host is named once.
        let got = screen_candidates_with(urls(&["https://cdn.example/a.m3u8", "http://intra.example/b", "http://intra.example/c", "http://gone.example/d"]), &none(), dns).await;
        assert_eq!(
            got,
            Screened::Kept { urls: urls(&["https://cdn.example/a.m3u8", "http://gone.example/d"]), dropped: urls(&["intra.example"]), unreadable: 0 }
        );
        // Nothing public left and something refused: the job ends (no yt-dlp fallback).
        let got = screen_candidates_with(urls(&["http://intra.example/b", "http://10.0.0.9/c"]), &none(), dns).await;
        assert_eq!(got, Screened::AllInternal { dropped: urls(&["intra.example", "10.0.0.9"]) });
        // An empty list, or only unknown ones, is not "all internal".
        assert_eq!(screen_candidates_with(vec![], &none(), dns).await, Screened::Kept { urls: vec![], dropped: vec![], unreadable: 0 });
        assert!(matches!(screen_candidates_with(urls(&["http://gone.example/x"]), &none(), dns).await, Screened::Kept { .. }));
    }

    #[tokio::test]
    async fn an_ambiguous_candidate_is_dropped_but_ends_the_job_only_with_an_internal_one() {
        let urls = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let bad = "http://x.invalid\\@10.0.0.5/v.mp4";
        let got = screen_candidates_with(urls(&[bad, "https://cdn.example/a"]), &none(), dns).await;
        assert_eq!(got, Screened::Kept { urls: urls(&["https://cdn.example/a"]), dropped: vec![], unreadable: 1 });
        let got = screen_candidates_with(urls(&[bad]), &none(), dns).await;
        assert_eq!(got, Screened::Kept { urls: vec![], dropped: vec![], unreadable: 1 });
        let got = screen_candidates_with(urls(&[bad, "http://intra.example/b"]), &none(), dns).await;
        assert_eq!(got, Screened::AllInternal { dropped: urls(&["intra.example"]) });
        let got = screen_candidates_with(urls(&["/media/v.mp4"]), &none(), dns).await;
        assert!(matches!(got, Screened::Kept { ref urls, .. } if urls.is_empty()), "{got:?}");
    }

    fn urls(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn yt_dlps_own_pick_is_screened_before_it_reads_the_page() {
        // A single-video page: no `videos`, the pick is in `media_urls` (an og:video on a metadata address).
        let v = screen_page_scan_with(vec![], &urls(&["http://169.254.169.254/latest/x"]), &none(), dns).await;
        assert_eq!(v, PageScanVerdict::EndInternal { hosts: urls(&["169.254.169.254"]) });
        // An internal name as the pick, next to a public one.
        let v = screen_page_scan_with(vec![], &urls(&["https://cdn.example/a", "http://intra.example/b"]), &none(), dns).await;
        assert!(matches!(v, PageScanVerdict::EndInternal { ref hosts } if hosts == &urls(&["intra.example"])), "{v:?}");
        // Public media and public videos: unchanged.
        let v = screen_page_scan_with(urls(&["https://cdn.example/v1"]), &urls(&["https://cdn.example/m"]), &none(), dns).await;
        assert_eq!(v, PageScanVerdict::Continue { videos: urls(&["https://cdn.example/v1"]), dropped: vec![], unreadable: 0 });
        // Nothing at all.
        assert!(matches!(screen_page_scan_with(vec![], &[], &none(), dns).await, PageScanVerdict::Continue { .. }));
        // A media address whose lookup fails is no verdict.
        let v = screen_page_scan_with(vec![], &urls(&["http://gone.example/m.mp4"]), &none(), dns).await;
        assert!(matches!(v, PageScanVerdict::Continue { .. }), "{v:?}");
        // A media address the URL parser cannot read (a link-local zone id) is not handed to yt-dlp.
        let v = screen_page_scan_with(vec![], &urls(&["http://[fe80::1%25eth0]/v.mp4"]), &none(), dns).await;
        assert_eq!(v, PageScanVerdict::EndUnreadable);
    }

    #[tokio::test]
    async fn a_page_scan_of_only_unreadable_addresses_ends_the_job_instead_of_sending_yt_dlp_to_the_page() {
        let v = screen_page_scan_with(urls(&["http://x.invalid\\@10.0.0.5/v.mp4"]), &[], &none(), dns).await;
        assert_eq!(v, PageScanVerdict::EndUnreadable);
        // An ambiguous media address ends it as well, with a public video next to it.
        let v = screen_page_scan_with(urls(&["https://cdn.example/v1"]), &urls(&["http://user:pw@cdn.example/v"]), &none(), dns).await;
        assert_eq!(v, PageScanVerdict::EndUnreadable);
        // Internal and unreadable together, nothing kept: internal is named, the unreadable are not.
        let v = screen_page_scan_with(urls(&["http://a b/", "http://intra.example/x"]), &[], &none(), dns).await;
        assert_eq!(v, PageScanVerdict::EndInternal { hosts: urls(&["intra.example"]) });
        // One public video left: the page goes on, the refused ones are counted apart.
        let v = screen_page_scan_with(urls(&["http://a b/", "http://intra.example/x", "https://cdn.example/v"]), &[], &none(), dns).await;
        assert_eq!(v, PageScanVerdict::Continue { videos: urls(&["https://cdn.example/v"]), dropped: urls(&["intra.example"]), unreadable: 1 });
    }

    #[test]
    fn the_internal_heading_never_carries_an_unreadable_address() {
        let ev = dropped_event_texts(&urls(&["intra.example"]), 2);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].1, "Streams on internal addresses were not used: intra.example");
        assert_eq!(ev[1].1, "Streams with an unreadable address were not used: 2");
        assert!(!ev[0].1.contains("(unreadable"), "{ev:?}");
        // Unreadable only: no internal heading at all.
        let ev = dropped_event_texts(&[], 1);
        assert_eq!(ev.len(), 1);
        assert!(!ev[0].1.contains("internal"), "{ev:?}");
        assert!(dropped_event_texts(&[], 0).is_empty());
    }

    #[test]
    fn messages_name_the_host_only_and_a_malformed_link_says_so() {
        let m = refusal_message(&EgressRefusal::NonPublic("10.0.0.5".into()));
        assert!(m.starts_with("10.0.0.5: "), "{m}");
        let m = refusal_message(&EgressRefusal::Malformed("userinfo"));
        assert_eq!(m, MALFORMED_LINK_EL);
        assert!(!m.contains("εσωτερική"), "a malformed link is not called internal: {m}");
    }

    /// `run_job` cannot be driven to the sniffer, the page scan or the landing without the real
    /// browser and yt-dlp, so their decisions are the functions above; this keeps the calls in
    /// `run_job` and `ytdlp_attempt` from being deleted without a test failing.
    #[test]
    fn every_enforcement_point_is_still_wired_into_the_job_path() {
        let main = include_str!("main.rs");
        for (call, times) in [
            ("vet_url(&orig_url, egress_allow)", 1),
            ("vet_url(&fixed, egress_allow)", 1),
            ("vet_url(&video, egress_allow)", 1),
            ("vet_landing_with(repair::final_page_landing(&u).await, egress_allow", 1),
            ("screen_candidates(pool, egress_allow)", 1),
            ("screen_page_scan(std::mem::take(&mut scan.videos), &scan.fetch_urls, egress_allow)", 1),
            ("egress::end_job_internal_hosts(", 2),
            ("egress::end_job_unreadable(", 1),
        ] {
            assert_eq!(main.matches(call).count(), times, "`{call}` must appear {times} time(s) in main.rs");
        }
    }
}
