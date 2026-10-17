//! The end-to-end benchmark (plan P6.4): the stored 65-link corpus, the target
//! list syntax, the thresholds a run is judged by, the comparison with the
//! previous run of the same mode, and the report built from a run's rows.
//!
//! Pure logic and data. The runner that fetches, encodes and verifies is in the
//! daemon's binary crate. Nothing here keeps a full address of a stream: a result
//! carries the host only, and a report names targets by number, domain and category.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::repository::Repository;
use crate::timestamps;

/// What a run is expected to do with a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expected {
    Pass,
    /// Known not to work (a page that holds no video): left out of every percentage.
    ExpectedFail,
}

impl Expected {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::ExpectedFail => "expected_fail",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pass" => Some(Self::Pass),
            "expected_fail" => Some(Self::ExpectedFail),
            _ => None,
        }
    }
}

/// One link of the stored corpus.
#[derive(Debug, Clone, Copy)]
pub struct CorpusRow {
    pub idx: i64,
    pub domain: &'static str,
    pub category: &'static str,
    pub url: &'static str,
    pub friction: &'static str,
    pub expected: Expected,
    pub note: &'static str,
}

const fn row(idx: i64, domain: &'static str, category: &'static str, url: &'static str, friction: &'static str) -> CorpusRow {
    CorpusRow { idx, domain, category, url, friction, expected: Expected::Pass, note: "" }
}

const fn expected_fail(
    idx: i64,
    domain: &'static str,
    category: &'static str,
    url: &'static str,
    friction: &'static str,
    note: &'static str,
) -> CorpusRow {
    CorpusRow { idx, domain, category, url, friction, expected: Expected::ExpectedFail, note }
}

/// The 65 links of the benchmark paper, in its order (YouTube, Shorts, Proto Thema,
/// Gazzetta, ERT, Iefimerida, News247, in.gr, Guardian, CNN, X).
pub const CORPUS: &[CorpusRow] = &[
    row(1, "youtube.com", "YouTube Long-Form", "https://www.youtube.com/watch?v=nYdCU1QQQro", "Multi-Stream DASH Manifest / Adaptive isolation"),
    row(2, "youtube.com", "YouTube Long-Form", "https://www.youtube.com/watch?v=h83rzGete5s", "Adaptive VP9/WebM Container / Throttling avoidance"),
    row(3, "youtube.com", "YouTube Long-Form", "https://www.youtube.com/watch?v=wadEnrHVNlE", "Dynamic VOD Stream / Chunk stitching resilience"),
    row(4, "youtube.com", "YouTube Long-Form", "https://www.youtube.com/watch?v=nUlUa2zJXh4", "Ultra-High Bitrate 4K Stream / Multi-codec resolution"),
    row(5, "youtube.com", "YouTube Long-Form", "https://www.youtube.com/watch?v=Yz7ZPBZiLBs", "Captioned OVP Delivery / Sidecar subtitle extraction"),
    row(6, "youtube.com", "YouTube Long-Form", "https://www.youtube.com/watch?v=qF1jrvjpirQ", "Broadcast News Clip / Standard client header emulation"),
    row(7, "youtube.com", "YouTube Long-Form", "https://www.youtube.com/watch?v=5_I6h7mpmvk", "Live Stream Recording / Buffer management in HLS"),
    row(8, "youtube.com", "YouTube Long-Form", "https://www.youtube.com/watch?v=mwMRAzw_UNY", "Localized Multi-Audio Stream / Audio track selection"),
    row(9, "youtube.com", "YouTube Long-Form", "https://www.youtube.com/watch?v=FWZEbZWnDv8", "Standard Dash Manifest / Extraction latency on JSON"),
    row(10, "youtube.com", "YouTube Long-Form", "https://www.youtube.com/watch?v=fWKOYAoaGJ0", "Legacy Container Endpoint / Older progressive descriptors"),
    row(11, "youtube.com", "YouTube Shorts", "https://www.youtube.com/shorts/RR3vPZExAaI", "Vertical Short-Form Feed / /shorts/ normalization"),
    row(12, "youtube.com", "YouTube Shorts", "https://www.youtube.com/shorts/fBPRNvvbs5Q", "Vertical Viewport Media / Dynamic metadata parsing"),
    row(13, "youtube.com", "YouTube Shorts", "https://www.youtube.com/shorts/Cb0_F4GgxBg", "Short-Form Stream Buffer / Rapid short-duration assets"),
    row(14, "youtube.com", "YouTube Shorts", "https://www.youtube.com/shorts/l5SlhwKlpwA", "Standard Micro-Content / Audio stream integrity"),
    row(15, "youtube.com", "YouTube Shorts (Mobile)", "https://m.youtube.com/shorts/eBORSg37h0M", "Mobile Web DOM Structure / Mobile user-agent headers"),
    row(16, "youtube.com", "YouTube Shorts (Mobile)", "https://m.youtube.com/shorts/0ckceVIloCk", "Mobile Web Viewport / Mobile endpoint redirects"),
    row(17, "youtube.com", "YouTube Shorts (Mobile)", "https://m.youtube.com/shorts/IOFYFgS412A", "High-Concurrency Short / Rate-limiting on trending"),
    row(18, "youtube.com", "YouTube Shorts", "https://www.youtube.com/shorts/Z0E0n2FAyLw", "Vertical Short-Form Video / Internal framerate & resolution"),
    row(19, "youtube.com", "YouTube Shorts", "https://www.youtube.com/shorts/v5_uwHAzbOo?vl=en-US", "Query-Parameterized Short / Query parameter stripping"),
    row(20, "youtube.com", "YouTube Shorts", "https://www.youtube.com/shorts/OWnfsm52qaA", "Standard Micro-Content / Audio normalization"),
    row(21, "protothema.gr", "Proto Thema Sports", "https://www.protothema.gr/sports/article/1877823/i-eduposiaki-apokrousi-tou-tzolaki-deite-to-video/", "Sports Highlight / Embedded Twitter/X card within ad grid"),
    row(22, "protothema.gr", "Proto Thema Domestic", "https://www.protothema.gr/greece/article/1878988/video-dokoumedo-kare-kare-i-stigmi-pou-o-drastis-vazei-fotia-sto-autokinito-tou-adidimarhou-zografou/", "Embedded Surveillance / JWPlayer iframe in dynamic scripts"),
    row(23, "protothema.gr", "Proto Thema Domestic", "https://www.protothema.gr/greece/article/1877461/agrio-xulo-metaxu-anilikon-mathitrion-sta-hania-deite-video/", "Third-Party Embedded Player / Recursive frame inspection"),
    row(24, "protothema.gr", "Proto Thema Cantina", "https://cantina.protothema.gr/viralvideos/24-vegan-fagita-pou-tha-sas-afisoun-me-to-stoma/", "Subdomain Lifestyle / Session cookies across subdomains"),
    row(25, "protothema.gr", "Proto Thema LifeStyle", "https://www.protothema.gr/life-style/article/1881236/lena-zeugara-makari-na-me-eulogisei-kapoia-stigmi-o-theos-na-kano-oikogeneia-alla-ohi-tora/", "Television Broadcast / Pre-roll ad filtering"),
    row(26, "protothema.gr", "Proto Thema Cantina", "https://cantina.protothema.gr/video/ftiachnoume-meraklidiko-elliniko-kafe-sti-chovoli/", "Dedicated Vertical / Progressive MP4 vs adaptive stream"),
    row(27, "protothema.gr", "Proto Thema Greece", "https://www.protothema.gr/greece/article/1856734/video-apo-ti-suro-i-41hroni-metamfiesmeni-sto-spiti-tou-zeugariou-tin-katadiokei-o-andras-kai-tis-petaei-karekla/", "Native Documentary / Breaking news layout under heavy ad load"),
    row(28, "protothema.gr", "Proto Thema World", "https://www.protothema.gr/world/article/1879685/video-me-ti-stigmi-tis-sudrivis-tou-elikopterou-tou-nbc-sto-los-adzeles-den-boreis-na-to-anevaseis-rotisan-ton-piloto/", "External Broadcast / Cross-origin iframe syndicated footage"),
    row(29, "protothema.gr", "Proto Thema Greece", "https://www.protothema.gr/greece/article/1880954/odigos-stin-euvoia-edese-alogo-stin-karotsa-kai-to-eserne-sto-dromo-deite-video/", "Syndicated Community / External player embedded in copy"),
    row(30, "protothema.gr", "Proto Thema LifeStyle", "https://www.protothema.gr/life-style/article/1880245/i-prigianka-tsopra-anevase-video-me-ton-nik-tzonas-na-paizei-piano-kai-tin-kori-tous-na-tragouda-to-diamonds/", "Social Media Feed / Social API wrappers in text"),
    row(31, "protothema.gr", "Proto Thema Video Hub", "https://video.protothema.gr/episode/proedros-eof-gia-mazonaki-kobogiannitis-opoios-mila-gia-kvantiki-iatriki-anevazoun-tis-times-se-amfilegomenes-therapeies-gia-na-fainontai-premium/", "Dedicated Video Hub / Enterprise video portal stream sniffing"),
    row(32, "protothema.gr", "Proto Thema English", "https://en.protothema.gr/2026/09/16/kyriakos-mitsotakis-in-parliament-on-constitutional-revision-video/", "Parliamentary Webcast / English-language mirror DOM stability"),
    row(33, "protothema.gr", "Proto Thema World", "https://www.protothema.gr/world/article/1881295/katheirxi-21-eton-se-algerino-pou-viase-vretanida-se-dromo-tou-londinou-kai-pige-na-ti-vrei-kai-sto-nosokomeio/", "Breaking World News / High-turnover world wire stories"),
    row(34, "protothema.gr", "Proto Thema World", "https://www.protothema.gr/world/article/1881231/tragodia-sto-metro-tis-neas-uorkis-duo-24hronoi-agaliazodai-kai-filoudai-prin-pesoun-stis-rages/", "International Wire / Dynamically injected video components"),
    row(35, "gazzetta.gr", "Gazzetta Sports", "https://www.gazzetta.gr/basketball/euroleague/2567028/montero-moirase-aytografa-se-filathloys-toy-olympiakoy-vid", "X (Twitter) Social Embed / pic.twitter.com widget frame"),
    row(36, "gazzetta.gr", "Gazzetta gMotion", "https://www.gazzetta.gr/gmotion/2570231/199-san-simera-oi-odigoi-tis-f1-ta-ebalan-me-tin-omihli-vid", "Archival Motorsports / Fallback routines on legacy CDNs"),
    row(37, "gazzetta.gr", "Gazzetta gMotion", "https://www.gazzetta.gr/gmotion/2570249/circuit-27-mia-pagkosmia-anasa-gia-elliniko-karting", "High-Bitrate Feature / Custom HTML5 player in responsive layout"),
    row(38, "gazzetta.gr", "Gazzetta gMotion", "https://www.gazzetta.gr/gmotion/2543774/san-simera-o-agonas-poy-o-soymaher-ekane-plaka-ston-antagonismo-vid", "Sports Broadcast / Third-party syndication players"),
    row(39, "gazzetta.gr", "Gazzetta gMotion", "https://www.gazzetta.gr/gmotion/2569120/159-san-simera-o-tragikos-thanatos-toy-kolin-makrei-vid", "Documentary Retrospective / Deeply nested editorial blocks"),
    row(40, "gazzetta.gr", "Gazzetta gMotion", "https://www.gazzetta.gr/gmotion/2570103/189-san-simera-o-agonas-poy-espase-tin-kyriarhia-tis-mercedes-vid", "Multi-Asset Video / Deduplication when multiple players load"),
    row(41, "gazzetta.gr", "Gazzetta Sports", "https://www.gazzetta.gr/basketball/euroleague/2367766/olympiakos-thymithike-proto-paihnidi-toy-printezi-stin-euroleague-vid", "Basketball Classic / Rights-restricted syndication boundaries"),
    row(42, "gazzetta.gr", "Gazzetta Football", "https://www.gazzetta.gr/football/premier-league/2033394/i-norits-anakoinose-ton-tzoli-vid", "Football News / Embed player cards in transfer bulletins"),
    row(43, "gazzetta.gr", "Gazzetta Sports", "https://www.gazzetta.gr/basketball/euroleague/2568522/olympiakos-paroxysmos-gia-mia-fotografia-me-ta-tropaia-tis-omadas-vid", "Embedded Tweet / Recursively resolve origin status streams"),
    row(44, "gazzetta.gr", "Gazzetta PLUS", "https://www.gazzetta.gr/plus/2071546/adianoitos-serbitoros-koybalaei-33-piata-me-ti-mia-vid", "Viral Social / Stream extraction from lifestyle pieces"),
    row(45, "gazzetta.gr", "Gazzetta Football", "https://www.gazzetta.gr/football/2476326/o-masoyras-ebale-gkol-prokrisis-gia-tin-al-kalitz-vid", "Match Highlight / Single play highlights on dynamically signed links"),
    row(46, "ertnews.gr", "ERT News Video", "https://www.ertnews.gr/video/amfiloxia-neo-vinteo-gia-tin-touristiki-anaptyksi-oi-kryfes-omorfies-kai-oi-monadikes-stigmes-tou-dimou/", "Native Enterprise HLS / Master playlist negotiation & ERTFLIX feed"),
    row(47, "ertnews.gr", "ERT News Video", "https://www.ertnews.gr/video/vinteo-petakste-pano-apo-to-katar-stin-plati-enos-gerakiou/", "Wildlife Feature / Cross-origin OVP manifest sniffing"),
    row(48, "ertnews.gr", "ERT News Regional", "https://www.ertnews.gr/perifereiakoi-stathmoi/volos/to-vinteo-foititon-toy-panepistimioy-thessalias-enantia-se-kathe-morfi-vias-kai-katapiesis-video/", "Regional Station / Resolve media across station taxonomy pathways"),
    row(49, "ertnews.gr", "ERT News Wire", "https://www.ertnews.gr/eidiseis/nigiria-enoploi-dimosiopoiisan-vinteo-me-ekatontades-gynaikes-ilikiomenous-kai-paidia-pou-ferontai-na-exoun-apaxthei/", "Wire Agency / Dynamic video assets embedded within wire copy"),
    row(50, "ertnews.gr", "ERT News CCTV", "https://www.ertnews.gr/roi-idiseon/thessaloniki-vinteo-ntokoymento-deichnei-ti-drasi-thrasytaton-diarrikton-sti-thermi/", "Investigative CCTV / Asset discovery within updating streams"),
    row(51, "ertnews.gr", "ERT News Culture", "https://www.ertnews.gr/video/entyposiase-to-soou-me-drones-ston-ourano-tis-thessalonikis-foto-vinteo/", "Culture Feature / Isolate video components from photo arrays"),
    row(52, "iefimerida.gr", "Iefimerida Kosmos", "https://www.iefimerida.gr/kosmos/deite-binteo-apokardiotikes-eikones-logo-xirasias", "Lazy-Hydrated / Force Glomex script execution via lazy scroll"),
    row(53, "iefimerida.gr", "Iefimerida Media", "https://www.iefimerida.gr/media/deite-binteo-gia-ta-15-hronia-stin-epohi-ton-fake-news-empisteysoy-tin-iefimerida", "Native Campaign / Direct progressive MP4 from newspaper CDN"),
    row(54, "iefimerida.gr", "Iefimerida Politiki", "https://www.iefimerida.gr/politiki/idiaitero-teletoyrgiko-gia-tin-anakoinosi-toy-onomatos-toy-kommatos-tsipra", "Political Event / Error-state fallbacks on custom players"),
    row(55, "iefimerida.gr", "Iefimerida Ellada", "https://www.iefimerida.gr/ellada/i-stigmi-toy-seismoy-stin-ekpompi-morning-point", "Breaking News / Fast asset identification in crisis bulletins"),
    row(56, "iefimerida.gr", "Iefimerida Auto", "https://www.iefimerida.gr/aytokinito/me-drone-pianontai-oi-parabates-stin-toyrkia?amp", "AMP Automotive / Adapt scraping strategy to AMP-specific markup"),
    row(57, "news247.gr", "News247 Listicle", "https://www.news247.gr/viral/youtube-10-viral-ellinika-vinteo-pou-mas-exoun-stoixeiosei/", "Multi-Video Aggregation / Stress-test extraction against 10+ embeds"),
    row(58, "news247.gr", "News247 Video", "https://www.news247.gr/videos/ti-krivetai-piso-apo-ta-sklira-vinteo-ton-neon/", "Editorial Production / Dedicated /videos/ taxonomy and players"),
    row(59, "news247.gr", "News247 Ellada", "https://www.news247.gr/ellada/martino-vinteo-ntokoumento-apo-tin-anatinaxi-atm-me-leia-ano-ton-43-000-evro/", "Glomex Syndication / Sandboxed iframes capturing dynamic manifest"),
    row(60, "in.gr", "In.gr English", "https://www.in.gr/2025/05/13/english-edition/a-trumpet-for-life/", "Custom Frame / Plymouth/Plyr embedded HLS manifest on MSVDN"),
    expected_fail(61, "theguardian.com", "The Guardian Hub", "https://www.theguardian.com/world/world+content/video", "Consent Wall / CMP acceptance event required before player hydration", "hub page, needs a real article URL"),
    row(62, "theguardian.com", "The Guardian Archive", "https://www.theguardian.com/world/greek-election-blog-2012/video/2012/jun/15/greece-hospital-austerity-cuts-video", "Archival Brightcove / Legacy streaming fallback parsing"),
    row(63, "arabic.cnn.com", "CNN Arabic", "https://arabic.cnn.com/videos", "RTL Enterprise / Akamai Signed HLS & HMAC token expiration"),
    row(64, "cnn.gr", "CNN Greece", "https://www.cnn.gr/kosmos/story/553062/saoudiki-aravia-floges-kai-kapnos-ypsonontai-konta-sto-aerodromio-tou-riant", "Regional Affiliate / VAST/VPAID promotional ad stripping"),
    row(65, "x.com", "X Native Platform", "https://x.com/gazzetta_gr/status/1834190772244975765", "Origin Status Direct / Dynamic guest access token and GraphQL HLS"),
];

/// What a run goes as far as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Find the video only: nothing is downloaded.
    Extract,
    /// Extract, download, transcode, rewrap and verify, in a scratch directory.
    Full,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Extract => "extract",
            Self::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "extract" => Some(Self::Extract),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// `1-65`, `1-5,9`, `7`: the targets a run covers, sorted, each once. `max` is
/// the highest number that exists. Anything else is an error that says why.
pub fn parse_targets(spec: &str, max: i64) -> std::result::Result<Vec<i64>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("Name the targets, for example 1-65 or 1-5,9.".to_string());
    }
    let number = |s: &str| -> std::result::Result<i64, String> {
        let s = s.trim();
        if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) || s.len() > 6 {
            return Err(format!("'{s}' is not a target number."));
        }
        let n: i64 = s.parse().map_err(|_| format!("'{s}' is not a target number."))?;
        if n < 1 || n > max {
            return Err(format!("Target {n} does not exist; the corpus has 1-{max}."));
        }
        Ok(n)
    };
    let mut out: Vec<i64> = Vec::new();
    for part in spec.split(',') {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (number(a)?, number(b)?);
                if a > b {
                    return Err(format!("'{}' runs backwards.", part.trim()));
                }
                out.extend(a..=b);
            }
            None => out.push(number(part)?),
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// The host of an address, lowercase, with no scheme, port, path or query. A result
/// keeps this and nothing more of a stream's address: the rest can carry a token.
pub fn stream_host(url: &str) -> Option<String> {
    let full = if url.starts_with("//") { format!("https:{url}") } else { url.to_string() };
    let parsed = reqwest::Url::parse(&full).ok()?;
    parsed.host_str().map(|h| h.to_ascii_lowercase())
}

// ------------------------------------------------------------------ stored rows

/// One row of `benchmark_targets`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRow {
    pub id: i64,
    pub idx: i64,
    pub url: String,
    pub domain: String,
    pub category: String,
    pub friction: String,
    pub expected: Expected,
    pub note: String,
}

/// One row of `benchmark_runs`.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    /// Waiting for a running job to finish: on-air work comes first.
    pub paused: bool,
    pub mode: Mode,
    pub state: String,
    pub params_json: String,
    pub summary_json: Option<String>,
    /// `daemon` (started from the admin card) or `cli`.
    pub owner: String,
}

/// One row of `benchmark_results`: what a run found for one target. A stage that
/// did not run is `None`, never zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultRow {
    pub run_id: i64,
    pub target_id: i64,
    pub extract_ok: Option<bool>,
    pub extract_ms: Option<i64>,
    pub method: Option<String>,
    pub candidates: Option<i64>,
    pub download_ok: Option<bool>,
    pub download_ms: Option<i64>,
    pub transcode_ms: Option<i64>,
    pub verify_ok: Option<bool>,
    pub total_ms: Option<i64>,
    /// An `ErrorCode` string: never a message, which could quote an address.
    pub error_code: Option<String>,
    /// The host of the stream found, never the address.
    pub stream_host: Option<String>,
}

impl ResultRow {
    /// Found, downloaded, and passed the compliance check: what the pipeline
    /// would have delivered.
    pub fn delivery_equivalent(&self) -> bool {
        self.extract_ok == Some(true) && self.download_ok == Some(true) && self.verify_ok == Some(true)
    }

    /// Found, but longer than a benchmark transcodes. Production has no length cap, so this says
    /// nothing about delivery: the target is left out of the delivery rate and of the failures.
    pub fn skipped(&self) -> bool {
        self.error_code.as_deref() == Some(SKIPPED_CODE)
    }
}

// ------------------------------------------------------------------ thresholds

/// The error code of a target skipped for its length.
pub const SKIPPED_CODE: &str = "SOURCE_TOO_LONG";

/// Extraction must succeed on at least this share of the targets not expected to fail.
pub const EXTRACT_MIN_PCT: f64 = 95.0;
/// A full run must deliver (in effect) at least this share of them.
pub const DELIVERY_MIN_PCT: f64 = 90.0;
/// Failed compliance checks allowed in a full run.
pub const MAX_COMPLIANCE_FAILS: usize = 0;
/// The mean time to find a video, at most.
pub const MEAN_EXTRACT_MAX_MS: f64 = 12_000.0;

pub fn extract_rate_passes(pct: f64) -> bool {
    pct >= EXTRACT_MIN_PCT
}

pub fn delivery_rate_passes(pct: f64) -> bool {
    pct >= DELIVERY_MIN_PCT
}

pub fn mean_extract_passes(ms: f64) -> bool {
    ms <= MEAN_EXTRACT_MAX_MS
}

/// One threshold and how the run stands against it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    /// What the run measured; `None` when nothing could be measured.
    pub value: Option<f64>,
    /// `>=` or `<=`, the way `value` is compared with `limit`.
    pub op: String,
    pub limit: f64,
    pub unit: String,
    /// `None`: not applicable to this run (an extract-only run has no compliance check).
    pub pass: Option<bool>,
    /// Why, when the number alone does not say.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainRow {
    pub domain: String,
    pub targets: usize,
    pub extract_ok: usize,
    pub delivery_ok: usize,
    pub mean_extract_ms: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StageTimings {
    pub extract_mean_ms: Option<f64>,
    pub download_mean_ms: Option<f64>,
    pub transcode_mean_ms: Option<f64>,
    pub total_mean_ms: Option<f64>,
}

/// The numbers of a run; stored with it and compared with the next.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub mode: Mode,
    pub targets_run: usize,
    /// Targets run that are not expected to fail: the base of every percentage.
    pub counted: usize,
    pub extract_ok: usize,
    pub extract_pct: Option<f64>,
    pub delivery_ok: Option<usize>,
    pub delivery_pct: Option<f64>,
    pub compliance_fails: Option<usize>,
    pub mean_extract_ms: Option<f64>,
    pub timings: StageTimings,
    pub checks: Vec<Check>,
    /// Every applicable threshold passed; `None` when there was nothing to judge.
    pub pass: Option<bool>,
    pub by_domain: Vec<DomainRow>,
    /// The targets run, and those of them that failed (not expected to).
    pub ran: Vec<i64>,
    pub failed: Vec<i64>,
    /// Targets not carried through because the source is too long: not failures.
    #[serde(default)]
    pub skipped: Vec<i64>,
    /// Targets expected to fail that did not.
    pub surprise_passes: Vec<i64>,
}

fn mean(values: impl Iterator<Item = i64>) -> Option<f64> {
    let (sum, n) = values.fold((0i64, 0usize), |(s, n), v| (s + v, n + 1));
    (n > 0).then(|| sum as f64 / n as f64)
}

fn pct(ok: usize, of: usize) -> Option<f64> {
    // Multiply first: 19 of 20 is exactly 95.0, not 94.99999999999999.
    (of > 0).then(|| ok as f64 * 100.0 / of as f64)
}

/// The summary of the results so far, one `(target, result)` pair per target run.
pub fn summarize(mode: Mode, rows: &[(TargetRow, ResultRow)]) -> Summary {
    let full = mode == Mode::Full;
    let counted: Vec<&(TargetRow, ResultRow)> = rows.iter().filter(|(t, _)| t.expected == Expected::Pass).collect();
    let extract_ok = counted.iter().filter(|(_, r)| r.extract_ok == Some(true)).count();
    let extract_pct = pct(extract_ok, counted.len());
    let delivery_ok = counted.iter().filter(|(_, r)| r.delivery_equivalent()).count();
    // A source too long to transcode is not part of what delivery is judged on.
    let delivery_base = counted.iter().filter(|(_, r)| !r.skipped()).count();
    let delivery_pct = pct(delivery_ok, delivery_base);
    // A file that failed the check is a failure wherever it came from.
    let compliance_fails = rows.iter().filter(|(_, r)| r.verify_ok == Some(false)).count();
    let mean_extract_ms = mean(counted.iter().filter_map(|(_, r)| r.extract_ms));

    let checks = vec![
        Check {
            name: "Extraction success".into(),
            value: extract_pct,
            op: ">=".into(),
            limit: EXTRACT_MIN_PCT,
            unit: "%".into(),
            pass: extract_pct.map(extract_rate_passes),
            note: None,
        },
        Check {
            name: "Delivery-equivalent success".into(),
            value: delivery_pct.filter(|_| full),
            op: ">=".into(),
            limit: DELIVERY_MIN_PCT,
            unit: "%".into(),
            // A full run that delivered nothing (all skipped, or no target counted) is not a pass.
            pass: if !full {
                None
            } else if delivery_base == 0 {
                Some(false)
            } else {
                delivery_pct.map(delivery_rate_passes)
            },
            note: (full && delivery_base == 0).then(|| "nothing was delivered".to_string()),
        },
        Check {
            name: "Compliance failures".into(),
            value: full.then_some(compliance_fails as f64),
            op: "<=".into(),
            limit: MAX_COMPLIANCE_FAILS as f64,
            unit: "".into(),
            pass: full.then_some(compliance_fails <= MAX_COMPLIANCE_FAILS),
            note: None,
        },
        Check {
            name: "Mean extraction time".into(),
            value: mean_extract_ms.map(|ms| ms / 1000.0),
            op: "<=".into(),
            limit: MEAN_EXTRACT_MAX_MS / 1000.0,
            unit: "s".into(),
            pass: mean_extract_ms.map(mean_extract_passes),
            note: None,
        },
    ];
    let judged: Vec<bool> = checks.iter().filter_map(|c| c.pass).collect();
    let pass = (!judged.is_empty()).then(|| judged.iter().all(|p| *p));

    let mut domains: std::collections::BTreeMap<String, Vec<&(TargetRow, ResultRow)>> = std::collections::BTreeMap::new();
    for pair in rows {
        domains.entry(pair.0.domain.clone()).or_default().push(pair);
    }
    let by_domain = domains
        .into_iter()
        .map(|(domain, rs)| DomainRow {
            domain,
            targets: rs.len(),
            extract_ok: rs.iter().filter(|(_, r)| r.extract_ok == Some(true)).count(),
            delivery_ok: rs.iter().filter(|(_, r)| r.delivery_equivalent()).count(),
            mean_extract_ms: mean(rs.iter().filter_map(|(_, r)| r.extract_ms)),
        })
        .collect();

    let reached = |r: &ResultRow| if full { r.delivery_equivalent() } else { r.extract_ok == Some(true) };
    let mut ran: Vec<i64> = rows.iter().map(|(t, _)| t.idx).collect();
    ran.sort_unstable();
    let mut failed: Vec<i64> =
        rows.iter().filter(|(t, r)| t.expected == Expected::Pass && !r.skipped() && !reached(r)).map(|(t, _)| t.idx).collect();
    failed.sort_unstable();
    let mut skipped: Vec<i64> = rows.iter().filter(|(_, r)| r.skipped()).map(|(t, _)| t.idx).collect();
    skipped.sort_unstable();
    let mut surprise_passes: Vec<i64> =
        rows.iter().filter(|(t, r)| t.expected == Expected::ExpectedFail && reached(r)).map(|(t, _)| t.idx).collect();
    surprise_passes.sort_unstable();

    Summary {
        mode,
        targets_run: rows.len(),
        counted: counted.len(),
        extract_ok,
        extract_pct,
        delivery_ok: full.then_some(delivery_ok),
        delivery_pct: delivery_pct.filter(|_| full),
        compliance_fails: full.then_some(compliance_fails),
        mean_extract_ms,
        timings: StageTimings {
            extract_mean_ms: mean(rows.iter().filter_map(|(_, r)| r.extract_ms)),
            download_mean_ms: mean(rows.iter().filter_map(|(_, r)| r.download_ms)),
            transcode_mean_ms: mean(rows.iter().filter_map(|(_, r)| r.transcode_ms)),
            total_mean_ms: mean(rows.iter().filter_map(|(_, r)| r.total_ms)),
        },
        checks,
        pass,
        by_domain,
        ran,
        failed,
        skipped,
        surprise_passes,
    }
}

// ------------------------------------------------------------------ deltas

/// How a run differs from the previous finished run of the same mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Deltas {
    pub previous_run_id: i64,
    pub extract_pct: Option<f64>,
    pub delivery_pct: Option<f64>,
    pub mean_extract_ms: Option<f64>,
    pub compliance_fails: Option<i64>,
    /// Targets both runs covered that now fail and did not.
    pub regressions: Vec<i64>,
    /// Targets both runs covered that now work and did not.
    pub fixes: Vec<i64>,
}

fn diff(now: Option<f64>, before: Option<f64>) -> Option<f64> {
    Some(now? - before?)
}

/// The change from `previous` to `current`. Only runs of the same mode are
/// comparable (a full run's timings include work an extract run never does):
/// `None` for any other pair.
pub fn deltas(current: &Summary, previous: &Summary, previous_run_id: i64) -> Option<Deltas> {
    if current.mode != previous.mode {
        return None;
    }
    // A target skipped in either run says nothing about a change.
    let in_both = |idx: &i64| {
        previous.ran.contains(idx)
            && current.ran.contains(idx)
            && !previous.skipped.contains(idx)
            && !current.skipped.contains(idx)
    };
    Some(Deltas {
        previous_run_id,
        extract_pct: diff(current.extract_pct, previous.extract_pct),
        delivery_pct: diff(current.delivery_pct, previous.delivery_pct),
        mean_extract_ms: diff(current.mean_extract_ms, previous.mean_extract_ms),
        compliance_fails: match (current.compliance_fails, previous.compliance_fails) {
            (Some(a), Some(b)) => Some(a as i64 - b as i64),
            _ => None,
        },
        regressions: current.failed.iter().filter(|i| in_both(i) && !previous.failed.contains(i)).copied().collect(),
        fixes: previous.failed.iter().filter(|i| in_both(i) && !current.failed.contains(i)).copied().collect(),
    })
}

// ------------------------------------------------------------------ report

#[derive(Debug, Clone, Serialize)]
pub struct RunInfo {
    pub id: i64,
    pub mode: Mode,
    pub state: String,
    /// `None` is shown blank, never as the current time.
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// Waiting for a running job to finish.
    pub paused: bool,
    pub params: serde_json::Value,
}

/// One line of the per-target table.
#[derive(Debug, Clone, Serialize)]
pub struct TargetLine {
    pub idx: i64,
    pub domain: String,
    pub category: String,
    pub expected: Expected,
    pub note: String,
    /// Not carried through: the source is longer than a benchmark transcodes.
    pub skipped: bool,
    pub extract_ok: Option<bool>,
    pub extract_ms: Option<i64>,
    pub method: Option<String>,
    pub candidates: Option<i64>,
    pub download_ok: Option<bool>,
    pub download_ms: Option<i64>,
    pub transcode_ms: Option<i64>,
    pub verify_ok: Option<bool>,
    pub total_ms: Option<i64>,
    pub error_code: Option<String>,
    pub stream_host: Option<String>,
}

/// Everything a run's report shows. Built from the run's rows, so the daemon, the
/// CLI and the admin card all read the same thing.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub run: RunInfo,
    pub summary: Summary,
    pub deltas: Option<Deltas>,
    pub targets: Vec<TargetLine>,
}

/// The report of run `run_id`; `None` when there is no such run.
pub fn build_report(repo: &Repository, run_id: i64) -> Result<Option<Report>> {
    let Some(run) = repo.benchmark_run(run_id)? else { return Ok(None) };
    let targets = repo.benchmark_targets()?;
    let mut rows: Vec<(TargetRow, ResultRow)> = repo
        .benchmark_results(run_id)?
        .into_iter()
        .filter_map(|r| targets.iter().find(|t| t.id == r.target_id).cloned().map(|t| (t, r)))
        .collect();
    rows.sort_by_key(|(t, _)| t.idx);
    let summary = summarize(run.mode, &rows);
    let previous = repo.previous_benchmark_run(run.mode, run.id)?;
    let deltas = previous.and_then(|p| {
        let before: Summary = serde_json::from_str(p.summary_json.as_deref()?).ok()?;
        self::deltas(&summary, &before, p.id)
    });
    let lines = rows
        .into_iter()
        .map(|(t, r)| TargetLine {
            idx: t.idx,
            domain: t.domain,
            category: t.category,
            expected: t.expected,
            note: t.note,
            skipped: r.skipped(),
            extract_ok: r.extract_ok,
            extract_ms: r.extract_ms,
            method: r.method,
            candidates: r.candidates,
            download_ok: r.download_ok,
            download_ms: r.download_ms,
            transcode_ms: r.transcode_ms,
            verify_ok: r.verify_ok,
            total_ms: r.total_ms,
            error_code: r.error_code,
            stream_host: r.stream_host,
        })
        .collect();
    Ok(Some(Report {
        run: RunInfo {
            id: run.id,
            mode: run.mode,
            state: run.state,
            started_at: run.started_at.map(timestamps::format),
            finished_at: run.finished_at.map(timestamps::format),
            paused: run.paused,
            params: serde_json::from_str(&run.params_json).unwrap_or(serde_json::Value::Null),
        },
        summary,
        deltas,
        targets: lines,
    }))
}

fn cell(s: &str) -> String {
    s.replace('|', "/").replace(['\r', '\n'], " ")
}

fn yes_no(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "yes",
        Some(false) => "NO",
        None => "",
    }
}

fn secs_i(v: Option<i64>) -> String {
    v.map(|m| format!("{:.1}", m as f64 / 1000.0)).unwrap_or_default()
}

fn secs_f(v: Option<f64>) -> String {
    v.map(|m| format!("{:.1}", m / 1000.0)).unwrap_or_default()
}

fn signed(v: Option<f64>, unit: &str) -> String {
    match v {
        Some(d) => format!("{d:+.1}{unit}"),
        None => "n/a".to_string(),
    }
}

fn idx_list(v: &[i64]) -> String {
    if v.is_empty() {
        "none".to_string()
    } else {
        v.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(", ")
    }
}

/// The report as Markdown, for `report.md`.
pub fn markdown(report: &Report) -> String {
    use std::fmt::Write;
    let r = &report.run;
    let s = &report.summary;
    let mut o = String::new();
    let _ = writeln!(o, "# Benchmark run {} ({})\n", r.id, r.mode.as_str());
    let _ = writeln!(
        o,
        "State: {}. Started: {}. Finished: {}. Targets run: {} ({} counted, the rest expected to fail).\n",
        r.state,
        r.started_at.as_deref().unwrap_or(""),
        r.finished_at.as_deref().unwrap_or(""),
        s.targets_run,
        s.counted
    );
    let verdict = match s.pass {
        Some(true) => "PASS",
        Some(false) => "FAIL",
        None => "n/a",
    };
    let _ = writeln!(o, "## Thresholds: {verdict}\n");
    let _ = writeln!(o, "| Threshold | Limit | Measured | Result |\n|---|---|---|---|");
    for c in &s.checks {
        let measured = c.value.map(|v| format!("{v:.1}{}", c.unit)).unwrap_or_default();
        let result = match c.pass {
            Some(true) => "PASS",
            Some(false) => "FAIL",
            None => "n/a",
        };
        let note = c.note.as_deref().map(|n| format!(" ({n})")).unwrap_or_default();
        let _ = writeln!(o, "| {} | {} {:.1}{} | {} | {}{} |", c.name, c.op, c.limit, c.unit, measured, result, note);
    }
    let _ = writeln!(o, "\n## Against the previous {} run\n", r.mode.as_str());
    match &report.deltas {
        Some(d) => {
            let _ = writeln!(
                o,
                "Previous run: {}. Extraction {}. Delivery {}. Mean extraction {}. Compliance failures {}.\n",
                d.previous_run_id,
                signed(d.extract_pct, " points"),
                signed(d.delivery_pct, " points"),
                signed(d.mean_extract_ms.map(|m| m / 1000.0), " s"),
                d.compliance_fails.map(|c| format!("{c:+}")).unwrap_or_else(|| "n/a".to_string()),
            );
            let _ = writeln!(o, "Newly failing: {}. Newly working: {}.\n", idx_list(&d.regressions), idx_list(&d.fixes));
        }
        None => {
            let _ = writeln!(o, "No earlier finished {} run to compare with.\n", r.mode.as_str());
        }
    }
    let t = &s.timings;
    let _ = writeln!(o, "## Stage timings (mean, seconds)\n");
    let _ = writeln!(o, "| Extract | Download | Transcode + rewrap | Total per target |\n|---|---|---|---|");
    let _ = writeln!(
        o,
        "| {} | {} | {} | {} |\n",
        secs_f(t.extract_mean_ms),
        secs_f(t.download_mean_ms),
        secs_f(t.transcode_mean_ms),
        secs_f(t.total_mean_ms)
    );
    let _ = writeln!(o, "## By domain\n");
    let _ = writeln!(o, "| Domain | Targets | Extracted | Delivery-equivalent | Mean extract (s) |\n|---|---|---|---|---|");
    for d in &s.by_domain {
        let _ = writeln!(o, "| {} | {} | {} | {} | {} |", cell(&d.domain), d.targets, d.extract_ok, d.delivery_ok, secs_f(d.mean_extract_ms));
    }
    let _ = writeln!(o, "\n## By target\n");
    let _ = writeln!(
        o,
        "| # | Domain | Category | Expected | Extract | s | Method | Streams | Download | s | Transcode s | Verify | Total s | Error | Stream host |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
    );
    for l in &report.targets {
        let _ = writeln!(
            o,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            l.idx,
            cell(&l.domain),
            cell(&l.category),
            l.expected.as_str(),
            yes_no(l.extract_ok),
            secs_i(l.extract_ms),
            cell(l.method.as_deref().unwrap_or("")),
            l.candidates.map(|c| c.to_string()).unwrap_or_default(),
            yes_no(l.download_ok),
            secs_i(l.download_ms),
            secs_i(l.transcode_ms),
            yes_no(l.verify_ok),
            secs_i(l.total_ms),
            if l.skipped { "SOURCE_TOO_LONG (skipped)".to_string() } else { cell(l.error_code.as_deref().unwrap_or("")) },
            cell(l.stream_host.as_deref().unwrap_or("")),
        );
    }
    if !s.skipped.is_empty() {
        let _ = writeln!(o, "\nSkipped, the source is over ten minutes (not counted in the delivery rate): {}.", idx_list(&s.skipped));
    }
    if !s.surprise_passes.is_empty() {
        let _ = writeln!(o, "\nExpected to fail but did not: {}.", idx_list(&s.surprise_passes));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(idx: i64, expected: Expected) -> TargetRow {
        TargetRow {
            id: idx,
            idx,
            url: String::new(),
            domain: "news.example".into(),
            category: "Test".into(),
            friction: String::new(),
            expected,
            note: String::new(),
        }
    }

    fn found(idx: i64, ms: i64) -> (TargetRow, ResultRow) {
        let r = ResultRow { target_id: idx, extract_ok: Some(true), extract_ms: Some(ms), ..Default::default() };
        (target(idx, Expected::Pass), r)
    }

    fn missed(idx: i64, ms: i64) -> (TargetRow, ResultRow) {
        let r = ResultRow { target_id: idx, extract_ok: Some(false), extract_ms: Some(ms), ..Default::default() };
        (target(idx, Expected::Pass), r)
    }

    #[test]
    fn the_corpus_has_65_rows_in_order_and_61_is_the_hub_page() {
        assert_eq!(CORPUS.len(), 65);
        for (i, row) in CORPUS.iter().enumerate() {
            assert_eq!(row.idx, i as i64 + 1);
            assert!(row.url.starts_with("https://"), "#{} has no address", row.idx);
        }
        let fails: Vec<i64> = CORPUS.iter().filter(|r| r.expected == Expected::ExpectedFail).map(|r| r.idx).collect();
        assert_eq!(fails, vec![61], "only the Guardian hub page is expected to fail");
        assert!(CORPUS[60].note.contains("hub page"));
    }

    #[test]
    fn target_lists_parse_and_bad_ones_are_errors() {
        assert_eq!(parse_targets("1-65", 65).unwrap().len(), 65);
        assert_eq!(parse_targets("1-5,9", 65).unwrap(), vec![1, 2, 3, 4, 5, 9]);
        assert_eq!(parse_targets(" 9 , 3-4,4 ", 65).unwrap(), vec![3, 4, 9], "sorted, each once");
        for bad in ["", "  ", "0", "66", "1-66", "5-3", "a", "1-", "-3", "1,,2", "1-2-3", "1;2", "1.5"] {
            assert!(parse_targets(bad, 65).is_err(), "'{bad}' must be refused");
        }
    }

    #[test]
    fn extraction_threshold_boundary_is_95_percent() {
        assert!(!extract_rate_passes(94.9));
        assert!(extract_rate_passes(95.0));
        // Through the real arithmetic: 19 of 20 is 95 %, 18 of 20 is 90 %.
        let nineteen: Vec<_> = (1..=19).map(|i| found(i, 1000)).chain([missed(20, 1000)]).collect();
        assert_eq!(summarize(Mode::Extract, &nineteen).checks[0].pass, Some(true));
        let eighteen: Vec<_> = (1..=18).map(|i| found(i, 1000)).chain([missed(19, 1000), missed(20, 1000)]).collect();
        assert_eq!(summarize(Mode::Extract, &eighteen).checks[0].pass, Some(false));
    }

    #[test]
    fn mean_extraction_threshold_boundary_is_12_seconds() {
        assert!(mean_extract_passes(12_000.0));
        assert!(!mean_extract_passes(12_100.0));
        let slow = [found(1, 12_100)];
        let s = summarize(Mode::Extract, &slow);
        assert_eq!(s.checks[3].pass, Some(false));
        let ok = [found(1, 12_000)];
        assert_eq!(summarize(Mode::Extract, &ok).checks[3].pass, Some(true));
    }

    #[test]
    fn delivery_and_compliance_thresholds_apply_to_full_runs_only() {
        let delivered = |idx: i64| {
            let r = ResultRow {
                target_id: idx,
                extract_ok: Some(true),
                extract_ms: Some(1000),
                download_ok: Some(true),
                verify_ok: Some(true),
                ..Default::default()
            };
            (target(idx, Expected::Pass), r)
        };
        let mut rows: Vec<_> = (1..=9).map(delivered).collect();
        rows.push(found(10, 1000)); // found, never downloaded
        let full = summarize(Mode::Full, &rows);
        assert_eq!(full.delivery_pct, Some(90.0));
        assert_eq!(full.checks[1].pass, Some(true));
        assert_eq!(full.checks[2].pass, Some(true), "no compliance failure");
        // One failed compliance check fails the run whatever the percentages.
        rows[0].1.verify_ok = Some(false);
        let full = summarize(Mode::Full, &rows);
        assert_eq!(full.compliance_fails, Some(1));
        assert_eq!(full.checks[2].pass, Some(false));
        assert_eq!(full.pass, Some(false));
        // An extract-only run has nothing to say about either.
        let ex = summarize(Mode::Extract, &rows);
        assert_eq!(ex.checks[1].pass, None);
        assert_eq!(ex.checks[2].pass, None);
    }

    #[test]
    fn a_full_run_that_delivered_nothing_does_not_pass() {
        // Every counted target skipped (too long): nothing was delivered, so it cannot pass.
        let skipped = |idx: i64| {
            let r = ResultRow {
                target_id: idx,
                extract_ok: Some(true),
                extract_ms: Some(1000),
                download_ok: Some(true),
                error_code: Some(SKIPPED_CODE.to_string()),
                ..Default::default()
            };
            (target(idx, Expected::Pass), r)
        };
        let rows = [skipped(1), skipped(2)];
        let s = summarize(Mode::Full, &rows);
        assert_eq!(s.skipped, vec![1, 2]);
        assert_eq!(s.checks[1].pass, Some(false));
        assert_eq!(s.checks[1].note.as_deref(), Some("nothing was delivered"));
        assert_eq!(s.pass, Some(false));
        // Extraction alone has no delivery to speak of.
        assert_eq!(summarize(Mode::Extract, &rows).checks[1].pass, None);
        // The control: one delivered target next to a skipped one passes.
        let mut ok = found(3, 1000);
        ok.1.download_ok = Some(true);
        ok.1.verify_ok = Some(true);
        let s = summarize(Mode::Full, &[skipped(1), ok]);
        assert_eq!(s.checks[1].pass, Some(true));
    }

    #[test]
    fn expected_failures_stay_out_of_the_percentages() {
        let mut rows: Vec<_> = (1..=19).map(|i| found(i, 1000)).collect();
        // 19 of 19 counted; the hub page fails and must not pull the rate down.
        let hub = ResultRow { target_id: 61, extract_ok: Some(false), extract_ms: Some(25_000), ..Default::default() };
        rows.push((target(61, Expected::ExpectedFail), hub));
        let s = summarize(Mode::Extract, &rows);
        assert_eq!((s.counted, s.targets_run), (19, 20));
        assert_eq!(s.extract_pct, Some(100.0));
        assert_eq!(s.mean_extract_ms, Some(1000.0), "its 25 s timeout is not part of the mean");
        assert_eq!(s.pass, Some(true));
    }

    #[test]
    fn deltas_compare_only_runs_of_the_same_mode() {
        let rows = [found(1, 1000), missed(2, 1000)];
        let extract = summarize(Mode::Extract, &rows);
        let full = summarize(Mode::Full, &rows);
        assert!(deltas(&extract, &full, 7).is_none(), "an extract run is not compared with a full run");
        assert!(deltas(&full, &extract, 7).is_none());
        let better = summarize(Mode::Extract, &[found(1, 500), found(2, 500)]);
        let d = deltas(&better, &extract, 7).unwrap();
        assert_eq!(d.previous_run_id, 7);
        assert_eq!(d.extract_pct, Some(50.0));
        assert_eq!(d.mean_extract_ms, Some(-500.0));
        assert_eq!(d.fixes, vec![2]);
        assert!(d.regressions.is_empty());
        let worse = deltas(&extract, &better, 8).unwrap();
        assert_eq!(worse.regressions, vec![2]);
    }

    #[test]
    fn a_stream_keeps_only_its_host() {
        assert_eq!(stream_host("https://CDN.Example.com:8443/a/b.m3u8?token=secret#x").as_deref(), Some("cdn.example.com"));
        assert_eq!(stream_host("//cdn.example.com/a.mp4?sig=1").as_deref(), Some("cdn.example.com"));
        assert_eq!(stream_host("not an address"), None);
    }
}
