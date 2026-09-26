use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::paths::AppPaths;

/// The full-extraction prompt the LLM used before the deterministic parser
/// (plan P4.3). Kept for `llm.mode = primary`, which is not implemented yet;
/// nothing reads it today.
pub const DEFAULT_SYSTEM_PROMPT: &str =r#"You are a broadcast automation parsing engine for a Greek newsroom. Analyze the email and extract video asset links into a strict JSON array.

=== FEW-SHOT EXAMPLES ===

EXAMPLE 1 — Simple single-link numbered section:
Input:
"1.
Πάνω από 1 εκατ. φίλαθλοι των Knicks στην παρέλαση του NBA
https://www.lifo.gr/now/sport/eimaste-oloi-mia-oikogeneia-pano-apo-1-ekat-filathloi-ton-knicks-stin-parelasi-ton
ΓΙΑ ΠΛΑΝΑ: https://www.youtube.com/watch?v=mOMyiwJCX6I"
Output:
{
  "journalist_surname": "PAPADAKI",
  "jobs": [
    {"url": "https://www.youtube.com/watch?v=mOMyiwJCX6I", "index_str": "1", "keyword": "KNICKS", "confidence": 1.0}
  ]
}

EXAMPLE 2 — Multi-link section with sub-indexing:
Input:
"4.
Στο Ρεκόρ Γκίνες ο «Τζόναθαν» ως το γηραιότερο χερσαίο ζώο στον κόσμο
https://www.lifo.gr/now/world/rekor-gkines-i-194hroni-helona-tzonathan-einai-giraiotero-hersaio-zoo-ston-kosmo
ΓΙΑ ΠΛΑΝΑ: https://www.youtube.com/watch?v=aO8YWYaNoew&t=50s"
Output:
{
  "journalist_surname": "PAPADAKI",
  "jobs": [
    {"url": "https://www.youtube.com/watch?v=aO8YWYaNoew", "index_str": "4", "keyword": "TZONATHAN", "confidence": 1.0}
  ]
}

EXAMPLE 3 — Unnumbered topic with journalist override in body text and multi-link:
Input:
"ΑΔΕΡΦΟΣ 45χρονος +ΣΥΝΟΔΟΙΠΟΡΟΙ
ΠΗΓΗ:STAR CHANNEL
https://www.facebook.com/staralithies/videos/..."
Output:
{
  "journalist_surname": "NIKOLAOU",
  "jobs": [
    {"url": "https://www.facebook.com/staralithies/videos/...", "index_str": "1", "keyword": "STARCHANNEL", "confidence": 1.0}
  ]
}

=== RULES ===

1. JOURNALIST IDENTIFICATION
Scan the SUBJECT LINE FIRST for patterns like:
  - "ΕΠΙΚΑΙΡΟΤΗΤΑ ΣΟΦΙΑ ΔΗΜΗΤΡΙΟΥ" → surname = "DIMITRIOU"
  - "ΘΕΜΑΤΑ ΑΝΝΑΣ" → surname = "PAPADAKI" (ARIA = Anna Papadaki)
  - "ΕΠΙΚΑΙΡΟΤΗΤΑ ΑΝΤΩΝΗΣ ΓΕΩΡΓΙΟΥ" → surname = "GEORGIOU"
  - "ΓΙΑ ΜΟΝΤΑΖ ΣΤΟ ΠΡΕΜΙΕΡ" → no journalist in subject → scan body
If subject has no name, scan the BODY for patterns:
  - "ΝΑ ΠΕΡΑΣΤΟΥΝ ΣΤΟ ΟΝΟΜΑ ΤΗΣ ΝΙΚΟΛΑΟΥ" → surname = "NIKOLAOU"
  - "ΝΑ ΠΕΡΑΣΤΕΙ ΣΤΟ ΟΝΟΜΑ ΜΟΥ" → use CC/sender mailbox name
  - "ΓΙΑ" + person name (e.g., "Πλάνα για Πέτρο") → surname = "PETROS"
  - "Θέματα για Καστανάκη" → surname = "GEORGIOU"
  Convert Greek surnames to LATIN uppercase (e.g., "ΝΙΚΟΛΑΟΥ" → "NIKOLAOU", "ΠΑΠΑΔΑΚΗ" → "PAPADAKI", "ΓΕΩΡΓΙΟΥ" → "GEORGIOU").
  If no name found anywhere, set journalist_surname to "MCR".

2. STRICT LITERAL NUMBERING & SUB-INDEXING
- Copy the EXACT base number from the text. If text says "4." use "4", do NOT auto-increment.
- SINGLE-LINK sections: index_str is just the number (e.g., "3", "4", "10").
- MULTI-LINK sections: append uppercase A, B, C, D... (e.g., "1A","1B","1C","1D").

3. COMPLETE DOCUMENT PROCESSING
You must scan EVERY line. Do not stop early. Process every numbered section from 1 to N.

4. TIER 1 LINKS (confidence = 1.0)
Any URL containing: youtube.com, youtu.be, youtube.com/shorts, instagram.com/reel/, instagram.com/p/, facebook.com/reel/, facebook.com/share/r/, facebook.com/watch, tiktok.com, x.com/*/status/*/video/
When a section has BOTH a Tier 1 link AND a Tier 2 link, extract ONLY the Tier 1 link(s).

5. TIER 2 LINKS (confidence = 0.5)
News portals: neakriti.gr, lifo.gr, protothema.gr, newsit.gr, iefimerida.gr, athletiko.gr, carandmotor.gr, gazzetta.gr, star.gr, bbc.com, cnn.com.
Only extract Tier 2 if NO Tier 1 link exists in that section.

6. GIA PLANA / ΓΙΑ ΠΛΑΝΑ HANDLER
When a section contains the text "GIA PLANA:" or "ΓΙΑ ΠΛΑΝΑ:" followed by a link, treat that specific link as the video asset.

7. PHOTO-ONLY BYPASS
If the ENTIRE email contains ONLY image URLs (.jpg, .png, etc.) with no video links, return: {"journalist_surname":"MCR","jobs":[]}.

8. DETERMINISTIC KEYWORD GENERATION
For each job, generate "keyword" by extracting 1-2 distinctive title words, transliterating Greek to Latin uppercase, max 20 chars.

9. OUTPUT FORMAT
Respond with ONLY valid JSON:
{
  "journalist_surname": "SURNAME",
  "jobs": [
    {
      "url": "https://...",
      "index_str": "1A",
      "keyword": "KEYWORD",
      "confidence": 1.0
    }
  ]
}"#;

/// Security policy (plan P2.1, P2.2).
///
/// This replaces the old `auth_mode` enum. `auth_mode: "open_mcr"` was a global
/// switch: it made `/mcr` and every read API reachable by *anyone* who could
/// route to the port, which on a newsroom LAN is everyone. The replacement is
/// an explicit allowlist of client networks, so the same convenience — MCR
/// workstations never see a login screen — costs exactly the networks the
/// operator names and nothing else.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Client networks that may use the MCR panel without logging in, in CIDR
    /// form. An **empty list means login for everyone** — that is the safe
    /// reading, and it is what a station that never configures this gets once
    /// it removes the loopback default.
    #[serde(default = "default_mcr_open_networks")]
    pub mcr_open_networks: Vec<String>,

    /// Read the client IP from `X-Forwarded-For` instead of the socket peer.
    ///
    /// Off by default, and even when on it is only honoured when the *direct*
    /// peer is in [`Self::trusted_proxies`]. A forwarded header from an
    /// untrusted peer is client-controlled text, so trusting it unconditionally
    /// would let anyone claim to be on the allowlist.
    #[serde(default)]
    pub trust_proxy_header: bool,

    /// Peers whose `X-Forwarded-For` is believed, in CIDR form.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,

    /// Absolute session lifetime for `user` accounts.
    #[serde(default = "default_session_hours_user")]
    pub session_hours_user: i64,

    /// Absolute session lifetime for `admin` accounts — deliberately the
    /// shortest, because an admin session can repoint the watchfolder.
    #[serde(default = "default_session_hours_admin")]
    pub session_hours_admin: i64,

    /// Absolute session lifetime for MCR accounts. Long, because the MCR desk
    /// is a shared always-on workstation in a controlled room and a login
    /// prompt mid-bulletin is its own kind of outage.
    #[serde(default = "default_session_days_mcr")]
    pub session_days_mcr: i64,

    /// Idle timeout: a session unused for this long is ended, whatever its
    /// absolute lifetime. This is what protects an unattended browser on a
    /// shared MCR workstation, which the absolute window alone does not.
    #[serde(default = "default_session_idle_hours")]
    pub session_idle_hours: i64,

    /// Failed logins allowed per client IP per five minutes before a 429.
    #[serde(default = "default_login_rate_limit")]
    pub login_rate_limit_per_5min: u32,

    /// Failed logins allowed per account per hour, independent of source IP.
    /// This is the one that matters against a distributed guess.
    #[serde(default = "default_login_rate_limit_account")]
    pub login_rate_limit_per_account_hour: u32,
}

fn default_mcr_open_networks() -> Vec<String> {
    vec!["127.0.0.1/32".to_string(), "::1/128".to_string()]
}
fn default_session_hours_user() -> i64 {
    12
}
fn default_session_hours_admin() -> i64 {
    8
}
fn default_session_days_mcr() -> i64 {
    30
}
fn default_retention_days() -> i64 {
    7
}
fn default_session_idle_hours() -> i64 {
    12
}
fn default_login_rate_limit() -> u32 {
    5
}
fn default_login_rate_limit_account() -> u32 {
    10
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            mcr_open_networks: default_mcr_open_networks(),
            trust_proxy_header: false,
            trusted_proxies: Vec::new(),
            session_hours_user: default_session_hours_user(),
            session_hours_admin: default_session_hours_admin(),
            session_days_mcr: default_session_days_mcr(),
            session_idle_hours: default_session_idle_hours(),
            login_rate_limit_per_5min: default_login_rate_limit(),
            login_rate_limit_per_account_hour: default_login_rate_limit_account(),
        }
    }
}

impl SecurityConfig {
    /// Parse [`Self::mcr_open_networks`], dropping and logging bad entries.
    pub fn open_networks(&self) -> crate::net::CidrSet {
        crate::net::CidrSet::parse_lossy(&self.mcr_open_networks)
    }

    /// Parse [`Self::trusted_proxies`], dropping and logging bad entries.
    pub fn trusted_proxy_networks(&self) -> crate::net::CidrSet {
        crate::net::CidrSet::parse_lossy(&self.trusted_proxies)
    }
}

/// TLS material for the web listener (plan P2.7).
///
/// The certificate is a **PKCS#12** bundle (`.pfx`/`.p12`) holding the
/// certificate and its private key together, because the TLS implementation is
/// Windows SChannel — see the note on `WebServer::run_tls` for why rustls is
/// not used. A `.pfx` is also the form Windows IT issues, so this asks the
/// operator for the file they already have rather than for a PEM pair they
/// would have to convert.
///
/// The passphrase is **not** here: it lives in the secret store under
/// `web.tls_password`, like every other credential (plan P2.6).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsConfig {
    #[serde(default)]
    pub cert_path: Option<String>,
}

impl TlsConfig {
    pub fn is_enabled(&self) -> bool {
        self.cert_path.is_some()
    }

    /// `Ok(None)` when TLS is off.
    pub fn resolve(&self, paths: &AppPaths) -> Result<Option<PathBuf>> {
        let Some(cert) = &self.cert_path else {
            return Ok(None);
        };
        let resolved = paths.resolve(cert);
        if !resolved.exists() {
            // Refuse to start rather than silently falling back to plaintext:
            // an operator who configured TLS and got HTTP would have no way to
            // tell, and the `Secure` cookie flag would be wrong either way.
            anyhow::bail!(
                "web.tls.cert_path points at {resolved:?}, which does not exist. Refusing to \
                 start rather than silently serving the panels over plaintext HTTP."
            );
        }
        Ok(Some(resolved))
    }
}

/// Microsoft Graph mailbox (plan P4.2, defect E-01).
///
/// Used instead of IMAP whenever it is configured: Exchange Online no longer
/// accepts the basic-auth IMAP login the daemon started with. The app
/// registration needs `Mail.Read` (application), limited to the ingest
/// mailbox by an Exchange application access policy. `Mail.ReadWrite` is
/// optional and only used when `write_access` is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphConfig {
    #[serde(default)]
    pub tenant_id: String,
    #[serde(default)]
    pub client_id: String,
    /// The ingest mailbox, e.g. `ingest@example.gr`.
    #[serde(default)]
    pub mailbox: String,
    #[serde(default = "default_processed_folder")]
    pub processed_folder: String,
    #[serde(default = "default_failed_folder")]
    pub failed_folder: String,
    /// The app also holds `Mail.ReadWrite`: mark processed mail read and move
    /// it to `processed_folder` / `failed_folder`. Off by default; with
    /// `Mail.Read` alone the mailbox is never written and the database alone
    /// records what was handled (plan P4.8).
    #[serde(default)]
    pub write_access: bool,
    /// Client secret — **runtime only**, loaded from the secret store
    /// (`graph.client_secret`). Never read from or written to config.json.
    #[serde(skip)]
    pub client_secret: String,
}

fn default_processed_folder() -> String {
    "Omni/Processed".into()
}

fn default_failed_folder() -> String {
    "Omni/Failed".into()
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            tenant_id: String::new(),
            client_id: String::new(),
            mailbox: String::new(),
            processed_folder: default_processed_folder(),
            failed_folder: default_failed_folder(),
            write_access: false,
            client_secret: String::new(),
        }
    }
}

impl GraphConfig {
    /// Everything needed to log in is present, secret included.
    pub fn is_configured(&self) -> bool {
        !self.tenant_id.trim().is_empty()
            && !self.client_id.trim().is_empty()
            && !self.mailbox.trim().is_empty()
            && !self.client_secret.is_empty()
    }
}

/// Environment variables that override the Graph settings (plan P4.9).
///
/// For console and development runs, so a developer's own app registration
/// never has to be typed into a config.json that sits in a working copy. The
/// service should keep using config.json and the encrypted store: a service's
/// environment lives in plaintext in the registry.
pub mod env_vars {
    pub const GRAPH_TENANT_ID: &str = "OMNI_GRAPH_TENANT_ID";
    pub const GRAPH_CLIENT_ID: &str = "OMNI_GRAPH_CLIENT_ID";
    pub const GRAPH_MAILBOX: &str = "OMNI_GRAPH_MAILBOX";
    pub const GRAPH_CLIENT_SECRET: &str = "OMNI_GRAPH_CLIENT_SECRET";

    /// Variables whose values must be redacted from every log line.
    pub const SECRETS: &[&str] = &[GRAPH_CLIENT_SECRET];
}

/// What the LLM may do with an email (plan P4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LlmMode {
    /// Never called. The deterministic parser alone decides.
    Off,
    /// Called only where the parser is unsure, and only to suggest a
    /// journalist or a keyword, which are validated before use.
    #[default]
    Assist,
    /// Full extraction by the model. Not implemented: treated as `assist`,
    /// with a warning at start-up.
    Primary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    #[serde(default)]
    pub mode: LlmMode,
    #[serde(default = "default_llm_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_llm_max_tokens")]
    pub max_tokens: u32,
    /// Ask for a keyword for every section, not only where the parser fell
    /// back to `ASSET`.
    #[serde(default)]
    pub keyword_polish: bool,
}

fn default_llm_timeout() -> u64 {
    30
}

fn default_llm_max_tokens() -> u32 {
    400
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            mode: LlmMode::default(),
            timeout_secs: default_llm_timeout(),
            max_tokens: default_llm_max_tokens(),
            keyword_polish: false,
        }
    }
}

/// Email parser tuning (plan P4.3; `parser` in the config v2 shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParserConfig {
    /// News portals whose article pages carry the video (Tier 2).
    #[serde(default = "default_tier2_domains")]
    pub tier2_domains: Vec<String>,
    /// Queue low-confidence links anyway and let the sniffer try; only
    /// failures reach MCR review.
    #[serde(default = "default_true")]
    pub auto_attempt_unknown_domains: bool,
    /// Subject words that raise the job priority.
    #[serde(default = "default_urgent_keywords")]
    pub urgent_keywords: Vec<String>,
    /// Video attachments above this are not queued (warning instead).
    #[serde(default = "default_max_attachment_mb")]
    pub max_attachment_mb: u64,
}

pub fn default_tier2_domains() -> Vec<String> {
    [
        "neakriti.gr",
        "lifo.gr",
        "protothema.gr",
        "newsit.gr",
        "iefimerida.gr",
        "athletiko.gr",
        "carandmotor.gr",
        "gazzetta.gr",
        "star.gr",
        "ertnews.gr",
        "ert.gr",
        "in.gr",
        "news247.gr",
        "cnn.gr",
        "bbc.com",
        "bbc.co.uk",
        "cnn.com",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub fn default_urgent_keywords() -> Vec<String> {
    vec!["ΕΚΤΑΚΤΟ".into(), "BREAKING".into(), "URGENT".into()]
}

fn default_max_attachment_mb() -> u64 {
    2048
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            tier2_domains: default_tier2_domains(),
            auto_attempt_unknown_domains: true,
            urgent_keywords: default_urgent_keywords(),
            max_attachment_mb: default_max_attachment_mb(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_db_path")]
    pub database_path: String,

    #[serde(default = "default_watchfolder")]
    pub watchfolder_path: String,

    #[serde(default = "default_temp_path")]
    pub temp_path: String,

    #[serde(default = "default_bin_dir")]
    pub bin_dir: String,

    #[serde(default = "default_web_port")]
    pub web_port: u16,

    #[serde(default = "default_web_host")]
    pub web_host: String,

    #[serde(default)]
    pub security: SecurityConfig,

    #[serde(default)]
    pub tls: TlsConfig,

    #[serde(default)]
    pub log: crate::logging::LogConfig,

    #[serde(default)]
    pub parser: ParserConfig,

    #[serde(default)]
    pub graph: GraphConfig,

    /// How long a finished job's working files are kept before the nightly
    /// retention task removes them (plan P6.6).
    ///
    /// Only the *workspace* — the download and the intermediate transcode —
    /// not the delivered MXF, which belongs to playout, and not the job row,
    /// which is the record of what went to air.
    #[serde(default = "default_retention_days")]
    pub retention_days: i64,

    #[serde(default = "default_concurrent")]
    pub max_concurrent_downloads: usize,

    #[serde(default = "default_concurrent")]
    pub max_concurrent_transcodes: usize,

    // Email ingest: the mailbox itself is `graph` below (plan P4.7).
    /// Pre-Graph IMAP password, read so [`AppConfig::adopt_secrets`] can wipe
    /// it from an old config.json. IMAP is gone; the value is never stored,
    /// used or written back.
    #[serde(default, skip_serializing, rename = "email_password")]
    legacy_email_password: String,

    #[serde(default = "default_poll_interval")]
    pub email_poll_interval_secs: u64,

    // LLM configuration
    #[serde(default = "default_ollama_endpoint")]
    pub ollama_endpoint: String,

    #[serde(default = "default_ollama_model")]
    pub ollama_model: String,

    /// LLM assist policy (plan P4.4). Endpoint and model stay above.
    ///
    /// There is deliberately no `system_prompt` any more (defect E-03): the
    /// deployed config.json overrode the tuned prompt with a one-liner and
    /// nobody could tell. An old file that still has the key loads fine; it
    /// is ignored, and the next save drops it.
    #[serde(default)]
    pub llm: LlmConfig,

    // Update settings
    #[serde(default = "default_channel")]
    pub ytdl_channel: String,

    #[serde(default = "default_true")]
    pub ytdl_auto_update_nightly: bool,

    // Adblock settings
    #[serde(default = "default_true")]
    pub adblock_enabled: bool,

    #[serde(default = "default_true")]
    pub adblock_hagezi_enabled: bool,

    #[serde(default = "default_true")]
    pub adblock_greek_enabled: bool,

    #[serde(default = "default_true")]
    pub adblock_auto_update_nightly: bool,

    #[serde(default)]
    pub adblock_ubol_extension_enabled: bool,

    // Tool overrides
    pub ffmpeg_path: Option<String>,
    pub ffprobe_path: Option<String>,
    pub bmxtranswrap_path: Option<String>,
    pub ytdl_path: Option<String>,
}

fn default_db_path() -> String {
    "data/omni.db".to_string()
}
fn default_watchfolder() -> String {
    "C:/Users/Shado/Videos/test".to_string()
}
fn default_temp_path() -> String {
    "temp".to_string()
}
fn default_bin_dir() -> String {
    "bin".to_string()
}
fn default_web_port() -> u16 {
    8080
}
fn default_web_host() -> String {
    "0.0.0.0".to_string()
}
fn default_concurrent() -> usize {
    2
}
fn default_poll_interval() -> u64 {
    20
}
fn default_ollama_endpoint() -> String {
    "http://localhost:11434/v1".to_string()
}
fn default_ollama_model() -> String {
    "google/gemma-4-e4b".to_string()
}

fn default_channel() -> String {
    "stable".to_string()
}
fn default_true() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            database_path: default_db_path(),
            watchfolder_path: default_watchfolder(),
            temp_path: default_temp_path(),
            bin_dir: default_bin_dir(),
            web_port: default_web_port(),
            web_host: default_web_host(),
            security: SecurityConfig::default(),
            tls: TlsConfig::default(),
            log: crate::logging::LogConfig::default(),
            parser: ParserConfig::default(),
            graph: GraphConfig::default(),
            retention_days: default_retention_days(),
            max_concurrent_downloads: default_concurrent(),
            max_concurrent_transcodes: default_concurrent(),
            legacy_email_password: String::new(),
            email_poll_interval_secs: default_poll_interval(),
            ollama_endpoint: default_ollama_endpoint(),
            ollama_model: default_ollama_model(),
            llm: LlmConfig::default(),
            ytdl_channel: default_channel(),
            ytdl_auto_update_nightly: true,
            adblock_enabled: true,
            adblock_hagezi_enabled: true,
            adblock_greek_enabled: true,
            adblock_auto_update_nightly: true,
            adblock_ubol_extension_enabled: false,
            ffmpeg_path: None,
            ffprobe_path: None,
            bmxtranswrap_path: None,
            ytdl_path: None,
        }
    }
}

/// Tell the operator, loudly, that `auth_mode` no longer does anything.
///
/// Serde ignores unknown fields, so an existing deployment that ran
/// `auth_mode: "open_mcr"` would upgrade into the new loopback-only default and
/// the MCR desk would meet a login screen with no explanation. The tightening
/// is deliberate — the old switch was open to the whole LAN — but it must not
/// be silent.
fn warn_on_removed_auth_mode(raw: &str, path: &Path) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return;
    };
    let Some(mode) = value.get("auth_mode").and_then(|v| v.as_str()) else {
        return;
    };
    if mode == "open_mcr" {
        tracing::warn!(
            config = ?path,
            "`auth_mode: open_mcr` has been removed: it opened the MCR panel to every host \
             that could reach the port. Access without login is now granted per client \
             network. Set security.mcr_open_networks to the newsroom subnet (e.g. \
             [\"10.20.0.0/16\"]); until then only loopback skips the login screen."
        );
    } else {
        tracing::warn!(
            config = ?path,
            "`auth_mode` has been removed and is ignored; see security.mcr_open_networks."
        );
    }
}

impl AppConfig {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let p = path.as_ref();
        if p.exists() {
            let content = std::fs::read_to_string(p)
                .with_context(|| format!("Failed reading config file at {:?}", p))?;
            let config: AppConfig = serde_json::from_str(&content)
                .with_context(|| format!("Failed parsing JSON from {:?}", p))?;
            warn_on_removed_auth_mode(&content, p);
            Ok(config)
        } else {
            let config = AppConfig::default();
            config.save_to_file(p)?;
            Ok(config)
        }
    }

    /// Write config.json.
    ///
    /// Secrets are `skip_serializing`, so they cannot reach this file even if
    /// a caller forgets. `secrets_never_reach_config_json` asserts it.
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let p = path.as_ref();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed creating directory for config {:?}", parent))?;
        }
        let json_str = serde_json::to_string_pretty(self)
            .context("Failed serializing AppConfig to JSON")?;
        std::fs::write(p, json_str)
            .with_context(|| format!("Failed writing config file {:?}", p))?;
        Ok(())
    }

    /// Move any plaintext secret out of config.json and load the real ones
    /// from the encrypted store (plan P2.6).
    ///
    /// Returns `true` when config.json needs rewriting because a secret was
    /// harvested from it. The caller must then save, or the plaintext stays on
    /// disk until something else happens to save.
    ///
    /// Idempotent: on every later start there is nothing to harvest and this
    /// just repopulates the runtime field.
    pub fn adopt_secrets(&mut self, store: &crate::secrets::SecretStore) -> Result<bool> {
        use crate::secrets::keys;

        let mut rewrote = false;

        if !self.legacy_email_password.is_empty() {
            // IMAP is gone (plan P4.7): nothing reads this password any more,
            // so it is dropped rather than moved into the store.
            self.legacy_email_password.clear();
            tracing::warn!(
                "Removed the old IMAP mailbox password from config.json; IMAP is no longer                  supported. The value is still in any backup of config.json taken before now."
            );
            rewrote = true;
        }
        for key in keys::RETIRED {
            if store.get_lossy(key).is_some() {
                store.remove(key).with_context(|| format!("Failed removing retired secret {key}"))?;
                tracing::warn!("Removed the retired secret `{key}` from the encrypted store.");
            }
        }

        // Always read back from the store, so the store is the single source
        // of truth and a secret removed there takes effect on restart.
        self.graph.client_secret = store.get_lossy(keys::GRAPH_CLIENT_SECRET).unwrap_or_default();
        Ok(rewrote)
    }

    /// Apply the `OMNI_GRAPH_*` overrides ([`env_vars`]). An unset or blank
    /// variable leaves the setting alone. Returns the names applied, never
    /// the values, for the start-up log.
    ///
    /// Call after [`AppConfig::adopt_secrets`]: the environment wins over the
    /// store for the life of the process and is never written anywhere.
    pub fn apply_env_overrides(&mut self, get: impl Fn(&str) -> Option<String>) -> Vec<&'static str> {
        let mut applied = Vec::new();
        let targets: [(&'static str, &mut String); 4] = [
            (env_vars::GRAPH_TENANT_ID, &mut self.graph.tenant_id),
            (env_vars::GRAPH_CLIENT_ID, &mut self.graph.client_id),
            (env_vars::GRAPH_MAILBOX, &mut self.graph.mailbox),
            (env_vars::GRAPH_CLIENT_SECRET, &mut self.graph.client_secret),
        ];
        for (name, field) in targets {
            if let Some(value) = get(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
                *field = value;
                applied.push(name);
            }
        }
        applied
    }

    /// Resolve a config path against the **install directory** (plan P0.1, W-10).
    ///
    /// Under the Windows service the process working directory is
    /// `C:\Windows\System32`, so the previous CWD-relative behaviour silently
    /// created a second, empty `data/omni.db` there while the operator stared at
    /// an empty queue. Callers that hold an [`AppPaths`] should prefer
    /// [`AppPaths::resolve`]; this method exists for the code paths that only
    /// have a config, and delegates to the same logic.
    pub fn resolve_path_in(&self, paths: &AppPaths, relative_or_absolute: &str) -> PathBuf {
        paths.resolve(relative_or_absolute)
    }

    /// Resolve against the discovered install root.
    ///
    /// Kept for call sites that have no `AppPaths` to hand. It discovers the
    /// root from the executable location, never from the CWD.
    pub fn resolve_path(&self, relative_or_absolute: &str) -> PathBuf {
        let p = Path::new(relative_or_absolute);
        if p.is_absolute() {
            return p.to_path_buf();
        }
        AppPaths::discover("config.json").resolve(relative_or_absolute)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{keys, SecretStore};
    use tempfile::TempDir;

    #[test]
    fn secrets_never_reach_config_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");

        let mut config = AppConfig::default();
        config.graph.client_secret = "plaintext-graph-secret".to_string();
        config.save_to_file(&path).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            !written.contains("plaintext-graph-secret"),
            "the Graph secret was written to config.json: {written}"
        );
        assert!(!written.contains("client_secret"), "the key itself should not be emitted either: {written}");
    }

    #[test]
    fn an_old_imap_password_is_wiped_from_config_json_and_not_kept() {
        // A config.json from before P4.7, with the IMAP password (and its
        // server fields) still in it, and the password in the store as well.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{ "imap_server": "outlook.office365.com", "imap_port": 993,
                 "email_address": "ingest@station.gr", "email_password": "legacy-secret-value" }"#,
        )
        .unwrap();
        let store = SecretStore::new(dir.path().join("secrets.bin"));
        store.set("mail.password", "legacy-secret-value").unwrap();

        let mut config = AppConfig::load_from_file(&path).unwrap();
        assert!(config.adopt_secrets(&store).unwrap(), "the caller must be told to rewrite config.json");
        assert_eq!(store.get("mail.password").unwrap(), None, "the retired secret stays in the store");

        config.save_to_file(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("legacy-secret-value"), "{written}");
        assert!(!written.contains("imap"), "IMAP settings survived the rewrite: {written}");

        // Second start: nothing left to wipe.
        let mut config2 = AppConfig::load_from_file(&path).unwrap();
        assert!(!config2.adopt_secrets(&store).unwrap());
    }

    #[test]
    fn removing_a_secret_from_the_store_takes_effect_on_the_next_load() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        AppConfig::default().save_to_file(&path).unwrap();

        let store = SecretStore::new(dir.path().join("secrets.bin"));
        store.set(keys::GRAPH_CLIENT_SECRET, "current-value").unwrap();

        let mut config = AppConfig::load_from_file(&path).unwrap();
        config.adopt_secrets(&store).unwrap();
        assert_eq!(config.graph.client_secret, "current-value");

        store.remove(keys::GRAPH_CLIENT_SECRET).unwrap();
        let mut config = AppConfig::load_from_file(&path).unwrap();
        config.adopt_secrets(&store).unwrap();
        assert_eq!(config.graph.client_secret, "");
    }

    #[test]
    fn environment_overrides_win_over_the_store_and_never_reach_config_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let store = SecretStore::new(dir.path().join("secrets.bin"));
        store.set(keys::GRAPH_CLIENT_SECRET, "stored-secret").unwrap();

        let mut config = AppConfig::default();
        config.graph.tenant_id = "tenant-from-file".into();
        config.adopt_secrets(&store).unwrap();
        let env = |name: &str| match name {
            "OMNI_GRAPH_CLIENT_ID" => Some("client-from-env".to_string()),
            "OMNI_GRAPH_CLIENT_SECRET" => Some("  secret-from-env \n".to_string()),
            "OMNI_GRAPH_MAILBOX" => Some("   ".to_string()),
            _ => None,
        };
        let applied = config.apply_env_overrides(env);

        assert_eq!(applied, vec![env_vars::GRAPH_CLIENT_ID, env_vars::GRAPH_CLIENT_SECRET]);
        assert_eq!(config.graph.tenant_id, "tenant-from-file", "unset variable changed the setting");
        assert_eq!(config.graph.client_id, "client-from-env");
        assert_eq!(config.graph.client_secret, "secret-from-env");
        assert_eq!(config.graph.mailbox, "", "a blank variable must not count");

        config.save_to_file(&path).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("secret-from-env"));
        assert_eq!(store.get(keys::GRAPH_CLIENT_SECRET).unwrap().as_deref(), Some("stored-secret"));
    }

    #[test]
    fn the_graph_client_secret_lives_only_in_the_store() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");

        // Even a hand-edited config.json cannot supply it...
        std::fs::write(
            &path,
            r#"{ "graph": { "tenant_id": "t", "client_id": "c", "mailbox": "ingest@example.gr",
                            "client_secret": "typed-into-config" } }"#,
        )
        .unwrap();
        let mut config = AppConfig::load_from_file(&path).unwrap();
        assert_eq!(config.graph.client_secret, "");
        assert!(!config.graph.is_configured());
        assert_eq!(config.graph.processed_folder, "Omni/Processed");

        // ...the store does, and a save never writes it back.
        let store = SecretStore::new(dir.path().join("secrets.bin"));
        store.set(keys::GRAPH_CLIENT_SECRET, "from-the-store").unwrap();
        config.adopt_secrets(&store).unwrap();
        assert_eq!(config.graph.client_secret, "from-the-store");
        assert!(config.graph.is_configured());

        config.save_to_file(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("from-the-store"));
        assert!(!written.contains("client_secret"));
        assert!(written.contains("ingest@example.gr"));
    }

    #[test]
    fn the_removed_auth_mode_does_not_grant_open_access_after_an_upgrade() {
        // A deployment running `auth_mode: open_mcr` must not silently keep
        // station-wide access; it falls back to the loopback default, and the
        // loader warns.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{ "auth_mode": "open_mcr" }"#).unwrap();

        let config = AppConfig::load_from_file(&path).unwrap();
        assert_eq!(
            config.security.mcr_open_networks,
            vec!["127.0.0.1/32".to_string(), "::1/128".to_string()]
        );
    }
}
