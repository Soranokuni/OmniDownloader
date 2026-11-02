//! CIDR parsing and matching for the MCR open-access allowlist (plan P2.1).
//!
//! The panel is allowed to skip login for clients on a configured newsroom
//! subnet, which means an IP test sits directly in front of every unauthenticated
//! read of the queue. That test is small enough to own rather than to take a
//! dependency for, and owning it means the failure mode is explicit: a CIDR that
//! does not parse is **rejected at load time**, not silently treated as
//! "matches nothing" (a typo would then quietly lock MCR out) and never as
//! "matches everything" (a typo would then quietly open the station up).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// A parsed CIDR block, e.g. `10.20.0.0/16` or `::1/128`.
///
/// A bare address without a prefix length is accepted and means a single host
/// (`/32` for IPv4, `/128` for IPv6) — operators write `127.0.0.1` far more
/// often than `127.0.0.1/32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix_len: u8,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CidrParseError {
    #[error("`{0}` is not an IP address or CIDR block")]
    BadAddress(String),
    #[error("prefix length `{0}` is not a number")]
    BadPrefix(String),
    #[error("prefix length /{prefix} is out of range for {family} (max /{max})")]
    PrefixOutOfRange {
        prefix: u32,
        family: &'static str,
        max: u8,
    },
}

impl Cidr {
    /// The network address with all host bits cleared, and the prefix length.
    pub fn new(addr: IpAddr, prefix_len: u8) -> Result<Self, CidrParseError> {
        let max = match addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if prefix_len > max {
            return Err(CidrParseError::PrefixOutOfRange {
                prefix: prefix_len as u32,
                family: if max == 32 { "IPv4" } else { "IPv6" },
                max,
            });
        }
        Ok(Self {
            network: mask(addr, prefix_len),
            prefix_len,
        })
    }

    /// True when `ip` falls inside this block.
    ///
    /// An IPv4-mapped IPv6 address (`::ffff:10.0.0.5`, which is what a
    /// dual-stack listener reports for an IPv4 client) is unmapped first, so an
    /// operator writing `10.0.0.0/8` does not have to also write the v6 form.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = unmap_v4(ip);
        match (self.network, ip) {
            (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_)) => {
                mask(ip, self.prefix_len) == self.network
            }
            // Different families never overlap.
            _ => false,
        }
    }

    pub fn prefix_len(&self) -> u8 {
        self.prefix_len
    }

    pub fn network(&self) -> IpAddr {
        self.network
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix_len)
    }
}

impl FromStr for Cidr {
    type Err = CidrParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        match s.split_once('/') {
            Some((addr, prefix)) => {
                let addr: IpAddr = addr
                    .trim()
                    .parse()
                    .map_err(|_| CidrParseError::BadAddress(s.to_string()))?;
                let prefix: u32 = prefix
                    .trim()
                    .parse()
                    .map_err(|_| CidrParseError::BadPrefix(prefix.trim().to_string()))?;
                let max = if addr.is_ipv4() { 32u8 } else { 128u8 };
                if prefix > max as u32 {
                    return Err(CidrParseError::PrefixOutOfRange {
                        prefix,
                        family: if addr.is_ipv4() { "IPv4" } else { "IPv6" },
                        max,
                    });
                }
                Cidr::new(addr, prefix as u8)
            }
            None => {
                let addr: IpAddr = s
                    .parse()
                    .map_err(|_| CidrParseError::BadAddress(s.to_string()))?;
                let prefix = if addr.is_ipv4() { 32 } else { 128 };
                Cidr::new(addr, prefix)
            }
        }
    }
}

/// A list of CIDR blocks, parsed once at start-up.
///
/// An empty list means "nothing is allowlisted", i.e. login for everyone. That
/// is the safe reading of `mcr_open_networks: []` and the plan's stated
/// meaning.
#[derive(Debug, Clone, Default)]
pub struct CidrSet {
    blocks: Vec<Cidr>,
}

impl CidrSet {
    /// Parse a list of CIDR strings, reporting every bad entry rather than the
    /// first, so an operator fixes one typo per restart instead of three.
    pub fn parse(entries: &[String]) -> Result<Self, Vec<(String, CidrParseError)>> {
        let mut blocks = Vec::with_capacity(entries.len());
        let mut errors = Vec::new();
        for raw in entries {
            if raw.trim().is_empty() {
                continue;
            }
            match raw.parse::<Cidr>() {
                Ok(c) => blocks.push(c),
                Err(e) => errors.push((raw.clone(), e)),
            }
        }
        if errors.is_empty() {
            Ok(Self { blocks })
        } else {
            Err(errors)
        }
    }

    /// Parse, logging and dropping bad entries instead of failing.
    ///
    /// Used on the reload path, where refusing to serve is worse than serving
    /// with a narrower allowlist: a bad entry can only ever *remove* access,
    /// because a dropped block is one fewer network allowed in.
    pub fn parse_lossy(entries: &[String]) -> Self {
        match Self::parse(entries) {
            Ok(set) => set,
            Err(errors) => {
                let mut blocks = Vec::new();
                for raw in entries {
                    if let Ok(c) = raw.trim().parse::<Cidr>() {
                        blocks.push(c);
                    }
                }
                for (raw, err) in errors {
                    tracing::warn!(
                        entry = %raw,
                        error = %err,
                        "Ignoring invalid CIDR in a network allowlist (security.mcr_open_networks, \
                         security.trusted_proxies, security.egress_allow_networks or metrics.allow_networks); that network is not allowed"
                    );
                }
                Self { blocks }
            }
        }
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.blocks.iter().any(|b| b.contains(ip))
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn blocks(&self) -> &[Cidr] {
        &self.blocks
    }
}

/// Clear the host bits below `prefix_len`.
fn mask(addr: IpAddr, prefix_len: u8) -> IpAddr {
    match addr {
        IpAddr::V4(v4) => {
            let bits = u32::from(v4);
            let masked = if prefix_len == 0 {
                0
            } else {
                bits & (!0u32 << (32 - prefix_len as u32))
            };
            IpAddr::V4(Ipv4Addr::from(masked))
        }
        IpAddr::V6(v6) => {
            let bits = u128::from(v6);
            let masked = if prefix_len == 0 {
                0
            } else {
                bits & (!0u128 << (128 - prefix_len as u32))
            };
            IpAddr::V6(Ipv6Addr::from(masked))
        }
    }
}

/// Turn an IPv4-mapped IPv6 address back into its IPv4 form.
///
/// A socket bound to `0.0.0.0` on Windows reports IPv4 peers as IPv4, but a
/// dual-stack `[::]` bind reports them as `::ffff:a.b.c.d`. Without this, the
/// default `127.0.0.1/32` allowlist would not match a loopback client on a
/// dual-stack listener.
pub fn unmap_v4(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// True for an address an outbound fetch must never reach: private, loopback,
/// link-local, CGNAT, multicast, reserved, documentation and benchmarking
/// ranges, ULA, and every IPv6 form that embeds an IPv4 address
/// (IPv4-compatible, IPv4-mapped, SIIT, NAT64 incl. local-use, 6to4, Teredo).
pub fn is_non_public(ip: IpAddr) -> bool {
    const BLOCKS: &[&str] = &[
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "224.0.0.0/4",
        "240.0.0.0/4",
        "192.0.0.0/24",
        "192.0.2.0/24",
        "198.18.0.0/15",
        "198.51.100.0/24",
        "203.0.113.0/24",
        "::/96",
        "::ffff:0:0:0/96",
        "64:ff9b:1::/48",
        "2001::/32",
        "2001:db8::/32",
        "100::/64",
        "fec0::/10",
        "::ffff:0:0/96",
        "64:ff9b::/96",
        "2002::/16",
        "fc00::/7",
        "fe80::/10",
        "ff00::/8",
    ];
    BLOCKS
        .iter()
        .filter_map(|b| b.parse::<Cidr>().ok())
        .any(|c| c.contains(ip))
}

/// True when `ip` is a public unicast address a request may go to.
pub fn is_public_ip(ip: IpAddr) -> bool {
    !is_non_public(ip)
}

/// Why an address may not be requested.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EgressError {
    #[error("the name did not resolve")]
    Unresolved,
    #[error("the name did not resolve within the time allowed")]
    Timeout,
    #[error("the name resolves to a non-public address")]
    NonPublic,
}

/// The URL half of the egress rule, shared by the recipe `Fetch` step and the
/// repair gate: http or https, no userinfo, never `localhost` (trailing dot
/// included) and never a private or reserved address literal. Returns the
/// parsed URL, which the caller must use instead of the raw text (the parser
/// and other URL readers can disagree about the host of the raw text), and the
/// lowercase host without a trailing dot. The error is short and names the host
/// only, never the path or query.
pub fn parse_public_url(raw: &str) -> Result<(reqwest::Url, String), String> {
    let url = reqwest::Url::parse(raw).map_err(|_| "url".to_string())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("scheme".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("userinfo".to_string());
    }
    let shown = url.host_str().unwrap_or("").trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if shown.is_empty() {
        return Err("host missing".to_string());
    }
    if let Ok(ip) = shown.parse::<IpAddr>() {
        if is_non_public(ip) {
            return Err(format!("host {ip} is a private address"));
        }
        return Ok((url, ip.to_string()));
    }
    let d = shown.trim_end_matches('.').to_string();
    if d == "localhost" || d.ends_with(".localhost") {
        return Err(format!("host {d} is local"));
    }
    Ok((url, d))
}

/// Resolve `host` and return its addresses, failing when resolution fails, is
/// too slow, or ANY address is not public (a name with one public and one
/// private record is still a way in).
pub async fn resolve_public(
    host: &str,
    port: u16,
    timeout: std::time::Duration,
) -> Result<Vec<std::net::SocketAddr>, EgressError> {
    let found = tokio::time::timeout(timeout, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| EgressError::Timeout)?
        .map_err(|_| EgressError::Unresolved)?;
    check_addrs(found.collect())
}

/// The answer rule of [`resolve_public`]: at least one address, and EVERY one
/// public.
pub fn check_addrs(addrs: Vec<std::net::SocketAddr>) -> Result<Vec<std::net::SocketAddr>, EgressError> {
    if addrs.is_empty() {
        return Err(EgressError::Unresolved);
    }
    if addrs.iter().any(|a| !is_public_ip(a.ip())) {
        return Err(EgressError::NonPublic);
    }
    Ok(addrs)
}

/// Why the job path may not request a URL. Every message names the host only,
/// never the path or query (an address can carry a signed token).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EgressRefusal {
    /// Not a URL the rule can judge: not http(s), user info, no host, unparsable.
    #[error("not a plain http(s) address ({0})")]
    Malformed(&'static str),
    /// The host is, or resolves to, a loopback, private, link-local or reserved address.
    #[error("{0} is an internal address")]
    NonPublic(String),
    /// The name did not resolve: unknown, not a verdict.
    #[error("{0} did not resolve")]
    Unresolved(String),
    /// The lookup took too long: unknown, not a verdict.
    #[error("{0} did not resolve in time")]
    Timeout(String),
}

impl EgressRefusal {
    /// A transient lookup failure: the caller carries on and lets the download fail as it would.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unresolved(_) | Self::Timeout(_))
    }

    /// The host the refusal is about, when there is one.
    pub fn host(&self) -> Option<&str> {
        match self {
            Self::Malformed(_) => None,
            Self::NonPublic(h) | Self::Unresolved(h) | Self::Timeout(h) => Some(h),
        }
    }
}

/// A URL the egress rule let through: the parsed, normalised form (the one string every
/// later reader must use, so the checked and the requested address cannot differ) and the
/// addresses its name resolved to (empty for an IP literal), which a request can be pinned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub url: String,
    pub addrs: Vec<std::net::SocketAddr>,
}

/// The no-lookup part of the egress rule: a plain http(s) address with no user info. A control
/// character anywhere, or a backslash or whitespace in the scheme or authority, is refused as
/// ambiguous: the URL parser reads `\` as a slash and drops control characters, other readers
/// (Python, ffmpeg) do not, so they would disagree about the host of
/// `http://public.example\@10.0.0.5/`. Past the authority (`/clip 1.mp4`) neither can move the
/// host, so a public page's unencoded path still passes. Fail closed, before parsing.
fn read_plain_url(raw: &str) -> Result<reqwest::Url, EgressRefusal> {
    let text = raw.trim();
    // Scheme and authority: up to the first `/`, `?` or `#` after `://`; all of it without `://`.
    let head = match text.find("://") {
        Some(i) => text[i + 3..].find(['/', '?', '#']).map_or(text, |j| &text[..i + 3 + j]),
        None => text,
    };
    if text.chars().any(|c| c.is_ascii_control()) || head.chars().any(|c| c == '\\' || c.is_ascii_whitespace()) {
        return Err(EgressRefusal::Malformed("ambiguous"));
    }
    let url = reqwest::Url::parse(text).map_err(|_| EgressRefusal::Malformed("url"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(EgressRefusal::Malformed("scheme"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(EgressRefusal::Malformed("userinfo"));
    }
    Ok(url)
}

/// The one egress rule of the normal job path. http or https, no user info; an
/// IP-literal host must be public or inside `allow`; a name must resolve and
/// EVERY address must be public or inside `allow`. `localhost` and
/// `*.localhost` are refused unless every address they resolve to is inside
/// `allow`. `resolve` is the name lookup (injected so tests need no DNS).
pub async fn check_public_with<R, Fut>(
    url: &str,
    allow: &CidrSet,
    timeout: std::time::Duration,
    resolve: R,
) -> Result<Checked, EgressRefusal>
where
    R: Fn(String, u16) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>>,
{
    match plan_check(read_plain_url(url)?, allow)? {
        Plan::Done(checked) => Ok(checked),
        Plan::Lookup(l) => lookup_check(l, allow, timeout, resolve).await,
    }
}

/// What the egress rule needs for a readable URL: nothing more, or a name lookup.
enum Plan {
    Done(Checked),
    Lookup(Lookup),
}

/// A name the rule still has to resolve.
struct Lookup {
    url: String,
    domain: String,
    local: bool,
    port: u16,
}

/// The part of the rule that needs no lookup: an IP literal, `localhost` without an allowlist,
/// an empty host. Everything else is a name to resolve.
fn plan_check(url: reqwest::Url, allow: &CidrSet) -> Result<Plan, EgressRefusal> {
    let ok = |ip: IpAddr| is_public_ip(ip) || allow.contains(ip);
    // The parser has already turned every numeric IPv4 spelling (0x7f.1, 2130706433) into dotted form.
    let shown = url.host_str().unwrap_or("").trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if let Ok(ip) = shown.parse::<IpAddr>() {
        return if ok(ip) {
            Ok(Plan::Done(Checked { url: url.to_string(), addrs: Vec::new() }))
        } else {
            Err(EgressRefusal::NonPublic(ip.to_string()))
        };
    }
    let domain = shown.trim_end_matches('.').to_string();
    if domain.is_empty() {
        return Err(EgressRefusal::Malformed("host"));
    }
    let local = domain == "localhost" || domain.ends_with(".localhost");
    if local && allow.is_empty() {
        return Err(EgressRefusal::NonPublic(domain));
    }
    let port = url.port_or_known_default().unwrap_or(80);
    Ok(Plan::Lookup(Lookup { url: url.to_string(), domain, local, port }))
}

async fn lookup_check<R, Fut>(l: Lookup, allow: &CidrSet, timeout: std::time::Duration, resolve: R) -> Result<Checked, EgressRefusal>
where
    R: Fn(String, u16) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>>,
{
    let Lookup { url, domain, local, port } = l;
    let ok = |ip: IpAddr| is_public_ip(ip) || allow.contains(ip);
    let found = match tokio::time::timeout(timeout, resolve(domain.clone(), port)).await {
        Err(_) => return Err(if local { EgressRefusal::NonPublic(domain) } else { EgressRefusal::Timeout(domain) }),
        Ok(Err(_)) => return Err(if local { EgressRefusal::NonPublic(domain) } else { EgressRefusal::Unresolved(domain) }),
        Ok(Ok(found)) => found,
    };
    if found.is_empty() {
        return Err(if local { EgressRefusal::NonPublic(domain) } else { EgressRefusal::Unresolved(domain) });
    }
    // A local name must land entirely inside the allowlist; any other name only
    // needs every answer public or allowed.
    let allowed = |ip: IpAddr| if local { allow.contains(ip) } else { ok(ip) };
    if found.iter().any(|a| !allowed(a.ip())) {
        return Err(EgressRefusal::NonPublic(domain));
    }
    Ok(Checked { url, addrs: found })
}

/// [`check_public_with`] using the system resolver.
pub async fn check_public(url: &str, allow: &CidrSet, timeout: std::time::Duration) -> Result<Checked, EgressRefusal> {
    check_public_with(url, allow, timeout, system_lookup).await
}

/// The system name lookup, as the resolver [`check_public_with`] takes.
pub async fn system_lookup(host: String, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
    Ok(tokio::net::lookup_host((host.as_str(), port)).await?.collect())
}

/// Lookups in flight at once when a list of candidates is screened.
const SCREEN_CONCURRENCY: usize = 8;

/// What a screen of candidate URLs found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScreenOutcome {
    /// The candidates left, in input order (as written).
    pub kept: Vec<String>,
    /// The internal hosts refused, once each. Never an unreadable address: those are counted apart.
    pub dropped: Vec<String>,
    /// How many candidates were dropped as unreadable (not a plain http(s) address, ambiguous,
    /// user info, relative).
    pub unreadable: usize,
    /// At least one candidate was refused as an internal address (not merely unreadable).
    pub any_non_public: bool,
    /// For each input, in order, the refusal that dropped it; `None` when it was kept.
    pub refusals: Vec<Option<EgressRefusal>>,
}

impl EgressRefusal {
    /// A refusal for an address several readers would read differently (a backslash, whitespace,
    /// user info): never to be handed to a reader that might pick the internal one.
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, Self::Malformed("ambiguous") | Self::Malformed("userinfo"))
    }
}

/// Keep the URLs the egress rule lets through. Each distinct (host, port) is judged once and
/// its URLs share the verdict. A verdict that needs no lookup (an IP literal, `localhost`, an
/// unreadable address) is settled at once; only real names are looked up, at most
/// [`SCREEN_CONCURRENCY`] at a time. A host whose lookup failed, timed out, or got no verdict
/// before the overall deadline (twice `timeout`) is unknown: its URLs are kept (the download
/// fails on them as it would have). An internal host's URLs and any unreadable or ambiguous
/// address (including a relative one) are dropped. A candidate starting with `//` is judged as
/// `https:` + it and kept as written.
pub async fn screen_public_with<R, Fut>(
    urls: Vec<String>,
    allow: &CidrSet,
    timeout: std::time::Duration,
    resolve: R,
) -> ScreenOutcome
where
    R: Fn(String, u16) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>> + Send + 'static,
{
    // One entry per distinct (host, port), settled now when it needs no lookup; a URL that cannot
    // be read has its refusal already.
    let mut index: std::collections::HashMap<(String, u16), usize> = std::collections::HashMap::new();
    let mut verdicts: Vec<Option<EgressRefusal>> = Vec::new();
    let mut lookups: Vec<(usize, Lookup)> = Vec::new();
    let mut slot: Vec<Result<usize, EgressRefusal>> = Vec::with_capacity(urls.len());
    for u in &urls {
        let judged = match u.trim() {
            t if t.starts_with("//") => format!("https:{t}"),
            t => t.to_string(),
        };
        slot.push(read_plain_url(&judged).map(|parsed| {
            let key = (parsed.host_str().unwrap_or("").to_ascii_lowercase(), parsed.port_or_known_default().unwrap_or(80));
            *index.entry(key).or_insert_with(|| {
                let i = verdicts.len();
                match plan_check(parsed, allow) {
                    Ok(Plan::Done(_)) => verdicts.push(None),
                    Ok(Plan::Lookup(l)) => {
                        verdicts.push(None);
                        lookups.push((i, l));
                    }
                    Err(r) => verdicts.push(Some(r)),
                }
                i
            })
        }));
    }
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(SCREEN_CONCURRENCY));
    let mut set = tokio::task::JoinSet::new();
    for (i, l) in lookups {
        let (gate, allow, resolve) = (gate.clone(), allow.clone(), resolve.clone());
        set.spawn(async move {
            let _permit = gate.acquire_owned().await;
            (i, lookup_check(l, &allow, timeout, resolve).await)
        });
    }
    let collect = async {
        while let Some(done) = set.join_next().await {
            if let Ok((i, Err(refusal))) = done {
                verdicts[i] = Some(refusal);
            }
        }
    };
    // On expiry the unfinished tasks are aborted (and their permits freed); a blocking system
    // lookup thread may still run to its end in the background.
    let _ = tokio::time::timeout(timeout * 2, collect).await;
    set.abort_all();
    let mut out = ScreenOutcome::default();
    for (u, s) in urls.into_iter().zip(slot) {
        let refusal = match s {
            Err(r) => Some(r),
            Ok(k) => verdicts[k].clone(),
        };
        match refusal {
            Some(r) if !r.is_unknown() => {
                match &r {
                    EgressRefusal::NonPublic(host) => {
                        out.any_non_public = true;
                        if !out.dropped.contains(host) {
                            out.dropped.push(host.clone());
                        }
                    }
                    _ => out.unreadable += 1,
                }
                out.refusals.push(Some(r));
            }
            _ => {
                out.kept.push(u);
                out.refusals.push(None);
            }
        }
    }
    out
}

/// [`screen_public_with`] using the system resolver.
pub async fn screen_public(urls: Vec<String>, allow: &CidrSet, timeout: std::time::Duration) -> ScreenOutcome {
    screen_public_with(urls, allow, timeout, system_lookup).await
}

/// Redirect hops [`get_public_with`] follows (reqwest's default policy limit).
const MAX_PUBLIC_HOPS: usize = 10;

/// What a [`get_public_with`] request is made with.
#[derive(Debug, Clone, Copy)]
pub struct PublicGet<'a> {
    /// The whole request, all hops and their lookups included.
    pub timeout: std::time::Duration,
    /// The name lookup of the address rule, per hop.
    pub lookup: std::time::Duration,
    pub user_agent: &'a str,
}

/// A GET under the strict rule (nothing allowlisted). The address is checked
/// before the request and again at every redirect hop, which is followed by
/// hand, and each request is pinned to the addresses that were checked
/// (`resolve_to_addrs`), so a second answer from DNS cannot send it elsewhere.
/// A refused hop, a name that did not resolve, a failed request or too many hops
/// is `None`; the caller treats it as it does any other failed lookup.
pub async fn get_public_with<R, Fut>(url: &str, how: PublicGet<'_>, resolve: R) -> Option<reqwest::Response>
where
    R: Fn(String, u16) -> Fut + Clone,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>>,
{
    get_checked_with(url, &CidrSet::default(), how, resolve).await
}

/// [`get_public_with`] with an allowlist (the same rule as [`check_public_with`]).
async fn get_checked_with<R, Fut>(url: &str, allow: &CidrSet, how: PublicGet<'_>, resolve: R) -> Option<reqwest::Response>
where
    R: Fn(String, u16) -> Fut + Clone,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>>,
{
    let hops = async {
        let mut current = reqwest::Url::parse(url).ok()?;
        for _ in 0..=MAX_PUBLIC_HOPS {
            let checked = match check_public_with(current.as_str(), allow, how.lookup, resolve.clone()).await {
                Ok(c) => c,
                Err(r) => {
                    tracing::debug!("request refused: {r}");
                    return None;
                }
            };
            let mut builder = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(how.timeout)
                .user_agent(how.user_agent);
            if !checked.addrs.is_empty() {
                if let Some(host) = current.host_str() {
                    builder = builder.resolve_to_addrs(host, &checked.addrs);
                }
            }
            let resp = builder.build().ok()?.get(current.clone()).send().await.ok()?;
            if !resp.status().is_redirection() {
                return Some(resp);
            }
            // A redirect without a usable Location is the answer itself, as with reqwest's own policy.
            let next = resp.headers().get(reqwest::header::LOCATION).and_then(|l| l.to_str().ok()).and_then(|l| current.join(l).ok());
            match next {
                Some(next) => current = next,
                None => return Some(resp),
            }
        }
        None
    };
    tokio::time::timeout(how.timeout, hops).await.ok().flatten()
}

/// [`get_public_with`] using the system resolver.
pub async fn get_public(url: &str, how: PublicGet<'_>) -> Option<reqwest::Response> {
    get_public_with(url, how, system_lookup).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_ip_range_edges() {
        // (first non-public, last non-public) per block, then the public
        // neighbours just outside.
        for a in [
            "0.0.0.0", "0.255.255.255", "10.0.0.0", "10.255.255.255", "100.64.0.0", "100.127.255.255",
            "127.0.0.0", "127.255.255.255", "169.254.0.0", "169.254.255.255", "172.16.0.0", "172.31.255.255",
            "192.168.0.0", "192.168.255.255", "224.0.0.0", "239.255.255.255", "240.0.0.0", "255.255.255.255",
            "192.0.0.0", "192.0.0.255", "192.0.2.0", "192.0.2.255", "198.18.0.0", "198.19.255.255",
            "198.51.100.0", "198.51.100.255", "203.0.113.0", "203.0.113.255",
            "fc00::", "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", "fe80::", "febf:ffff::1", "ff00::", "2001:db8::", "2001:db8:ffff::1",
        ] {
            assert!(!is_public_ip(a.parse().unwrap()), "{a} must not be public");
        }
        for a in [
            "1.0.0.0", "9.255.255.255", "11.0.0.0", "100.63.255.255", "100.128.0.0", "126.255.255.255", "128.0.0.0",
            "169.253.255.255", "169.255.0.0", "172.15.255.255", "172.32.0.0", "192.167.255.255", "192.169.0.0",
            "192.0.1.0", "192.0.3.0", "198.17.255.255", "198.20.0.0", "198.51.99.255", "198.51.101.0",
            "203.0.112.255", "203.0.114.0", "223.255.255.255", "2606:4700::1111", "2a00:1450:4001::1",
        ] {
            assert!(is_public_ip(a.parse().unwrap()), "{a} must be public");
        }
    }

    #[test]
    fn public_url_rule_rejects_local_forms() {
        for u in [
            "ftp://cdn.example.org/a.mp4", "http://user:pw@cdn.example.org/", "http://localhost/", "http://LOCALHOST./x",
            "http://a.localhost/", "http://127.0.0.1/", "http://2130706433/", "http://0x7f.1/", "http://[::1]/",
            "http://[::ffff:10.0.0.5]/", "http://10.1.2.3:8080/", "file:///C:/x", "not a url",
        ] {
            assert!(parse_public_url(u).is_err(), "{u}");
        }
        let (url, host) = parse_public_url("https://CDN.Example.org./v/a.mp4?x=1").unwrap();
        assert_eq!(host, "cdn.example.org");
        assert_eq!(url.scheme(), "https");
        assert_eq!(parse_public_url("http://93.184.216.34/a").unwrap().1, "93.184.216.34");
    }

    #[test]
    fn an_answer_is_accepted_only_when_every_address_is_public() {
        let sa = |s: &str| -> std::net::SocketAddr { s.parse().unwrap() };
        assert_eq!(check_addrs(vec![]), Err(EgressError::Unresolved));
        let all_public = vec![sa("93.184.216.34:443"), sa("[2606:4700::1111]:443")];
        assert_eq!(check_addrs(all_public.clone()), Ok(all_public));
        assert_eq!(check_addrs(vec![sa("10.0.0.1:80"), sa("[fd00::1]:80")]), Err(EgressError::NonPublic));
        assert_eq!(check_addrs(vec![sa("93.184.216.34:80"), sa("192.168.0.9:80")]), Err(EgressError::NonPublic));
        assert_eq!(check_addrs(vec![sa("127.0.0.1:80"), sa("93.184.216.34:80")]), Err(EgressError::NonPublic));
    }

    #[tokio::test]
    async fn resolve_public_refuses_a_loopback_answer() {
        // `localhost` resolves to loopback without any network.
        let r = resolve_public("localhost", 80, std::time::Duration::from_secs(5)).await;
        assert!(matches!(r, Err(EgressError::NonPublic) | Err(EgressError::Unresolved)), "{r:?}");
    }

    #[test]
    fn non_public_covers_private_and_embedded_v4_forms() {
        for a in [
            "10.0.0.5", "127.0.0.1", "169.254.1.1", "100.64.0.1", "172.16.0.1", "192.168.1.1", "224.0.0.1",
            "0.0.0.0", "::1", "::", "::127.0.0.1", "::ffff:10.0.0.5", "64:ff9b::a00:1", "2002:7f00:1::1",
            "fd00::1", "fe80::1", "ff02::1",
            "192.0.0.8", "192.0.2.1", "198.18.0.1", "198.19.255.1", "198.51.100.7", "203.0.113.9",
            "::ffff:0:0:1", "64:ff9b:1::1", "2001:0:4136:e378::1", "2001:db8::1", "100::1", "fec0::1",
        ] {
            assert!(is_non_public(a.parse().unwrap()), "{a}");
        }
        for a in ["93.184.216.34", "8.8.8.8", "2606:4700::1111"] {
            assert!(!is_non_public(a.parse().unwrap()), "{a}");
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn host_bits_are_cleared_so_a_sloppy_cidr_still_matches_its_own_subnet() {
        // Operators write the address of a machine they know, not the network
        // address. `10.20.30.40/24` must behave as `10.20.30.0/24`.
        let c: Cidr = "10.20.30.40/24".parse().unwrap();
        assert_eq!(c.to_string(), "10.20.30.0/24");
        assert!(c.contains(ip("10.20.30.1")));
        assert!(c.contains(ip("10.20.30.255")));
        assert!(!c.contains(ip("10.20.31.1")));
    }

    #[test]
    fn a_bare_address_is_a_single_host() {
        let c: Cidr = "192.168.1.50".parse().unwrap();
        assert_eq!(c.prefix_len(), 32);
        assert!(c.contains(ip("192.168.1.50")));
        assert!(!c.contains(ip("192.168.1.51")));

        let c6: Cidr = "::1".parse().unwrap();
        assert_eq!(c6.prefix_len(), 128);
        assert!(c6.contains(ip("::1")));
    }

    #[test]
    fn ipv4_mapped_ipv6_clients_match_ipv4_blocks() {
        // What a dual-stack listener reports for an IPv4 client. Without
        // unmapping, the default loopback allowlist silently fails.
        let c: Cidr = "127.0.0.1/32".parse().unwrap();
        assert!(c.contains(ip("::ffff:127.0.0.1")));

        let net: Cidr = "10.0.0.0/8".parse().unwrap();
        assert!(net.contains(ip("::ffff:10.4.5.6")));
        assert!(!net.contains(ip("::ffff:11.4.5.6")));
    }

    #[test]
    fn families_never_cross() {
        let v4: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(!v4.contains(ip("2001:db8::1")));
        let v6: Cidr = "::/0".parse().unwrap();
        assert!(!v6.contains(ip("8.8.8.8")));
    }

    #[test]
    fn a_zero_prefix_matches_the_whole_family() {
        let c: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(c.contains(ip("8.8.8.8")));
        assert!(c.contains(ip("127.0.0.1")));
    }

    #[test]
    fn out_of_range_and_malformed_prefixes_are_rejected_not_clamped() {
        // Clamping /33 to /32 would turn a typo into a working rule and hide it.
        assert!(matches!(
            "10.0.0.0/33".parse::<Cidr>(),
            Err(CidrParseError::PrefixOutOfRange { .. })
        ));
        assert!(matches!(
            "::1/129".parse::<Cidr>(),
            Err(CidrParseError::PrefixOutOfRange { .. })
        ));
        assert!(matches!(
            "10.0.0.0/abc".parse::<Cidr>(),
            Err(CidrParseError::BadPrefix(_))
        ));
        assert!(matches!(
            "not-an-ip/24".parse::<Cidr>(),
            Err(CidrParseError::BadAddress(_))
        ));
    }

    #[test]
    fn an_empty_set_matches_nothing() {
        let set = CidrSet::default();
        assert!(!set.contains(ip("127.0.0.1")));
        assert!(!set.contains(ip("10.0.0.1")));
    }

    #[test]
    fn parse_reports_every_bad_entry_not_just_the_first() {
        let entries = vec![
            "127.0.0.1/32".to_string(),
            "10.0.0.0/33".to_string(),
            "nonsense".to_string(),
        ];
        let errors = CidrSet::parse(&entries).unwrap_err();
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn parse_lossy_keeps_the_good_entries_and_drops_the_bad_ones() {
        let entries = vec!["10.0.0.0/8".to_string(), "garbage".to_string()];
        let set = CidrSet::parse_lossy(&entries);
        assert_eq!(set.len(), 1);
        assert!(set.contains(ip("10.1.2.3")));
        // The dropped entry can only narrow access, never widen it.
        assert!(!set.contains(ip("8.8.8.8")));
    }

    // ---------------------------------------------------- the job path's egress rule

    use std::net::SocketAddr;
    use std::time::Duration;

    const T: Duration = Duration::from_secs(2);

    /// A resolver that answers `ips` for every name, never touching DNS.
    fn answers(
        ips: &[&str],
    ) -> impl Fn(String, u16) -> std::future::Ready<std::io::Result<Vec<SocketAddr>>> + Clone + Send + 'static {
        let ips: Vec<IpAddr> = ips.iter().map(|s| s.parse().unwrap()).collect();
        move |_h, p| std::future::ready(Ok(ips.iter().map(|ip| SocketAddr::new(*ip, p)).collect()))
    }

    async fn verdict(url: &str, allow: &[&str], ips: &[&str]) -> Result<(), EgressRefusal> {
        let allow = CidrSet::parse_lossy(&allow.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        check_public_with(url, &allow, T, answers(ips)).await.map(|_| ())
    }

    #[tokio::test]
    async fn a_name_with_only_public_answers_passes() {
        assert_eq!(verdict("https://news.example/v.mp4", &[], &["93.184.216.34", "2606:2800:220:1::1"]).await, Ok(()));
    }

    #[tokio::test]
    async fn a_name_resolving_to_an_internal_address_is_refused() {
        for bad in ["127.0.0.1", "10.0.0.5", "169.254.169.254", "fd00::1", "::ffff:10.0.0.5"] {
            assert_eq!(
                verdict("http://cdn.example/a?token=SECRET", &[], &[bad]).await,
                Err(EgressRefusal::NonPublic("cdn.example".into())),
                "{bad}"
            );
        }
        // One public and one private record is still a way in.
        assert_eq!(
            verdict("http://cdn.example/", &[], &["93.184.216.34", "10.0.0.5"]).await,
            Err(EgressRefusal::NonPublic("cdn.example".into()))
        );
    }

    #[tokio::test]
    async fn an_internal_ip_literal_is_refused_and_the_message_has_no_path() {
        for u in ["http://127.0.0.1/", "http://10.0.0.5:8080/x", "http://[::1]/", "http://0x7f.1/", "http://2130706433/"] {
            let r = verdict(u, &[], &["93.184.216.34"]).await.unwrap_err();
            assert!(matches!(r, EgressRefusal::NonPublic(_)), "{u}: {r:?}");
        }
        let r = verdict("http://10.0.0.5/secret/path?token=SECRET", &[], &[]).await.unwrap_err();
        assert!(!r.to_string().contains("SECRET") && !r.to_string().contains("path"), "{r}");
        assert_eq!(verdict("http://93.184.216.34/v.mp4", &[], &[]).await, Ok(()));
    }

    #[tokio::test]
    async fn localhost_names_are_refused_without_a_lookup_unless_allowed() {
        // The resolver would say "public": the name alone refuses.
        for u in ["http://localhost/", "http://LOCALHOST./", "http://app.localhost/"] {
            assert!(matches!(verdict(u, &[], &["93.184.216.34"]).await, Err(EgressRefusal::NonPublic(_))), "{u}");
        }
        // Allowed only when every address it resolves to is inside the allowlist.
        assert_eq!(verdict("http://localhost/", &["127.0.0.0/8"], &["127.0.0.1"]).await, Ok(()));
        assert!(matches!(
            verdict("http://localhost/", &["127.0.0.0/8"], &["93.184.216.34"]).await,
            Err(EgressRefusal::NonPublic(_))
        ));
    }

    #[tokio::test]
    async fn a_failed_or_slow_lookup_is_unknown_not_internal() {
        let allow = CidrSet::default();
        let failing = |_h: String, _p: u16| async { Err::<Vec<SocketAddr>, _>(std::io::Error::other("no such host")) };
        let r = check_public_with("http://gone.example/", &allow, T, failing).await.unwrap_err();
        assert_eq!(r, EgressRefusal::Unresolved("gone.example".into()));
        assert!(r.is_unknown());
        let empty = |_h: String, _p: u16| async { Ok::<Vec<SocketAddr>, std::io::Error>(vec![]) };
        assert!(check_public_with("http://gone.example/", &allow, T, empty).await.unwrap_err().is_unknown());
        let slow = |_h: String, _p: u16| async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok::<Vec<SocketAddr>, std::io::Error>(vec![])
        };
        let r = check_public_with("http://slow.example/", &allow, Duration::from_millis(50), slow).await.unwrap_err();
        assert_eq!(r, EgressRefusal::Timeout("slow.example".into()));
        assert!(r.is_unknown());
    }

    #[tokio::test]
    async fn the_allowlist_admits_only_its_own_networks() {
        assert_eq!(verdict("http://10.0.0.5/", &["10.0.0.0/8"], &[]).await, Ok(()));
        assert_eq!(verdict("http://intra.example/", &["10.0.0.0/8"], &["10.1.2.3"]).await, Ok(()));
        assert_eq!(verdict("http://intra.example/", &["10.0.0.0/8"], &["::ffff:10.1.2.3"]).await, Ok(()));
        assert!(matches!(verdict("http://192.168.1.1/", &["10.0.0.0/8"], &[]).await, Err(EgressRefusal::NonPublic(_))));
        assert!(matches!(
            verdict("http://intra.example/", &["10.0.0.0/8"], &["10.1.2.3", "192.168.1.1"]).await,
            Err(EgressRefusal::NonPublic(_))
        ));
    }

    #[tokio::test]
    async fn other_schemes_and_user_info_are_refused() {
        for u in ["ftp://news.example/v", "file:///c:/x", "http://user:pw@news.example/", "http://user@93.184.216.34/", "not a url"] {
            assert!(matches!(verdict(u, &[], &["93.184.216.34"]).await, Err(EgressRefusal::Malformed(_))), "{u}");
        }
    }

    #[tokio::test]
    async fn screening_drops_internal_candidates_once_per_host_and_keeps_unknown_ones() {
        let resolve = |h: String, p: u16| async move {
            match h.as_str() {
                "cdn.example" => Ok(vec![SocketAddr::new("93.184.216.34".parse().unwrap(), p)]),
                "intra.example" => Ok(vec![SocketAddr::new("10.0.0.5".parse().unwrap(), p)]),
                _ => Err(std::io::Error::other("no such host")),
            }
        };
        let urls: Vec<String> = [
            "https://cdn.example/a.m3u8",
            "http://intra.example/b.mp4",
            "http://intra.example/c.mp4",
            "http://10.0.0.9/d.mp4",
            "https://dnsdown.example/e.mp4",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = screen_public_with(urls, &CidrSet::default(), T, resolve).await;
        assert_eq!(out.kept, vec!["https://cdn.example/a.m3u8", "https://dnsdown.example/e.mp4"]);
        assert_eq!(out.dropped, vec!["intra.example", "10.0.0.9"]);
        assert!(out.any_non_public);
        assert!(out.dropped.iter().all(|h| !h.contains('/')));
    }

    fn no_lookup() -> impl Fn(String, u16) -> std::future::Ready<std::io::Result<Vec<SocketAddr>>> + Clone + Send + 'static {
        |_h, _p| panic!("an ambiguous address must be refused before any lookup")
    }

    #[tokio::test]
    async fn an_ambiguous_address_is_refused_before_any_lookup() {
        let allow = CidrSet::default();
        for u in [
            "http://x.invalid\\@10.0.0.5/v.mp4",
            "http://x.invalid\\@10.0.0.5",
            "http:\\\\10.0.0.5/v.mp4",
            "http://cdn .example/a",
            "http://cdn.example/a\tb",
            "http://cdn.example/a\u{7f}",
        ] {
            let r = check_public_with(u, &allow, T, no_lookup()).await;
            assert_eq!(r, Err(EgressRefusal::Malformed("ambiguous")), "{u:?}");
        }
    }

    /// A space or backslash in the path cannot move the host for any reader: a public page's
    /// unencoded file name is judged by its host as before, not refused.
    #[tokio::test]
    async fn a_space_in_the_path_is_not_ambiguous() {
        for u in ["https://cdn.example/media/clip 1.mp4", "https://cdn.example/a\\b.mp4", "https://cdn.example/v?q=a b"] {
            assert_eq!(verdict(u, &[], &["93.184.216.34"]).await, Ok(()), "{u:?}");
        }
        assert_eq!(
            verdict("https://cdn.example/media/clip 1.mp4", &[], &["10.0.0.5"]).await,
            Err(EgressRefusal::NonPublic("cdn.example".into()))
        );
    }

    #[tokio::test]
    async fn screening_drops_ambiguous_and_relative_candidates_without_a_lookup() {
        let urls = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let out = screen_public_with(urls(&["http://x.invalid\\@10.0.0.5/v.mp4", "/media/v.mp4"]), &CidrSet::default(), T, no_lookup()).await;
        assert!(out.kept.is_empty() && !out.any_non_public);
        assert!(out.dropped.is_empty(), "an unreadable address is not an internal host: {:?}", out.dropped);
        assert_eq!(out.unreadable, 2);
        assert!(out.refusals[0].as_ref().unwrap().is_ambiguous());
        assert!(!out.refusals[1].as_ref().unwrap().is_ambiguous(), "a relative address is merely unreadable");
    }

    #[tokio::test]
    async fn a_literal_address_is_judged_without_waiting_for_a_lookup_permit() {
        // 20 names whose lookups never finish hold all permits until the deadline; the literal after
        // them needs no lookup and must still get its verdict.
        let slow = |_h: String, _p: u16| async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok::<Vec<SocketAddr>, std::io::Error>(vec![])
        };
        let mut urls: Vec<String> = (0..20).map(|i| format!("https://h{i}.example/v.mp4")).collect();
        urls.push("http://10.0.0.5/v.mp4".to_string());
        urls.push("http://localhost/v.mp4".to_string());
        let out = screen_public_with(urls, &CidrSet::default(), Duration::from_millis(100), slow).await;
        assert_eq!(out.dropped, vec!["10.0.0.5", "localhost"]);
        assert!(out.any_non_public);
        assert_eq!(out.kept.len(), 20);
    }

    #[tokio::test]
    async fn a_protocol_relative_candidate_is_judged_as_https_and_kept_as_written() {
        let resolve = |h: String, p: u16| async move {
            let ip = if h == "intra.example" { "10.0.0.5" } else { "93.184.216.34" };
            Ok::<_, std::io::Error>(vec![SocketAddr::new(ip.parse().unwrap(), p)])
        };
        let urls: Vec<String> = ["//cdn.example/v.m3u8", "//intra.example/v.m3u8"].iter().map(|s| s.to_string()).collect();
        let out = screen_public_with(urls, &CidrSet::default(), T, resolve).await;
        assert_eq!(out.kept, vec!["//cdn.example/v.m3u8"]);
        assert_eq!(out.dropped, vec!["intra.example"]);
        assert_eq!(out.unreadable, 0);
    }

    #[tokio::test]
    async fn screening_judges_each_host_once() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        let resolve = move |_h: String, p: u16| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(Ok::<_, std::io::Error>(vec![SocketAddr::new("93.184.216.34".parse().unwrap(), p)]))
        };
        let urls: Vec<String> = (0..30).map(|i| format!("https://{}.example/seg{i}.ts", if i % 2 == 0 { "a" } else { "b" })).collect();
        let out = screen_public_with(urls.clone(), &CidrSet::default(), T, resolve).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(out.kept, urls, "input order kept");
    }

    #[tokio::test]
    async fn a_screen_has_an_overall_deadline_and_keeps_what_has_no_verdict() {
        let slow = |_h: String, _p: u16| async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok::<Vec<SocketAddr>, std::io::Error>(vec![])
        };
        let urls: Vec<String> = (0..20).map(|i| format!("https://h{i}.example/v.mp4")).collect();
        let started = std::time::Instant::now();
        let out = screen_public_with(urls.clone(), &CidrSet::default(), Duration::from_millis(200), slow).await;
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert_eq!(out.kept, urls);
        assert!(out.dropped.is_empty());
    }

    /// Answers every connection in turn, one reply each, chosen by the request path.
    fn serve_many(l: tokio::net::TcpListener, reply: fn(&str) -> String) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                let _ = s.write_all(reply(&path).as_bytes()).await;
            }
        })
    }

    #[tokio::test]
    async fn a_redirect_without_location_is_the_answer_and_seven_hops_are_followed() {
        let allow = CidrSet::parse_lossy(&["127.0.0.2/32".to_string()]);
        let l = tokio::net::TcpListener::bind("127.0.0.2:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let server = serve_many(l, |path| match path {
            "/nolocation" => "HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
            p => match p.trim_start_matches('/').parse::<u32>() {
                Ok(n) if n < 7 => format!("HTTP/1.1 302 Found\r\nLocation: /{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", n + 1),
                _ => "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_string(),
            },
        });
        let got = get_checked_with(&format!("http://h.example:{port}/nolocation"), &allow, HOW, answers(&["127.0.0.2"])).await;
        assert_eq!(got.expect("the 304 itself").status().as_u16(), 304);
        let got = get_checked_with(&format!("http://h.example:{port}/0"), &allow, HOW, answers(&["127.0.0.2"])).await;
        assert_eq!(got.expect("seven redirects are followed").status().as_u16(), 200);
        server.abort();
    }

    #[tokio::test]
    async fn the_whole_request_is_bounded_by_one_timeout() {
        let allow = CidrSet::parse_lossy(&["127.0.0.2/32".to_string()]);
        // Accepts nothing: the request hangs.
        let l = tokio::net::TcpListener::bind("127.0.0.2:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let how = PublicGet { timeout: Duration::from_millis(300), lookup: T, user_agent: "test" };
        let started = std::time::Instant::now();
        let got = get_checked_with(&format!("http://h.example:{port}/"), &allow, how, answers(&["127.0.0.2"])).await;
        assert!(got.is_none());
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        drop(l);
    }

    const HOW: PublicGet<'static> = PublicGet { timeout: Duration::from_secs(5), lookup: T, user_agent: "test" };

    /// One reply, then the listener is done; returns what the request line said.
    fn serve_once(l: tokio::net::TcpListener, reply: String) -> tokio::task::JoinHandle<String> {
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).await.unwrap_or(0);
            s.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        })
    }

    /// The first hop is allowlisted (127.0.0.2, so no network is used) and redirects to
    /// 127.0.0.1, which is not: the hop is refused and that listener sees nothing.
    #[tokio::test]
    async fn a_redirect_hop_to_an_internal_address_is_refused_before_it_connects() {
        let first = tokio::net::TcpListener::bind("127.0.0.2:0").await.unwrap();
        let first_port = first.local_addr().unwrap().port();
        let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        second.set_nonblocking(true).unwrap();
        let second_port = second.local_addr().unwrap().port();
        let served = serve_once(
            first,
            format!("HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{second_port}/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
        );
        let allow = CidrSet::parse_lossy(&["127.0.0.2/32".to_string()]);
        let got = get_checked_with(&format!("http://first.example:{first_port}/"), &allow, HOW, answers(&["127.0.0.2"])).await;
        assert!(got.is_none(), "the hop to 127.0.0.1 is refused");
        // The first hop WAS requested, so the refusal is the hop's, not a failed first request.
        assert!(served.await.unwrap().starts_with("GET / "));
        assert_eq!(second.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock, "the second listener saw no connection");
    }

    /// The request goes to the address that was checked, not to a second answer from DNS:
    /// "rebind.example" does not resolve for the system at all, so only the pin can reach it.
    #[tokio::test]
    async fn a_request_is_pinned_to_the_checked_address() {
        let a = tokio::net::TcpListener::bind("127.0.0.2:0").await.unwrap();
        let port = a.local_addr().unwrap().port();
        let served = serve_once(a, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_string());
        let allow = CidrSet::parse_lossy(&["127.0.0.2/32".to_string()]);
        let got = get_checked_with(&format!("http://rebind.example:{port}/p"), &allow, HOW, answers(&["127.0.0.2"])).await;
        assert_eq!(got.expect("reached the pinned address").status().as_u16(), 200);
        assert!(served.await.unwrap().starts_with("GET /p "));
    }

    #[tokio::test]
    async fn get_public_refuses_an_internal_url_before_connecting() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let url = format!("http://127.0.0.1:{}/", l.local_addr().unwrap().port());
        assert!(get_public(&url, HOW).await.is_none());
        assert_eq!(l.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }
}
