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
                        "Ignoring invalid CIDR in security.mcr_open_networks; clients on that \
                         network will have to log in"
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
