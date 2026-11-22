//! The Public Suffix List (P14.6): which ending of a host is a public suffix
//! (`gr`, `com.gr`, `co.uk`, `kawasaki.jp`'s cities...), so
//! [`crate::urlnorm::registrable_domain`] names the site and not a guess.
//!
//! The list is `data/public_suffix_list.dat`, embedded unmodified from
//! <https://publicsuffix.org/list/public_suffix_list.dat> (its version line is
//! at the top of the file). That file is under the Mozilla Public License 2.0,
//! whose copyleft is per file: it ships as it came, with its licence header,
//! and nothing else in this project takes its licence. Updating it means
//! replacing the file with a newer copy; nothing is fetched at run time.
//!
//! Only the ICANN section is used. The private section (`github.io`,
//! `blogspot.com`, `cloudfront.net`...) would make every customer subdomain
//! of a hosting or CDN company a site of its own, which per-site pacing,
//! statistics and cookie jars do not want.

use std::collections::HashSet;
use std::sync::OnceLock;

static LIST: &str = include_str!("../data/public_suffix_list.dat");

/// The ICANN rules, each in its Unicode and its punycode form.
struct Rules {
    /// `com.gr`: the suffix itself.
    exact: HashSet<String>,
    /// `*.kawasaki.jp` stored as `kawasaki.jp`: any one label under it.
    wildcard: HashSet<String>,
    /// `!city.kawasaki.jp` stored as `city.kawasaki.jp`: not a suffix after all.
    exception: HashSet<String>,
}

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| parse(LIST))
}

/// The ICANN section of `list`.
fn parse(list: &str) -> Rules {
    let mut rules = Rules { exact: HashSet::new(), wildcard: HashSet::new(), exception: HashSet::new() };
    let mut in_icann = false;
    for line in list.lines() {
        let line = line.trim();
        if line.starts_with("// ===BEGIN ICANN DOMAINS===") {
            in_icann = true;
            continue;
        }
        if line.starts_with("// ===END ICANN DOMAINS===") {
            break;
        }
        if !in_icann || line.is_empty() || line.starts_with("//") {
            continue;
        }
        // A rule is the line's first word.
        let Some(rule) = line.split_whitespace().next() else { continue };
        let (set, rule) = if let Some(r) = rule.strip_prefix('!') {
            (&mut rules.exception, r)
        } else if let Some(r) = rule.strip_prefix("*.") {
            (&mut rules.wildcard, r)
        } else {
            (&mut rules.exact, rule)
        };
        let rule = rule.to_lowercase();
        if let Some(puny) = punycode(&rule) {
            set.insert(puny);
        }
        set.insert(rule);
    }
    rules
}

/// The ASCII (punycode) form of a non-ASCII domain, `None` for an ASCII one.
fn punycode(domain: &str) -> Option<String> {
    if domain.is_ascii() {
        return None;
    }
    let url = reqwest::Url::parse(&format!("http://{domain}/")).ok()?;
    url.host_str().map(str::to_string).filter(|h| h != domain)
}

/// The longest host DNS allows, in bytes; a longer one is not a host name
/// and is not looked up (P14.6b).
pub const MAX_HOST_BYTES: usize = 253;

/// The labels at the end of a host that are matched: the list's longest rule
/// has 5 (with an exception's extra one, 6), so 8 is ample and keeps a long
/// host's lookup short (P14.6b).
const MATCHED_LABELS: usize = 8;

/// How many of `labels` (a host's labels, left to right) form its public
/// suffix, by the list's rules: an exception rule wins, then the longest
/// matching rule; with none, the last label (the list's implicit `*`). Only
/// the last [`MATCHED_LABELS`] are looked at.
pub fn suffix_labels(labels: &[&str]) -> usize {
    let rules = rules();
    let labels = &labels[labels.len().saturating_sub(MATCHED_LABELS)..];
    let n = labels.len();
    for i in 0..n {
        let candidate = labels[i..].join(".");
        if rules.exception.contains(&candidate) {
            return n - i - 1;
        }
        if rules.exact.contains(&candidate) {
            return n - i;
        }
        if i + 1 < n && rules.wildcard.contains(&labels[i + 1..].join(".")) {
            return n - i;
        }
    }
    n.min(1)
}

/// The registrable domain of `host` (already lowercased): its public suffix
/// and one label more. A host that is a public suffix itself, an IP address,
/// or longer than [`MAX_HOST_BYTES`] (no host name; a mailed link can be any
/// length, P14.6b) is returned as it is.
pub fn registrable(host: &str) -> String {
    if host.len() > MAX_HOST_BYTES || host.parse::<std::net::IpAddr>().is_ok() {
        return host.to_string();
    }
    let lowered;
    let host = if host.is_ascii() {
        host
    } else {
        lowered = host.to_lowercase();
        lowered.as_str()
    };
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    let suffix = suffix_labels(&labels);
    if labels.len() <= suffix {
        return labels.join(".");
    }
    labels[labels.len() - suffix - 1..].join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_list_is_the_icann_section_only() {
        let r = rules();
        assert!(r.exact.contains("gr") && r.exact.contains("com.gr") && r.exact.contains("co.uk"));
        assert!(r.wildcard.contains("ck") && r.exception.contains("www.ck"));
        assert!(r.exact.contains("ελ") && r.exact.contains("xn--qxam"), "Unicode and punycode forms");
        assert!(!r.exact.contains("blogspot.com") && !r.exact.contains("github.io"), "no private section");
        assert!(r.exact.len() > 5000, "the whole ICANN list: {}", r.exact.len());
        assert!(LIST.starts_with("// This Source Code Form is subject to the terms of the Mozilla Public"), "embedded unmodified, with its licence header");
    }

    #[test]
    fn suffixes_wildcards_and_exceptions() {
        for (host, want) in [
            ("www.in.gr", "in.gr"),
            ("sport.protothema.gr", "protothema.gr"),
            ("news.site.com.gr", "site.com.gr"),
            ("www.ypes.gov.gr", "ypes.gov.gr"),
            ("news.bbc.co.uk", "bbc.co.uk"),
            ("a.b.example.com.au", "example.com.au"),
            ("foo.bar.example.ck", "bar.example.ck"), // *.ck: example.ck is a suffix
            ("www.ck", "www.ck"),                     // !www.ck: not a suffix
            ("a.city.kawasaki.jp", "city.kawasaki.jp"),
            ("a.b.other.kawasaki.jp", "b.other.kawasaki.jp"),
            ("user.blogspot.com", "blogspot.com"), // the private section is not used
            ("www.example.unknowntld", "example.unknowntld"),
            ("ειδήσεις.ελ", "ειδήσεις.ελ"),
            ("www.xn--hxakic4aa.xn--qxam", "xn--hxakic4aa.xn--qxam"),
            ("192.168.10.4", "192.168.10.4"),
            ("co.uk", "co.uk"),
            ("localhost", "localhost"),
            ("pio.gov.cy", "pio.gov.cy"), // `gov.cy`: a suffix the old two-label table lacked
            ("www.pio.gov.cy", "pio.gov.cy"),
        ] {
            assert_eq!(registrable(host), want, "{host}");
        }
    }

    /// P14.6b: a host of any length is answered at once. Longer than DNS allows,
    /// it is returned as it is; within the limit, only its last labels are matched.
    #[test]
    fn a_huge_host_is_answered_at_once() {
        let huge = "a.".repeat(512 * 1024) + "example.com"; // about 1 MB
        let started = std::time::Instant::now();
        assert_eq!(registrable(&huge), huge, "not a host name: returned as it is");
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
        let long = "a.".repeat(120) + "news.example.com.gr"; // 259 bytes: over the limit
        assert_eq!(registrable(&long), long);
        let deep = "a.".repeat(110) + "example.com.gr"; // 234 bytes, 113 labels
        assert_eq!(registrable(&deep), "example.com.gr");
    }
}
