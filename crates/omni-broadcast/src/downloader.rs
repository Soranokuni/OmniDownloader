//! yt-dlp runner (plan P1.8; defects D-17, D-18).
//!
//! Everything here goes through `omni_core::process::run`, so the download has a
//! timeout and dies with its whole process tree — yt-dlp spawns ffmpeg to mux
//! fragments, and an orphaned ffmpeg holds the job's workspace open (defect
//! D-03). Every start goes through [`paced`] too: a run on a video platform
//! waits for its turn there (P11.4).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use omni_core::process::{run, RunOpts, RunOutcome};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::errors::{classify_download_error, ErrorCode};
use crate::pacing::{platform_key, HostPacer, PacePermit, Refused, RATE_LIMIT_COOL_DOWN};

pub struct DownloadProgress {
    pub percent: f64,
    pub speed: String,
    pub eta: String,
}

/// A download failure with its classification already applied, so the pipeline
/// does not have to re-read stderr to decide whether a retry could help.
#[derive(Debug)]
pub struct DownloadError {
    pub code: ErrorCode,
    pub message: String,
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DownloadError {}

/// Options for one download.
pub struct DownloadOpts<'a> {
    /// Sent with the request. Many Greek portals 403 a bare fetch of a stream
    /// URL but serve it happily with the article page as the referer.
    pub referer: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub cookie_header: Option<&'a str>,
    /// Netscape cookie jar for this domain (plan P3.5).
    pub cookie_jar: Option<&'a Path>,
    /// The jar's values: yt-dlp quotes a line it rejects, so every error text of a
    /// run that had a jar is scrubbed of them.
    pub cookie_values: &'a [String],
    /// Skip TLS verification. Off by default (defect D-17): a station that
    /// ingests whatever a MITM hands it is not a station with working ingest.
    pub insecure_tls: bool,
    pub max_height: u32,
    pub concurrent_fragments: u32,
    pub timeout: Duration,
    pub cancel: Option<CancellationToken>,
    /// Deno, for YouTube's JavaScript challenge (plan P6.8). `download`
    /// fills it from `bin/` when the caller leaves it empty.
    pub js_runtime: Option<&'a Path>,
    /// The pinned ffmpeg for the merge (P11.4e). `download` fills it from
    /// the downloader when the caller leaves it empty.
    pub ffmpeg: Option<&'a Path>,
    /// Which formats to ask for: the usual ones, or the second route's (P11.6).
    pub formats: FormatRoute,
}

/// The format selection of a download (P11.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FormatRoute {
    /// H.264/AAC merged into MP4 where offered, else the best up to the cap.
    #[default]
    Usual,
    /// The second route after a source that failed probe, encode, wrap or the
    /// gate: another video codec at the same cap, else a file the site serves
    /// already merged as MP4 (no merge step of ours), both at least 720 lines
    /// high so the second try is no big drop in picture; else as usual.
    Second,
}

/// The lowest height the second route's own alternatives take (P11.6).
pub const SECOND_ROUTE_MIN_HEIGHT: u32 = 720;

/// The `-f` selector for `route`, every alternative capped at `h` but the last.
pub fn format_selector(route: FormatRoute, h: u32) -> String {
    match route {
        FormatRoute::Usual => format!(
            "bestvideo[height<={h}][vcodec^=avc1]+bestaudio[acodec^=mp4a]/\
             bestvideo[height<={h}]+bestaudio/best[height<={h}]/best"
        ),
        FormatRoute::Second => {
            let floor = SECOND_ROUTE_MIN_HEIGHT;
            format!(
                "bestvideo[height<={h}][height>={floor}][vcodec!^=avc1]+bestaudio/\
                 best[height<={h}][height>={floor}][ext=mp4]/best[height<={h}]/best"
            )
        }
    }
}

impl Default for DownloadOpts<'_> {
    fn default() -> Self {
        Self {
            referer: None,
            user_agent: None,
            cookie_header: None,
            cookie_jar: None,
            cookie_values: &[],
            insecure_tls: false,
            max_height: 1080,
            concurrent_fragments: 4,
            timeout: Duration::from_secs(1800),
            cancel: None,
            js_runtime: None,
            ffmpeg: None,
            formats: FormatRoute::Usual,
        }
    }
}

/// Hosts yt-dlp has a native extractor for, which want to be fetched as
/// themselves, never with another site's page context.
const VIDEO_PLATFORMS: &[&str] = &[
    "youtube.com",
    "youtu.be",
    "youtube-nocookie.com",
    "x.com",
    "twitter.com",
    "facebook.com",
    "fb.watch",
    "instagram.com",
    "tiktok.com",
    "vimeo.com",
    "dailymotion.com",
    "dai.ly",
    "streamable.com",
];

/// The `--match-filters` expression that keeps live and upcoming videos out.
/// Its skip message names it, which is how [`skipped_as_live`] recognises it.
pub const LIVE_FILTER: &str = "!is_live & live_status!=?is_upcoming";

/// Did yt-dlp skip the video because [`LIVE_FILTER`] did not pass? Without
/// `--print` it says `[download] <title> does not pass filter (<filter>),
/// skipping ..` on stdout; with our `--print` (which implies quiet) it says
/// nothing, so this is only a secondary signal: see [`empty_download_is_live`].
pub fn skipped_as_live(lines: &[String]) -> bool {
    lines.iter().any(|l| l.contains("does not pass filter") && l.contains(LIVE_FILTER))
}

/// yt-dlp exited 0 and produced no file. Was that the live filter skipping a
/// live or upcoming video? Yes if its skip line is in `lines`, or if `probe`
/// (yt-dlp's own `is_live` / `live_status` for the same address) says so. A
/// probe that cannot answer says no, and the caller reports the plain
/// "no file" error.
pub async fn empty_download_is_live<F, Fut>(lines: &[String], probe: F) -> bool
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Option<bool>>,
{
    skipped_as_live(lines) || probe().await == Some(true)
}

/// How many candidates [`choose_primary`] asks about, at most.
pub const MAX_LIVE_PROBES: usize = 3;

/// One video the job might take as its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cand {
    pub url: String,
    /// Not the article's own: outside it, a link outside its body, a
    /// contextual glomex playlist. Held candidates are only taken when no
    /// other exists.
    pub held: bool,
    /// A platform video, whose live flag can be asked of yt-dlp. A raw
    /// stream has none (the generic extractor sets no `is_live`).
    pub probe_able: bool,
}

/// The candidates of a sniff for [`choose_primary`], in the sniffer's rank order. Every kind is
/// probe-able here (a raw stream or a player page can be gone as well as a platform post);
/// `held` says which are outside the article's own content.
pub fn sniffed_cands(streams: &[String], held: impl Fn(&str) -> bool) -> Vec<Cand> {
    streams.iter().map(|u| Cand { url: u.clone(), held: held(u), probe_able: true }).collect()
}

/// What a probe says about one candidate (P3.14, P3.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// A live or upcoming stream: never taken.
    Live,
    /// The candidate is gone (see [`is_dead_candidate`]): never taken.
    Dead(ErrorCode),
    /// yt-dlp reads it.
    Ok,
    /// Timeout, a retryable or other failure, no answer: the candidate stays in play.
    Unknown,
}

impl Probe {
    /// From a length probe (`probe_media`), which knows live but not dead.
    pub fn from_length(info: Option<PlatformInfo>) -> Self {
        match info {
            Some(i) if i.live => Probe::Live,
            Some(_) => Probe::Ok,
            None => Probe::Unknown,
        }
    }
}

/// The outcome of [`choose_primary`]; `live` and `dead` are the candidates found so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    Primary { url: String, live: Vec<String>, dead: Vec<String> },
    /// Every candidate of the tier in play is a live video.
    AllLive { live: Vec<String> },
    /// Every candidate of the tier in play is known dead, or dead or live with none readable:
    /// `code` is the first dead one's.
    AllDead { code: ErrorCode, live: Vec<String> },
    /// There was no candidate.
    Nothing,
}

/// The job's video among `cands` (in the order the caller ranks them).
///
/// Only the article's own candidates (`!held`) are walked while any exist; a
/// held one is never a fallback for them. With none of the own, the held ones
/// are walked, which is how a page whose only video is held still yields it.
/// In the walk a probe-able candidate is asked (at most `max_probes` asks in
/// all); a live or dead one is skipped, and the first that is readable, unknown,
/// or not asked wins. A tier whose every candidate is known live is `AllLive`;
/// one with every candidate known dead, or dead and live with none readable, is
/// `AllDead` with the first dead code. One candidate is never asked: there is
/// nothing to choose, and the download itself decides.
pub async fn choose_primary<F, Fut>(cands: &[Cand], max_probes: usize, mut probe: F) -> Choice
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Probe>,
{
    match cands {
        [] => return Choice::Nothing,
        [only] => return Choice::Primary { url: only.url.clone(), live: Vec::new(), dead: Vec::new() },
        _ => {}
    }
    let own: Vec<&Cand> = cands.iter().filter(|c| !c.held).collect();
    let tier: Vec<&Cand> = if own.is_empty() { cands.iter().collect() } else { own };
    let mut live = Vec::new();
    let mut dead: Vec<String> = Vec::new();
    let mut first_dead: Option<ErrorCode> = None;
    let mut asked = 0usize;
    for c in tier {
        if c.probe_able && asked < max_probes {
            asked += 1;
            match probe(c.url.clone()).await {
                Probe::Live => {
                    live.push(c.url.clone());
                    continue;
                }
                Probe::Dead(code) => {
                    first_dead.get_or_insert(code);
                    dead.push(c.url.clone());
                    continue;
                }
                Probe::Ok | Probe::Unknown => {}
            }
        }
        return Choice::Primary { url: c.url.clone(), live, dead };
    }
    match first_dead {
        Some(code) => Choice::AllDead { code, live },
        None => Choice::AllLive { live },
    }
}

/// Whether `url` is on YouTube.
pub fn is_youtube(url: &str) -> bool {
    let host = host_of(url);
    ["youtube.com", "youtu.be", "youtube-nocookie.com"]
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

pub(crate) fn host_of(url: &str) -> String {
    url.split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Whether `url` is a page on a video platform (as opposed to a raw stream
/// or a news portal's article).
pub fn is_video_platform(url: &str) -> bool {
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    VIDEO_PLATFORMS
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

pub struct Downloader {
    ytdl_path: PathBuf,
    /// `deno.exe` next to yt-dlp, when it is there.
    deno: Option<PathBuf>,
    /// The ffmpeg the daemon resolved, handed to every run ([`hardening_args`]).
    ffmpeg: Option<PathBuf>,
    /// The site's cookie jar file for the probes of this value (P3.5); a
    /// download takes its own from `DownloadOpts::cookie_jar`.
    cookie_jar: Option<PathBuf>,
    cookie_values: Vec<String>,
    /// Every run on a video platform waits here for its turn (P11.4).
    pacer: HostPacer,
    /// Told of a wait for a turn longer than [`crate::pacing::NOTE_WAIT_OVER`].
    on_wait: Option<WaitNote>,
    /// Not a job's: a cool-down is reported at once, not waited out (see [`paced`]).
    fail_fast: bool,
}

/// Told the platform key and the wait when a run waited for its turn.
pub type WaitNote = std::sync::Arc<dyn Fn(&'static str, Duration) + Send + Sync>;

/// A run that never started because its wait for a turn was cancelled: read
/// like a cancelled run by every caller.
fn cancelled_before_start() -> RunOutcome {
    RunOutcome {
        exit_code: None,
        success: false,
        elapsed: Duration::ZERO,
        stdout: String::new(),
        stderr_tail: String::new(),
        timed_out: false,
        cancelled: true,
    }
}

/// A run that was not started because `key` is cooling down for `left`: a
/// failure whose error line classifies as RATE_LIMITED and says so.
fn cooling_down(key: &str, left: Duration) -> RunOutcome {
    RunOutcome { stderr_tail: crate::pacing::cooling_down_line(key, left), cancelled: false, ..cancelled_before_start() }
}

/// Where a download keeps its turn on the platform, so its stdout handler can
/// give it back at the first progress line ([`release_turn`]).
pub(crate) type TurnSlot = std::sync::Arc<std::sync::Mutex<Option<PacePermit>>>;

/// Give back the turn in `slot`, if it is still there. Taking it out of the
/// slot is what makes the release happen once, whoever comes first.
pub(crate) fn release_turn(slot: &TurnSlot) {
    let permit = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
    drop(permit);
}

/// A progress line of the download (`rest` after `OMNIPROGRESS `), parsed. The
/// first one parsed means extraction is over and bytes are flowing: the turn
/// on the platform is given back then, not when the transfer ends (P11.4).
fn progress_line(rest: &str, turn: &TurnSlot) -> Option<DownloadProgress> {
    let p = parse_progress(rest)?;
    release_turn(turn);
    Some(p)
}

/// Run a yt-dlp process (`start`) whose target is `url`, paced (P11.4): on a
/// video platform the turn is taken immediately before the start, so the
/// process's own timeout does not count the wait, and held until the process
/// ends. With `early`, the turn is kept in that slot instead, and whoever
/// takes it out first gives it back: the download's first progress line
/// (the platform's bot check reacts to the burst of page and API requests at
/// the start of a run, not to the transfer), or the end of the process. The
/// wait ends early with `cancel`, and the run then reads as cancelled. A run
/// the platform refused as a rate limit holds every new start there for
/// [`RATE_LIMIT_COOL_DOWN`], whether or not it still held its turn. Any other
/// link starts at once.
///
/// `fail_fast` is for a caller that is not a job: during a cool-down with more
/// than [`crate::pacing::FAIL_FAST_COOL_DOWN_OVER`] left the run is not started
/// and comes back at once as a failure whose error line says the platform is
/// cooling down (RATE_LIMITED), instead of waiting against the caller's own
/// time limit. A job (`false`) waits the cool-down out.
///
/// Every yt-dlp start of this module goes through here.
pub(crate) async fn paced<F, Fut>(
    pacer: &HostPacer,
    url: &str,
    cancel: Option<&CancellationToken>,
    on_wait: Option<&WaitNote>,
    early: Option<&TurnSlot>,
    fail_fast: bool,
    start: F,
) -> Result<RunOutcome>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<RunOutcome>>,
{
    let Some(key) = platform_key(url) else {
        return start().await;
    };
    let never = CancellationToken::new();
    let acquired = if fail_fast {
        pacer.acquire_unless_cooling(key, cancel.unwrap_or(&never)).await
    } else {
        pacer.acquire(key, cancel.unwrap_or(&never)).await.map_err(|_| Refused::Cancelled)
    };
    let permit = match acquired {
        Ok(p) => p,
        Err(Refused::Cancelled) => return Ok(cancelled_before_start()),
        Err(Refused::CoolingDown(left)) => {
            info!("yt-dlp not started: {key} is cooling down for {} s", left.as_secs());
            return Ok(cooling_down(key, left));
        }
    };
    if permit.waited() > crate::pacing::NOTE_WAIT_OVER {
        info!("yt-dlp waited {} s for its turn on {key}", permit.waited().as_secs());
        if let Some(note) = on_wait {
            note(key, permit.waited());
        }
    }
    let held = match early {
        Some(slot) => {
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(permit);
            None
        }
        None => Some(permit),
    };
    // The slot's turn goes back when this run ends, or when it is dropped
    // (cancelled) first, if no progress line gave it back already.
    let give_back = GiveBackAtEnd(early);
    let outcome = start().await;
    if let Ok(o) = &outcome {
        if !o.success && !o.timed_out && !o.cancelled && classify_download_error(&o.stderr_tail) == ErrorCode::RateLimited {
            warn!("{key} is limiting our requests; no new yt-dlp start there for {} s", RATE_LIMIT_COOL_DOWN.as_secs());
            pacer.penalize(key, RATE_LIMIT_COOL_DOWN);
        }
    }
    drop(give_back);
    drop(held);
    outcome
}

/// Gives back the turn left in a download's slot when dropped.
struct GiveBackAtEnd<'a>(Option<&'a TurnSlot>);

impl Drop for GiveBackAtEnd<'_> {
    fn drop(&mut self) {
        if let Some(slot) = self.0 {
            release_turn(slot);
        }
    }
}

/// `line` without any of the jar's `values` (and without query strings, see
/// [`omni_core::cookies::scrub_detail`]); unchanged when there is no jar.
pub(crate) fn scrub_error(line: String, values: &[String]) -> String {
    if values.is_empty() {
        line
    } else {
        omni_core::cookies::scrub_detail(&line, values)
    }
}

/// `--cookies <path>` when a jar is given.
fn cookie_args(jar: Option<&Path>) -> Vec<String> {
    match jar {
        Some(p) => vec!["--cookies".into(), p.to_string_lossy().into_owned()],
        None => Vec::new(),
    }
}

/// `--js-runtimes deno:<path>`: yt-dlp only looks for Deno on PATH by
/// itself, and the service's PATH has no bin/.
fn js_runtime_args(deno: Option<&Path>) -> Vec<String> {
    match deno {
        Some(p) => vec!["--js-runtimes".into(), format!("deno:{}", p.display())],
        None => Vec::new(),
    }
}

/// The flags every yt-dlp run of an extraction, a probe or a download starts
/// with (P11.4e): no `yt-dlp.conf` and no plugin folder of the account's
/// profile changes what it does, and the merge uses the pinned ffmpeg, not
/// whichever one yt-dlp finds.
///
/// `--ffmpeg-location` is decided from the path's text alone (no disk access).
/// yt-dlp looks for `ffmpeg` inside a folder it is given, and checks that a
/// file it is given exists:
/// - a bare name with no folder part (`ffmpeg.exe`, found on PATH): no
///   location, yt-dlp finds it the way the encoder does;
/// - `…\ffmpeg.exe` or `…\ffmpeg` (any case): its folder;
/// - any other name: the path as it is, a renamed binary
///   (`ffmpeg-arm64.exe`) or a folder (`…\bin`, `…\ffmpeg-7.1`).
///
/// `None` leaves the location to yt-dlp.
pub fn hardening_args(ffmpeg: Option<&Path>) -> Vec<String> {
    let mut args = vec!["--ignore-config".to_string(), "--no-plugin-dirs".to_string()];
    if let Some(p) = ffmpeg {
        let dir = p.parent().filter(|d| !d.as_os_str().is_empty());
        let named_ffmpeg = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
            n.eq_ignore_ascii_case("ffmpeg.exe") || n.eq_ignore_ascii_case("ffmpeg")
        });
        let location = match dir {
            None => None,
            Some(dir) if named_ffmpeg => Some(dir),
            Some(_) => Some(p),
        };
        if let Some(location) = location {
            args.push("--ffmpeg-location".into());
            args.push(location.to_string_lossy().into_owned());
        }
    }
    args
}

impl Downloader {
    /// A downloader whose runs on a video platform wait for their turn in
    /// `pacer` (P11.4): the daemon's engine's, shared by everything that runs
    /// yt-dlp in the process. It is not a job's: a platform cooling down after
    /// a rate limit is reported at once ([`paced`]'s `fail_fast`), unless
    /// [`Self::for_a_job`] says otherwise.
    /// `ffmpeg` is the path the daemon resolved for its own ffmpeg (P11.4e).
    pub fn new<P: AsRef<Path>>(ytdl_path: P, ffmpeg: Option<&Path>, pacer: &HostPacer) -> Self {
        let ytdl_path = ytdl_path.as_ref().to_path_buf();
        // bin/deno.exe sits next to the legacy bin/yt-dlp.exe, and one level
        // up from the folder build's bin/yt-dlp/yt-dlp.exe (plan P2.10).
        let deno = ytdl_path
            .ancestors()
            .skip(1)
            .take(2)
            .map(|d| d.join("deno.exe"))
            .find(|p| p.is_file());
        Self {
            ytdl_path,
            deno,
            ffmpeg: ffmpeg.map(Path::to_path_buf),
            cookie_jar: None,
            cookie_values: Vec::new(),
            pacer: pacer.clone(),
            on_wait: None,
            fail_fast: true,
        }
    }

    /// A job's runs: they wait out a cool-down like any other wait for a turn
    /// (the job has no time limit of its own around them).
    pub fn for_a_job(mut self) -> Self {
        self.fail_fast = false;
        self
    }

    /// Tell `note` of a wait for a turn longer than two seconds (the job's history, P11.4).
    pub fn on_pace_wait(mut self, note: WaitNote) -> Self {
        self.on_wait = Some(note);
        self
    }

    /// One yt-dlp run at `url`, through [`paced`]; `early` as there (the
    /// download's slot, `None` for a probe, which holds its turn to the end).
    async fn run_ytdlp(
        &self,
        url: &str,
        args: Vec<String>,
        opts: RunOpts,
        cancel: Option<&CancellationToken>,
        early: Option<&TurnSlot>,
    ) -> Result<RunOutcome> {
        paced(&self.pacer, url, cancel, self.on_wait.as_ref(), early, self.fail_fast, || run(&self.ytdl_path, args, opts)).await
    }

    /// A probe's run, as `run_capture` runs it: stdout kept. The public probes
    /// have no token (their callers drop the future to cancel them, which also
    /// ends the wait); the download's own live probe passes the job's.
    async fn capture(&self, url: &str, args: Vec<String>, timeout: Duration, cancel: Option<&CancellationToken>) -> Result<RunOutcome> {
        let mut opts = RunOpts::new(timeout).capture_stdout();
        if let Some(token) = cancel {
            opts = opts.with_cancel(token.clone());
        }
        self.run_ytdlp(url, args, opts, cancel, None).await
    }

    /// Probes made through this value carry the jar file (plan P3.5). The caller
    /// keeps the file alive for the call and removes it after.
    pub fn with_cookie_jar(mut self, jar: Option<&crate::cookie_file::CookieFile>) -> Self {
        self.cookie_jar = jar.map(|j| j.path().to_path_buf());
        self.cookie_values = jar.map(|j| j.values().to_vec()).unwrap_or_default();
        self
    }

    /// A yt-dlp error line as it may be stored, logged or returned: with a jar
    /// in play, without its values.
    fn scrubbed(&self, line: String) -> String {
        scrub_error(line, &self.cookie_values)
    }

    /// Build the yt-dlp argument vector.
    ///
    /// Pure, so the format selection and the hardening flags are unit-testable
    /// without a network.
    pub fn build_args(url: &str, output_dir: &Path, opts: &DownloadOpts<'_>) -> Vec<String> {
        let h = opts.max_height;
        let mut args = hardening_args(opts.ffmpeg);
        args.extend::<Vec<String>>(vec![
            // Never pull 4K to downscale it to 1080 (defect D-18): on a news
            // deadline that is minutes of wasted download for no visible gain,
            // since the output is 1080i50 either way. Prefer H.264/AAC so the
            // merge is a remux rather than a re-encode.
            "-f".into(),
            format_selector(opts.formats, h),
            "--merge-output-format".into(),
            "mp4".into(),
            // A journalist's link often carries a playlist id; ingesting the
            // whole playlist would flood the watchfolder.
            "--no-playlist".into(),
            // --no-playlist does not stop a news page that yt-dlp's generic
            // extractor reads as a playlist of its embeds: an iefimerida.gr
            // article with four Streamable videos downloaded all four into
            // one job and delivered one of them. The page's other videos are
            // queued as their own jobs (`page_videos`).
            "--playlist-items".into(),
            "1".into(),
            // A live or upcoming video is skipped, not recorded: yt-dlp
            // downloads a live YouTube stream without error and the job sat in
            // DOWNLOAD until the timeout (protothema, 2026-10-17). `?` lets a
            // video whose live status is unknown (a plain mp4) through. A raw
            // HLS/mp4 is never marked live by the generic extractor, so this
            // does not guard those.
            "--match-filters".into(),
            LIVE_FILTER.into(),
            "--retries".into(),
            "5".into(),
            "--fragment-retries".into(),
            "10".into(),
            "--retry-sleep".into(),
            "exp=1:30".into(),
            "--socket-timeout".into(),
            "30".into(),
            "--concurrent-fragments".into(),
            opts.concurrent_fragments.to_string(),
            // Never resume a `.part` left by an earlier or cancelled run: the
            // same job may be retried with a different link (P1.15).
            "--no-continue".into(),
            // The file keeps its download time, not the server's Last-Modified.
            "--no-mtime".into(),
            // Nor take a complete `source.*` left in a kept workspace (a
            // restart or reap, then an override to a new link) as "already
            // downloaded": that would deliver the old link's video (P1.16).
            "--force-overwrites".into(),
            "--newline".into(),
            "--no-warnings".into(),
            "--no-color".into(),
            // Machine-readable progress instead of scraping the human format.
            "--progress-template".into(),
            "download:OMNIPROGRESS %(progress.downloaded_bytes)s %(progress.total_bytes_estimate)s \
             %(progress.speed)s %(progress.eta)s"
                .into(),
            // The definitive output path, instead of guessing by scanning the
            // directory for a name we hope matches (defect D-18).
            "--print".into(),
            "after_move:filepath".into(),
            "-o".into(),
            output_dir
                .join("source.%(ext)s")
                .to_string_lossy()
                .into_owned(),
        ]);

        if opts.insecure_tls {
            args.push("--no-check-certificates".into());
        }
        args.extend(js_runtime_args(opts.js_runtime));
        // The article page's session (referer, browser UA, its cookies) is for
        // raw streams on the portal's own CDN. A video platform the sniffer
        // found embedded in the article is fetched by yt-dlp's own extractor,
        // and the platform rejects a foreign referer: X's video CDN answers
        // 403 to an x.com post fetched "from" a news article.
        if !is_video_platform(url) {
            if let Some(r) = opts.referer {
                args.push("--referer".into());
                args.push(r.to_string());
            }
            if let Some(ua) = opts.user_agent {
                args.push("--user-agent".into());
                args.push(ua.to_string());
            }
            if let Some(c) = opts.cookie_header {
                args.push("--add-header".into());
                args.push(format!("Cookie: {c}"));
            }
        }
        if let Some(jar) = opts.cookie_jar {
            args.push("--cookies".into());
            args.push(jar.to_string_lossy().into_owned());
        }

        args.push(url.to_string());
        args
    }

    /// `opts` with this downloader's Deno and ffmpeg where the caller left them empty.
    fn own_tools<'a>(&'a self, mut opts: DownloadOpts<'a>) -> DownloadOpts<'a> {
        if opts.js_runtime.is_none() {
            opts.js_runtime = self.deno.as_deref();
        }
        if opts.ffmpeg.is_none() {
            opts.ffmpeg = self.ffmpeg.as_deref();
        }
        opts
    }

    /// Download `url` into `output_dir`, reporting progress.
    ///
    /// Returns the downloaded file, or a [`DownloadError`] carrying the
    /// classified reason.
    pub async fn download<F>(
        &self,
        job_id: i64,
        url: &str,
        output_dir: &Path,
        opts: DownloadOpts<'_>,
        mut on_progress: F,
    ) -> Result<PathBuf>
    where
        F: FnMut(DownloadProgress) + Send + 'static,
    {
        tokio::fs::create_dir_all(output_dir).await?;
        let opts = self.own_tools(opts);
        let args = Self::build_args(url, output_dir, &opts);

        info!("Job #{job_id}: downloading {}", omni_core::cookies::host_and_path(url));

        // yt-dlp prints the final path on stdout as well as progress lines, so
        // both are collected from the same stream.
        let printed_paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = printed_paths.clone();
        // The turn on the platform (P11.4), given back at the first progress
        // line: extraction is over and the transfer has begun.
        let turn: TurnSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let first_progress = turn.clone();

        let mut run_opts = RunOpts::new(opts.timeout).on_stdout_line(move |line| {
            if let Some(rest) = line.strip_prefix("OMNIPROGRESS ") {
                if let Some(p) = progress_line(rest, &first_progress) {
                    on_progress(p);
                }
            } else if !line.trim().is_empty() {
                sink.lock().unwrap().push(line.trim().to_string());
            }
        });
        if let Some(token) = opts.cancel.clone() {
            run_opts = run_opts.with_cancel(token);
        }

        // The workspace is this job's own; a `source.*` left by an earlier
        // owner must not be mistaken for this run's file by the fallback scan.
        if let Err(e) = clear_stale_sources(output_dir).await {
            return Err(DownloadError {
                code: ErrorCode::PipelineFailed,
                message: format!("could not clear a leftover source file in {output_dir:?}: {e}"),
            }
            .into());
        }
        let outcome = self
            .run_ytdlp(url, args, run_opts, opts.cancel.as_ref(), Some(&turn))
            .await
            .with_context(|| format!("Could not run yt-dlp at {:?}", self.ytdl_path))?;

        if !outcome.success {
            let code = if outcome.timed_out {
                ErrorCode::DownloadTimeout
            } else if outcome.cancelled {
                ErrorCode::PipelineFailed
            } else {
                classify_download_error(&outcome.stderr_tail)
            };
            return Err(DownloadError {
                code,
                message: scrub_error(first_error_line(&outcome.stderr_tail), opts.cookie_values),
            }
            .into());
        }

        // `--print after_move:filepath` gives the real path. Fall back to a
        // directory scan only if that produced nothing usable, because a wrong
        // guess here feeds the wrong file to the transcoder.
        let candidates = printed_paths.lock().unwrap().clone();
        if let Some(p) = printed_path_in_dir(&candidates, output_dir) {
            return Ok(p);
        }
        warn!("Job #{job_id}: yt-dlp did not print a usable output path; scanning {output_dir:?}");
        if let Some(p) = find_downloaded_file(output_dir).await {
            return Ok(p);
        }
        // Exit 0 and nothing written is what the live filter leaves behind
        // (`--print` keeps its skip message quiet): ask yt-dlp whether the
        // video is live or upcoming. Same jar as the download; the probe takes
        // no referer or user agent.
        let prober = Downloader {
            ytdl_path: self.ytdl_path.clone(),
            deno: self.deno.clone(),
            ffmpeg: opts.ffmpeg.map(Path::to_path_buf),
            cookie_jar: opts.cookie_jar.map(Path::to_path_buf).or_else(|| self.cookie_jar.clone()),
            cookie_values: opts.cookie_values.to_vec(),
            pacer: self.pacer.clone(),
            on_wait: self.on_wait.clone(),
            fail_fast: self.fail_fast,
        };
        // Its wait for a turn ends with the job's token, like the download's.
        let live = empty_download_is_live(&candidates, || async {
            prober.probe_media_until(url, Duration::from_secs(15), opts.cancel.as_ref()).await.ok().map(|i| i.live)
        })
        .await;
        if live {
            return Err(DownloadError {
                code: ErrorCode::LiveStream,
                message: "the video is live or not yet started; yt-dlp skipped it".to_string(),
            }
            .into());
        }
        Err(anyhow!("yt-dlp reported success but produced no file in {output_dir:?}"))
    }
}

/// The videos yt-dlp sees on a page (`--flat-playlist -J`), in page order,
/// when it reads the page as several; empty for a single video or anything
/// else. Only http(s) entry URLs are kept, each once.
pub fn parse_page_videos(json: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    if v.get("_type").and_then(|t| t.as_str()) != Some("playlist") {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for e in v.get("entries").and_then(|e| e.as_array()).into_iter().flatten() {
        let Some(u) = e.get("url").or_else(|| e.get("webpage_url")).and_then(|u| u.as_str()) else {
            continue;
        };
        if !is_http_url(u) {
            continue;
        }
        let u = canonical_streamable(u).unwrap_or_else(|| u.to_string());
        if !out.contains(&u) {
            out.push(u);
        }
    }
    out
}

/// `https://streamable.com/ID` for the player forms (`/e/ID`, `/o/ID`,
/// `/s/ID`), the form the sniffer reports too, so one video found by both
/// is one job.
pub fn canonical_streamable(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let path = rest.strip_prefix("streamable.com/")?;
    let mut parts = path.split(['/', '?', '#']).filter(|p| !p.is_empty());
    let first = parts.next()?;
    let id = if matches!(first, "e" | "o" | "s") { parts.next()? } else { first };
    id.chars().all(|c| c.is_ascii_alphanumeric()).then(|| format!("https://streamable.com/{id}"))
}

/// What a probe of a link found: the extractor and the video id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probed {
    pub extractor: String,
    pub id: String,
}

/// `--print "%(extractor)s %(id)s"` output: the first non-empty line.
pub fn parse_probe_line(stdout: &str) -> Option<Probed> {
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty())?;
    let (extractor, id) = line.split_once(' ')?;
    let id = id.trim();
    (!extractor.is_empty() && !id.is_empty() && id != "NA").then(|| Probed { extractor: extractor.to_string(), id: id.to_string() })
}

impl Downloader {
    /// Whether yt-dlp can still get the video at `url`, without downloading
    /// it (`--simulate`: the page is read and the formats are chosen). The
    /// self-check (plan P6.7) uses this. `Err` is the one-line reason.
    pub async fn probe(&self, url: &str, timeout: Duration) -> std::result::Result<Probed, String> {
        let args = self.probe_args(url);
        match self.capture(url, args, timeout, None).await {
            Ok(o) if o.success => parse_probe_line(&o.stdout).ok_or_else(|| "yt-dlp found no video".to_string()),
            Ok(o) if o.timed_out => Err(format!("yt-dlp did not answer within {} s", timeout.as_secs())),
            Ok(o) => Err(self.scrubbed(first_error_line(&o.stderr_tail))),
            Err(e) => Err(format!("could not run yt-dlp: {e:#}")),
        }
    }

    /// [`Self::probe`]'s arguments.
    fn probe_args(&self, url: &str) -> Vec<String> {
        let mut args = hardening_args(self.ffmpeg.as_deref());
        args.extend(
            [
                "--simulate",
                "--no-playlist",
                "--playlist-items",
                "1",
                "--no-warnings",
                "--socket-timeout",
                "30",
                "--print",
                "%(extractor)s %(id)s",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        args.extend(js_runtime_args(self.deno.as_deref()));
        args.extend(cookie_args(self.cookie_jar.as_deref()));
        args.push(url.to_string());
        args
    }

    /// The videos on a news page, when yt-dlp reads it as several (see
    /// [`parse_page_videos`]), and whether yt-dlp said it has no extractor
    /// for the page at all. Any other failure is no videos: the download
    /// that follows reports it properly.
    pub async fn page_videos(&self, url: &str, timeout: Duration) -> PageScan {
        let args = self.page_videos_args(url);
        match self.capture(url, args, timeout, None).await {
            Ok(o) if o.success => {
                let json = serde_json::from_str::<serde_json::Value>(&o.stdout).ok();
                let why = json.as_ref().and_then(|v| generic_why(v, url));
                PageScan {
                    videos: parse_page_videos(&o.stdout),
                    unsupported: false,
                    generic_only: why.is_some(),
                    hand_off_only: why == Some(GenericWhy::HandOff),
                    webpage_url: json.as_ref().and_then(|v| v.get("webpage_url")).and_then(|v| v.as_str()).map(str::to_string),
                    media_urls: json.as_ref().map(|v| parse_scan_media_urls(v, url)).unwrap_or_default(),
                    fetch_urls: json.as_ref().map(parse_scan_fetch_urls).unwrap_or_default(),
                }
            }
            Ok(o) if !o.timed_out => PageScan {
                videos: Vec::new(),
                unsupported: crate::errors::classify_download_error(&o.stderr_tail) == ErrorCode::UnsupportedUrl,
                ..Default::default()
            },
            _ => PageScan::default(),
        }
    }

    /// [`Self::page_videos`]'s arguments.
    fn page_videos_args(&self, url: &str) -> Vec<String> {
        let mut args = hardening_args(self.ffmpeg.as_deref());
        args.extend(["--flat-playlist", "-J", "--no-warnings", "--socket-timeout", "30"].iter().map(|s| s.to_string()));
        args.extend(js_runtime_args(self.deno.as_deref()));
        args.extend(cookie_args(self.cookie_jar.as_deref()));
        args.push(url.to_string());
        args
    }
}

/// What yt-dlp says about the video at a link, without downloading it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlatformInfo {
    pub duration_secs: Option<f64>,
    pub height: Option<u32>,
    pub live: bool,
}

/// `--print "%(duration)s|%(height)s|%(is_live)s|%(live_status)s"`: the first
/// non-empty line. A field yt-dlp does not know is `NA`.
pub fn parse_platform_line(stdout: &str) -> Option<PlatformInfo> {
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty())?;
    let f: Vec<&str> = line.split('|').map(str::trim).collect();
    if f.len() != 4 {
        return None;
    }
    fn known(s: &str) -> Option<&str> {
        (s != "NA" && !s.is_empty()).then_some(s)
    }
    Some(PlatformInfo {
        duration_secs: known(f[0]).and_then(|s| s.parse::<f64>().ok()).filter(|d| d.is_finite()),
        height: known(f[1]).and_then(|s| s.parse::<f64>().ok()).filter(|h| *h >= 0.0).map(|h| h as u32),
        live: f[2] == "True" || matches!(f[3], "is_live" | "is_upcoming"),
    })
}

impl Downloader {
    /// Length, height and live flag of the video at `url` (a page or platform
    /// link), without downloading it. `Err` is the one-line reason.
    pub async fn probe_media(&self, url: &str, timeout: Duration) -> std::result::Result<PlatformInfo, String> {
        self.probe_media_until(url, timeout, None).await
    }

    /// [`Self::probe_media`], its wait for a turn and its run ended by `cancel`.
    async fn probe_media_until(
        &self,
        url: &str,
        timeout: Duration,
        cancel: Option<&CancellationToken>,
    ) -> std::result::Result<PlatformInfo, String> {
        let args = self.probe_media_args(url);
        match self.capture(url, args, timeout, cancel).await {
            Ok(o) if o.success => parse_platform_line(&o.stdout).ok_or_else(|| "yt-dlp found no video".to_string()),
            Ok(o) if o.timed_out => Err(format!("yt-dlp did not answer within {} s", timeout.as_secs())),
            Ok(o) => Err(self.scrubbed(first_error_line(&o.stderr_tail))),
            Err(e) => Err(format!("could not run yt-dlp: {e:#}")),
        }
    }

    /// [`Self::probe_media_until`]'s arguments.
    fn probe_media_args(&self, url: &str) -> Vec<String> {
        let mut args = hardening_args(self.ffmpeg.as_deref());
        args.extend(
            [
                "--simulate",
                "--no-playlist",
                "--playlist-items",
                "1",
                "--no-warnings",
                "--socket-timeout",
                "15",
                "--print",
                "%(duration)s|%(height)s|%(is_live)s|%(live_status)s",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        args.extend(js_runtime_args(self.deno.as_deref()));
        args.extend(cookie_args(self.cookie_jar.as_deref()));
        args.push(url.to_string());
        args
    }
}

impl Downloader {
    /// What yt-dlp says about one of the sniffer's candidates, without downloading it (P3.15):
    /// live, gone (a dead-candidate code from its error), readable, or unknown. The sniffer's
    /// referer, user agent and cookies go along for a stream that is not a platform's, as in the
    /// download itself.
    pub async fn probe_candidate(
        &self,
        url: &str,
        referer: Option<&str>,
        user_agent: Option<&str>,
        cookie_header: Option<&str>,
        timeout: Duration,
    ) -> Probe {
        let args = self.probe_candidate_args(url, referer, user_agent, cookie_header);
        match self.capture(url, args, timeout, None).await {
            Ok(o) if o.success => Probe::from_length(parse_platform_line(&o.stdout)),
            Ok(o) if o.timed_out => Probe::Unknown,
            Ok(o) => {
                let code = crate::errors::classify_download_error(&o.stderr_tail);
                if is_dead_candidate(code) { Probe::Dead(code) } else { Probe::Unknown }
            }
            Err(_) => Probe::Unknown,
        }
    }

    /// [`Self::probe_candidate`]'s arguments.
    fn probe_candidate_args(&self, url: &str, referer: Option<&str>, user_agent: Option<&str>, cookie_header: Option<&str>) -> Vec<String> {
        let mut args = hardening_args(self.ffmpeg.as_deref());
        args.extend(
            [
                "--simulate",
                "--no-playlist",
                "--playlist-items",
                "1",
                "--no-warnings",
                "--socket-timeout",
                "15",
                "--print",
                "%(duration)s|%(height)s|%(is_live)s|%(live_status)s",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        args.extend(js_runtime_args(self.deno.as_deref()));
        if !is_video_platform(url) {
            if let Some(r) = referer {
                args.push("--referer".into());
                args.push(r.to_string());
            }
            if let Some(ua) = user_agent {
                args.push("--user-agent".into());
                args.push(ua.to_string());
            }
            if let Some(c) = cookie_header {
                args.push("--add-header".into());
                args.push(format!("Cookie: {c}"));
            }
        }
        args.extend(cookie_args(self.cookie_jar.as_deref()));
        args.push(url.to_string());
        args
    }
}

/// What yt-dlp says about a link, for the admin's Test URL (P10.15b).
#[derive(Debug, Clone, PartialEq)]
pub struct LinkInfo {
    pub extractor: String,
    pub title: Option<String>,
    pub duration_secs: Option<f64>,
    pub formats: usize,
}

/// `--print "%(extractor)s|%(duration)s|%(formats.:.format_id)j|%(title)s"`: the first
/// non-empty line. The title is last, so a `|` in it stays in it.
pub fn parse_info_line(stdout: &str) -> Option<LinkInfo> {
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty())?;
    let f: Vec<&str> = line.splitn(4, '|').collect();
    if f.len() != 4 || f[0].is_empty() || f[0] == "NA" {
        return None;
    }
    let known = |s: &str| (s != "NA" && !s.is_empty()).then(|| s.to_string());
    Some(LinkInfo {
        extractor: f[0].to_string(),
        title: known(f[3].trim()).map(|t| t.chars().take(200).collect()),
        duration_secs: known(f[1].trim()).and_then(|s| s.parse::<f64>().ok()).filter(|d| d.is_finite() && *d >= 0.0),
        formats: serde_json::from_str::<Vec<serde_json::Value>>(f[2].trim()).map(|v| v.len()).unwrap_or(0),
    })
}

impl Downloader {
    /// Extractor, title, length and number of formats of the video at `url`, without
    /// downloading it (`--simulate`). `Err` is the one-line reason.
    pub async fn probe_info(&self, url: &str, timeout: Duration) -> std::result::Result<LinkInfo, String> {
        let args = self.probe_info_args(url);
        match self.capture(url, args, timeout, None).await {
            Ok(o) if o.success => parse_info_line(&o.stdout).ok_or_else(|| "yt-dlp found no video".to_string()),
            Ok(o) if o.timed_out => Err(format!("yt-dlp did not answer within {} s", timeout.as_secs())),
            Ok(o) => Err(self.scrubbed(first_error_line(&o.stderr_tail))),
            Err(e) => Err(format!("could not run yt-dlp: {e:#}")),
        }
    }

    /// [`Self::probe_info`]'s arguments.
    fn probe_info_args(&self, url: &str) -> Vec<String> {
        let mut args = hardening_args(self.ffmpeg.as_deref());
        args.extend(
            [
                "--simulate",
                "--no-playlist",
                "--playlist-items",
                "1",
                "--no-warnings",
                "--socket-timeout",
                "15",
                "--print",
                "%(extractor)s|%(duration)s|%(formats.:.format_id)j|%(title)s",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        args.extend(js_runtime_args(self.deno.as_deref()));
        args.extend(cookie_args(self.cookie_jar.as_deref()));
        args.push(url.to_string());
        args
    }
}

/// What `--flat-playlist -J` made of a page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageScan {
    /// The page's videos when it holds several, in page order.
    pub videos: Vec<String>,
    /// yt-dlp has no extractor for the page ("Unsupported URL"): a download
    /// attempt would only read the page again to say the same, so the
    /// caller can go straight to the sniffer.
    pub unsupported: bool,
    /// yt-dlp reaches the page's video only through its generic page scan (P3.15):
    /// see [`generic_only`]. Never set with `unsupported`, or when the scan failed.
    pub generic_only: bool,
    /// `generic_only` fired only because the scan's `webpage_url` is on another site (a
    /// hand-off to an embed's extractor), not because the generic or html5 extractor
    /// answered. A redirect to a post looks the same, so the caller checks where the page
    /// really is ([`first_attempt_resolved`]).
    pub hand_off_only: bool,
    /// The scan's own `webpage_url`.
    pub webpage_url: Option<String>,
    /// The address(es) of the one video the scan read, when it is not a playlist (P3.16):
    /// its media `url`, its formats' and the scan's `webpage_url` when that is another address
    /// than the page. What yt-dlp's own pick would download, for the site-wide check.
    pub media_urls: Vec<String>,
    /// Every address the single video's result could make yt-dlp fetch ([`parse_scan_fetch_urls`]),
    /// uncapped: what the egress screen judges before yt-dlp reads the page (P2.12).
    pub fetch_urls: Vec<String>,
}

/// At most this many format addresses of a single-video scan are kept.
const MAX_SCAN_MEDIA_URLS: usize = 8;

/// The addresses a single-video `-J` result names (see [`PageScan::media_urls`]); empty for a
/// playlist, whose entries are the page's videos.
pub fn parse_scan_media_urls(scan: &serde_json::Value, page_url: &str) -> Vec<String> {
    if scan.get("_type").and_then(|t| t.as_str()) == Some("playlist") {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    // The scan's `webpage_url` first and outside the cap: for a hand-off it is the form the browser
    // records (a `watch?v=` address), and a long format list must not crowd it out.
    if let Some(w) = scan.get("webpage_url").and_then(|u| u.as_str()) {
        if is_http_url(w) && omni_core::urlnorm::normalize(w) != omni_core::urlnorm::normalize(page_url) {
            out.push(w.to_string());
        }
    }
    let reserved = out.len();
    let mut add = |u: &str| {
        if is_http_url(u) && out.len() < reserved + MAX_SCAN_MEDIA_URLS && !out.iter().any(|o| o == u) {
            out.push(u.to_string());
        }
    };
    if let Some(u) = scan.get("url").and_then(|u| u.as_str()) {
        add(u);
    }
    for f in scan.get("formats").and_then(|f| f.as_array()).into_iter().flatten() {
        if let Some(u) = f.get("url").and_then(|u| u.as_str()) {
            add(u);
        }
    }
    out
}

/// An http(s) address, however its scheme is written (`HTTP://` too: yt-dlp reads it alike).
pub fn is_http_url(u: &str) -> bool {
    let has = |p: &str| u.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p));
    has("http://") || has("https://")
}

/// Every address a single-video `-J` result could make yt-dlp fetch, for the egress screen (P2.12):
/// the scan's `webpage_url` (a hand-off), its `url`, `manifest_url` and `fragment_base_url`, and
/// the same of every format and requested format. Uncapped, unlike [`parse_scan_media_urls`]:
/// yt-dlp's pick is often the last format. Another protocol (`rtmp://host/…`) is given as
/// `http://host/…` so its host is judged; a value with no scheme (a relative fragment path) is
/// not an address. Empty for a playlist, whose entries are the page's videos.
pub fn parse_scan_fetch_urls(scan: &serde_json::Value) -> Vec<String> {
    if scan.get("_type").and_then(|t| t.as_str()) == Some("playlist") {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut add = |v: &serde_json::Value| {
        for key in ["url", "manifest_url", "fragment_base_url"] {
            let Some(u) = v.get(key).and_then(|u| u.as_str()).map(str::trim) else {
                continue;
            };
            let u = match u.find("://") {
                _ if is_http_url(u) => u.to_string(),
                Some(i) if i > 0 && u[..i].chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) => format!("http://{}", &u[i + 3..]),
                _ => continue,
            };
            if !out.contains(&u) {
                out.push(u);
            }
        }
    };
    add(scan);
    for list in ["formats", "requested_formats"] {
        for f in scan.get(list).and_then(|f| f.as_array()).into_iter().flatten() {
            add(f);
        }
    }
    if let Some(w) = scan.get("webpage_url").and_then(|u| u.as_str()) {
        if is_http_url(w) && !out.iter().any(|o| o == w) {
            out.push(w.to_string());
        }
    }
    out
}

/// Why [`generic_why`] says a page is read only generically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenericWhy {
    /// The generic or html5 extractor answered (or a `webpage_url` no dedicated extractor of an
    /// http page would report).
    Extractor,
    /// `webpage_url` is on another registrable domain than the page's.
    HandOff,
}

/// Whether a `--flat-playlist -J` result of `page_url` came from yt-dlp's generic
/// scan rather than an extractor for the page's own site: the generic or html5
/// extractor itself (a site-wide promo `<video>` is taken before the article's
/// player), or a hand-off from it to another site's extractor (the first YouTube
/// embed of the page, with no idea where on the page it sat). A dedicated
/// extractor on the page's own registrable domain is not.
pub fn generic_only(scan: &serde_json::Value, page_url: &str) -> bool {
    generic_why(scan, page_url).is_some()
}

/// Whether a failed download or probe says the candidate itself is gone, so another of the
/// page's videos may be the right one (P3.15). Retryable codes, a login or geo wall and a live
/// stream say nothing about the page's other videos.
pub fn is_dead_candidate(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::PageNotFound | ErrorCode::PrivateOrRemoved | ErrorCode::UnsupportedUrl | ErrorCode::NoStreamFound)
}

/// Whether the browser's final page address is a video platform's (a meta refresh or script
/// redirect to a post): the browser's choice is not taken then, yt-dlp follows it (P3.15).
pub fn landed_on_platform(final_url: Option<&str>) -> bool {
    final_url.is_some_and(is_video_platform)
}

/// Whether the scan shows the page address is itself a media file or stream (a direct
/// `.mp4` or `.m3u8`): yt-dlp's generic extractor reads it as a video of its own, and the
/// browser has no page to choose from.
fn direct_media(scan: &serde_json::Value, page_url: &str) -> bool {
    if scan.get("direct").and_then(|v| v.as_bool()) == Some(true) {
        return true;
    }
    if scan.get("webpage_url").and_then(|v| v.as_str()) != Some(page_url) {
        return false;
    }
    let path = page_url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    [".m3u8", ".mpd", ".mp4", ".m4v", ".mov", ".webm", ".ts", ".mkv"].iter().any(|e| path.ends_with(e))
}

/// [`generic_only`], with the reason it fired. A direct media address is never generic only.
/// Known limit: a dedicated extractor that itself delegates to another domain's extractor
/// (its `webpage_url` off the page's site) reads as a hand-off; the redirect check in
/// [`first_attempt_resolved`] is the only guard.
pub fn generic_why(scan: &serde_json::Value, page_url: &str) -> Option<GenericWhy> {
    if direct_media(scan, page_url) {
        return None;
    }
    let named = |key: &str, names: &[&str]| {
        scan.get(key).and_then(|v| v.as_str()).is_some_and(|s| names.iter().any(|n| s.eq_ignore_ascii_case(n)))
    };
    if named("extractor_key", &["generic", "html5mediaembed"]) || named("extractor", &["generic", "html5"]) {
        return Some(GenericWhy::Extractor);
    }
    let landed = scan.get("webpage_url").and_then(|v| v.as_str())?;
    // A dedicated extractor called on an http page never reports a webpage_url that is not
    // http(s) (an id such as `kaltura:...`): that came from a hand-off.
    if !(landed.starts_with("http://") || landed.starts_with("https://")) {
        return Some(GenericWhy::Extractor);
    }
    match (omni_core::urlnorm::registrable_domain(page_url), omni_core::urlnorm::registrable_domain(landed)) {
        (Some(page), Some(landed)) if page != landed => Some(GenericWhy::HandOff),
        _ => None,
    }
}

/// Who takes a web page's first attempt (P3.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstAttempt {
    /// yt-dlp downloads the page (or its page-scan pick), as before.
    YtDlp,
    /// yt-dlp has no extractor for the page: straight to the browser.
    SnifferOnly,
    /// yt-dlp reads the page only generically: the browser chooses the video,
    /// and yt-dlp's own pick is the fallback when it finds none.
    SnifferFirst,
}

/// [`first_attempt`], for a hand-off that only the cross-domain rule flagged: a shortener or
/// redirect to a platform post (bit.ly to a YouTube watch page) looks the same as a news
/// page's embed, and yt-dlp downloads the post itself, so `final_url` (where `page_url` really
/// ends up after redirects) decides. A final address that is a video platform or on the scan's
/// own `webpage_url` domain is a redirect to the post: yt-dlp. No answer: yt-dlp. A page that
/// stays on another, non-platform site: the browser.
pub async fn first_attempt_resolved<F, Fut>(
    site_api_hit: bool,
    is_platform: bool,
    is_sibling: bool,
    scan: &PageScan,
    page_url: &str,
    final_url: F,
) -> FirstAttempt
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let first = first_attempt(site_api_hit, is_platform, is_sibling, scan);
    if first != FirstAttempt::SnifferFirst || !scan.hand_off_only {
        return first;
    }
    let Some(landed) = final_url(page_url.to_string()).await else {
        return FirstAttempt::YtDlp;
    };
    if is_video_platform(&landed) {
        return FirstAttempt::YtDlp;
    }
    let same_as_scan = match (
        omni_core::urlnorm::registrable_domain(&landed),
        scan.webpage_url.as_deref().and_then(omni_core::urlnorm::registrable_domain),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    if same_as_scan {
        FirstAttempt::YtDlp
    } else {
        FirstAttempt::SnifferFirst
    }
}

/// The first attempt for a page. A link the site's API named, a video platform's own, or an
/// article sibling's (its parent already chose the address) never goes to the sniffer first;
/// otherwise the page scan decides.
pub fn first_attempt(site_api_hit: bool, is_platform: bool, is_sibling: bool, scan: &PageScan) -> FirstAttempt {
    if site_api_hit || is_platform {
        FirstAttempt::YtDlp
    } else if scan.unsupported {
        FirstAttempt::SnifferOnly
    } else if scan.generic_only && !is_sibling {
        FirstAttempt::SnifferFirst
    } else {
        FirstAttempt::YtDlp
    }
}

/// `downloaded total speed eta`, with `NA` for values yt-dlp does not know yet.
fn parse_progress(rest: &str) -> Option<DownloadProgress> {
    let f: Vec<&str> = rest.split_whitespace().collect();
    if f.len() < 4 {
        return None;
    }
    let downloaded: f64 = f[0].parse().ok()?;
    let total: f64 = f[1].parse().unwrap_or(0.0);
    let percent = if total > 0.0 {
        ((downloaded / total) * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let speed = match f[2].parse::<f64>() {
        Ok(bps) if bps > 0.0 => format!("{:.1} MB/s", bps / 1_048_576.0),
        _ => "--".to_string(),
    };
    let eta = match f[3].parse::<f64>() {
        Ok(s) if s > 0.0 => format!("{:02}:{:02}", (s as u64) / 60, (s as u64) % 60),
        _ => "--:--".to_string(),
    };
    Some(DownloadProgress {
        percent,
        speed,
        eta,
    })
}

/// The last printed line that names a file inside `output_dir`. A path that
/// resolves elsewhere is refused: it is not this run's download.
fn printed_path_in_dir(lines: &[String], output_dir: &Path) -> Option<PathBuf> {
    let root = output_dir.canonicalize();
    lines.iter().rev().find_map(|line| {
        let p = PathBuf::from(line);
        if !p.is_file() {
            return None;
        }
        let inside = match (p.canonicalize(), &root) {
            (Ok(real), Ok(root)) => real.starts_with(root),
            _ => lexically_directly_inside(&p, output_dir),
        };
        inside.then_some(p)
    })
}

/// Fallback when a path cannot be canonicalised: absolute, and its parent is
/// exactly `output_dir`.
fn lexically_directly_inside(p: &Path, output_dir: &Path) -> bool {
    p.is_absolute() && p.parent() == Some(output_dir)
}

/// Remove every `source.*` file left in `dir` by an earlier owner of the job,
/// so the fallback scan can only ever see this run's output. A file that
/// cannot be removed is an error: running on with it present is the bug.
async fn clear_stale_sources(dir: &Path) -> std::io::Result<()> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_name().to_string_lossy().to_lowercase().starts_with("source.") {
            continue;
        }
        if entry.metadata().await?.is_file() {
            tokio::fs::remove_file(entry.path()).await?;
        }
    }
    Ok(())
}

/// The largest complete `source.*` file in the job's workspace.
///
/// Only used when yt-dlp printed no path, after [`clear_stale_sources`] ran
/// before the spawn. `.part*` and `.ytdl` are in-progress artefacts; picking
/// one would hand a truncated file to the transcoder. No mtime test: yt-dlp may
/// stamp a fresh file with the server's date.
pub(crate) async fn find_downloaded_file(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    let mut entries = tokio::fs::read_dir(dir).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if !name.starts_with("source.")
            || name.contains(".part")
            || name.ends_with(".ytdl")
            || name.ends_with(".tmp")
            || name.ends_with(".temp")
        {
            continue;
        }
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if best.as_ref().map_or(true, |(size, _)| meta.len() > *size) {
            best = Some((meta.len(), path));
        }
    }
    best.map(|(_, p)| p)
}

/// The first `ERROR:` line, which is the one worth showing an operator.
pub(crate) fn first_error_line(stderr: &str) -> String {
    stderr
        .lines()
        .find(|l| l.trim_start().to_uppercase().starts_with("ERROR"))
        .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("yt-dlp failed with no diagnostic output")
        .trim()
        .chars()
        .take(500)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn scan_json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn a_page_read_by_the_html5_extractor_is_generic_only() {
        let v = scan_json(r#"{"_type":"video","extractor":"html5","extractor_key":"HTML5MediaEmbed","webpage_url":"https://www.example-news.gr/article/1"}"#);
        assert!(generic_only(&v, "https://www.example-news.gr/article/1"));
    }

    #[test]
    fn a_youtube_hand_off_from_a_news_page_is_generic_only() {
        let v = scan_json(r#"{"_type":"video","extractor":"youtube","extractor_key":"Youtube","webpage_url":"https://www.youtube.com/watch?v=abcdefghijk"}"#);
        assert!(generic_only(&v, "https://www.example-news.gr/story"));
    }

    #[test]
    fn an_embed_extractor_on_another_domain_is_generic_only() {
        let v = scan_json(r#"{"_type":"video","extractor":"ertwebtv:embed","extractor_key":"ERTWebtvEmbed","webpage_url":"https://www.ert.gr/webtv/x"}"#);
        assert!(generic_only(&v, "https://www.ertnews.gr/video/x"));
    }

    #[test]
    fn a_generic_playlist_is_generic_only() {
        let v = scan_json(r#"{"_type":"playlist","extractor":"generic","extractor_key":"Generic","webpage_url":"https://www.example-news.gr/a","entries":[{"url":"https://streamable.com/aaaa"}]}"#);
        assert!(generic_only(&v, "https://www.example-news.gr/a"));
    }

    #[test]
    fn a_direct_media_address_is_not_generic_only() {
        let direct = scan_json(r#"{"_type":"video","extractor":"generic","extractor_key":"Generic","direct":true,"webpage_url":"https://media.example-news.gr/clip.mp4","ext":"mp4"}"#);
        assert!(!generic_only(&direct, "https://media.example-news.gr/clip.mp4"));
        let stream = scan_json(r#"{"_type":"video","extractor":"generic","extractor_key":"Generic","webpage_url":"https://streams.example.test/x/index.M3U8?t=1"}"#);
        assert!(!generic_only(&stream, "https://streams.example.test/x/index.M3U8?t=1"));
        // A media extension on a different address than the page is not the page itself.
        let hand = scan_json(r#"{"extractor":"generic","extractor_key":"Generic","webpage_url":"https://x.example.test/a.mp4"}"#);
        assert!(generic_only(&hand, "https://www.example-news.gr/article"));
        // The html5 article control.
        let html5 = scan_json(r#"{"extractor":"html5","extractor_key":"HTML5MediaEmbed","webpage_url":"https://www.example-news.gr/article"}"#);
        assert!(generic_only(&html5, "https://www.example-news.gr/article"));
    }

    #[test]
    fn a_single_video_scan_names_the_addresses_yt_dlp_would_download() {
        let page = "https://www.example-news.gr/article";
        let html5 = scan_json(
            r#"{"extractor":"html5","webpage_url":"https://www.example-news.gr/article","url":"https://media.example-news.gr/promo.mp4","formats":[{"url":"https://media.example-news.gr/promo-hd.mp4"},{"url":"https://media.example-news.gr/promo.mp4"},{"url":"blob:x"}]}"#,
        );
        assert_eq!(
            parse_scan_media_urls(&html5, page),
            ["https://media.example-news.gr/promo.mp4", "https://media.example-news.gr/promo-hd.mp4"]
        );
        // The webpage address leads and is never crowded out by a long format list.
        let many: Vec<String> = (0..12).map(|i| format!(r#"{{"url":"https://cdn.example-news.gr/f{i}.m3u8"}}"#)).collect();
        let long = scan_json(&format!(
            r#"{{"extractor":"youtube","webpage_url":"https://www.youtube.com/watch?v=xZf1yNC3YDQ","formats":[{}]}}"#,
            many.join(",")
        ));
        let got = parse_scan_media_urls(&long, page);
        assert_eq!(got[0], "https://www.youtube.com/watch?v=xZf1yNC3YDQ");
        assert_eq!(got.len(), 1 + MAX_SCAN_MEDIA_URLS, "the formats are capped, the webpage address is not counted");
        // A hand-off names the other site's address; the page's own address is not a video.
        let hand = scan_json(r#"{"extractor":"youtube","webpage_url":"https://www.youtube.com/watch?v=abcdefghijk"}"#);
        assert_eq!(parse_scan_media_urls(&hand, page), ["https://www.youtube.com/watch?v=abcdefghijk"]);
        // A playlist's entries are the page's videos, not a single pick.
        let list = scan_json(r#"{"_type":"playlist","webpage_url":"https://x.example/","entries":[{"url":"https://x.example/a.mp4"}]}"#);
        assert!(parse_scan_media_urls(&list, page).is_empty());
        // An uppercase scheme is still an address yt-dlp would fetch (P2.12).
        let upper = scan_json(r#"{"extractor":"html5","url":"HTTP://169.254.169.254/latest/x","formats":[{"url":"HTTPS://10.0.0.5/v.mp4"}]}"#);
        assert_eq!(parse_scan_media_urls(&upper, page), ["HTTP://169.254.169.254/latest/x", "HTTPS://10.0.0.5/v.mp4"]);
        let list = scan_json(r#"{"_type":"playlist","entries":[{"url":"HTTP://10.0.0.5/a.mp4"}]}"#);
        assert_eq!(parse_page_videos(&list.to_string()), ["HTTP://10.0.0.5/a.mp4"]);
    }

    /// The egress screen sees every address yt-dlp could fetch for one video: past the media cap,
    /// the manifest and fragment bases, requested formats, another protocol's host (P2.12).
    #[test]
    fn the_fetch_list_names_every_address_uncapped() {
        let mut formats: Vec<String> = (0..12).map(|i| format!(r#"{{"url":"https://cdn.example/f{i}.mp4"}}"#)).collect();
        formats.push(r#"{"url":"https://cdn.example/last.m3u8","manifest_url":"HTTP://10.0.0.5/master.m3u8"}"#.into());
        formats.push(r#"{"url":"rtmp://10.0.0.6/live/x","fragment_base_url":"https://frag.example/b/","fragments":[{"path":"seg1"}]}"#.into());
        let scan = scan_json(&format!(
            r#"{{"extractor":"generic","webpage_url":"https://news.example/a","url":"https://cdn.example/f0.mp4","formats":[{}],"requested_formats":[{{"url":"https://req.example/v"}}]}}"#,
            formats.join(",")
        ));
        let got = parse_scan_fetch_urls(&scan);
        for want in [
            "https://cdn.example/f11.mp4",
            "HTTP://10.0.0.5/master.m3u8",
            "http://10.0.0.6/live/x",
            "https://frag.example/b/",
            "https://req.example/v",
            "https://news.example/a",
        ] {
            assert!(got.iter().any(|u| u == want), "{want} missing from {got:?}");
        }
        assert!(!got.iter().any(|u| u == "seg1"), "a relative fragment path is not an address");
        assert_eq!(got.iter().filter(|u| *u == "https://cdn.example/f0.mp4").count(), 1);
        let list = scan_json(r#"{"_type":"playlist","entries":[{"url":"https://x.example/a.mp4"}]}"#);
        assert!(parse_scan_fetch_urls(&list).is_empty());
    }

    #[test]
    fn an_article_sibling_is_never_sent_to_the_browser_first() {
        let generic = PageScan { generic_only: true, ..Default::default() };
        assert_eq!(first_attempt(false, false, false, &generic), FirstAttempt::SnifferFirst);
        assert_eq!(first_attempt(false, false, true, &generic), FirstAttempt::YtDlp);
    }

    #[test]
    fn a_dedicated_extractor_of_the_pages_own_site_is_not_generic_only() {
        let v = scan_json(r#"{"_type":"video","extractor":"example:article","extractor_key":"ExampleArticle","webpage_url":"https://www.example-news.gr/video/7"}"#);
        assert!(!generic_only(&v, "https://example-news.gr/video/7?x=1"));
        let y = scan_json(r#"{"_type":"video","extractor":"youtube","extractor_key":"Youtube","webpage_url":"https://www.youtube.com/watch?v=abcdefghijk"}"#);
        assert!(!generic_only(&y, "https://www.youtube.com/watch?v=abcdefghijk"));
        assert!(!generic_only(&scan_json("{}"), "https://www.example-news.gr/a"));
    }

    #[test]
    fn a_webpage_url_that_is_not_http_is_generic_only_as_an_extractor_result() {
        let v = scan_json(r#"{"_type":"video","extractor":"kaltura","extractor_key":"Kaltura","webpage_url":"kaltura:1234:0_abcd"}"#);
        assert_eq!(generic_why(&v, "https://www.example-news.gr/a"), Some(GenericWhy::Extractor));
        let h = scan_json(r#"{"extractor":"youtube","webpage_url":"https://www.youtube.com/watch?v=abcdefghijk"}"#);
        assert_eq!(generic_why(&h, "https://www.example-news.gr/a"), Some(GenericWhy::HandOff));
    }

    fn hand_off_scan() -> PageScan {
        PageScan {
            generic_only: true,
            hand_off_only: true,
            webpage_url: Some("https://www.youtube.com/watch?v=abcdefghijk".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_redirect_to_a_platform_post_stays_with_yt_dlp() {
        let got = first_attempt_resolved(false, false, false, &hand_off_scan(), "https://short.example/x", |_| async {
            Some("https://www.youtube.com/watch?v=abcdefghijk".to_string())
        })
        .await;
        assert_eq!(got, FirstAttempt::YtDlp);
        // A redirect to the scan's own site that is not a known platform.
        let mut other = hand_off_scan();
        other.webpage_url = Some("https://clips.example.org/v/1".into());
        let got = first_attempt_resolved(false, false, false, &other, "https://short.example/x", |_| async {
            Some("https://clips.example.org/v/1?x=2".to_string())
        })
        .await;
        assert_eq!(got, FirstAttempt::YtDlp);
    }

    #[tokio::test]
    async fn a_news_page_that_stays_on_its_site_with_an_embed_hand_off_goes_to_the_browser() {
        let got = first_attempt_resolved(false, false, false, &hand_off_scan(), "https://www.example-news.gr/a", |_| async {
            Some("https://www.example-news.gr/a".to_string())
        })
        .await;
        assert_eq!(got, FirstAttempt::SnifferFirst);
    }

    #[tokio::test]
    async fn a_resolver_that_fails_keeps_yt_dlp() {
        let got = first_attempt_resolved(false, false, false, &hand_off_scan(), "https://short.example/x", |_| async { None }).await;
        assert_eq!(got, FirstAttempt::YtDlp);
    }

    #[tokio::test]
    async fn an_html5_result_goes_to_the_browser_without_asking_where_the_page_is() {
        let scan = PageScan { generic_only: true, ..Default::default() };
        let got = first_attempt_resolved(false, false, false, &scan, "https://www.example-news.gr/a", |_| async {
            panic!("an extractor result needs no redirect check")
        })
        .await;
        assert_eq!(got, FirstAttempt::SnifferFirst);
    }

    #[test]
    fn the_first_attempt_goes_to_the_browser_only_for_a_generic_only_page() {
        let generic = PageScan { generic_only: true, ..Default::default() };
        let dedicated = PageScan::default();
        let unsupported = PageScan { unsupported: true, ..Default::default() };
        assert_eq!(first_attempt(false, false, false, &generic), FirstAttempt::SnifferFirst);
        assert_eq!(first_attempt(false, false, false, &dedicated), FirstAttempt::YtDlp);
        assert_eq!(first_attempt(false, false, false, &unsupported), FirstAttempt::SnifferOnly);
        // The site API named the video, or it is a platform link: never the browser first.
        assert_eq!(first_attempt(true, false, false, &generic), FirstAttempt::YtDlp);
        assert_eq!(first_attempt(false, true, false, &generic), FirstAttempt::YtDlp);
    }

    fn args_for(opts: &DownloadOpts<'_>) -> Vec<String> {
        Downloader::build_args(
            "https://www.youtube.com/watch?v=abc",
            Path::new("C:/temp/jobs/7"),
            opts,
        )
    }

    #[test]
    fn the_info_line_gives_extractor_length_formats_and_a_title_with_bars() {
        let i = parse_info_line("generic|93.5|[\"hls-720\",\"hls-480\"]|Ena | dyo\n").unwrap();
        assert_eq!((i.extractor.as_str(), i.duration_secs, i.formats), ("generic", Some(93.5), 2));
        assert_eq!(i.title.as_deref(), Some("Ena | dyo"));
        let n = parse_info_line("youtube|NA|null|NA").unwrap();
        assert_eq!((n.duration_secs, n.formats, n.title), (None, 0, None));
        assert!(parse_info_line("").is_none() && parse_info_line("NA|1|[]|x").is_none());
    }

    #[test]
    fn tls_verification_is_on_unless_explicitly_disabled() {
        // Defect D-17: --no-check-certificates was unconditional.
        let args = args_for(&DownloadOpts::default());
        assert!(
            !args.iter().any(|a| a == "--no-check-certificates"),
            "TLS verification must be on by default: {args:?}"
        );

        let args = args_for(&DownloadOpts {
            insecure_tls: true,
            ..Default::default()
        });
        assert!(args.iter().any(|a| a == "--no-check-certificates"));
    }

    #[test]
    fn a_stale_part_file_is_never_resumed() {
        let args = args_for(&DownloadOpts::default());
        assert!(args.iter().any(|a| a == "--no-continue"), "{args:?}");
    }

    #[test]
    fn the_download_is_not_stamped_with_the_servers_date() {
        let args = args_for(&DownloadOpts::default());
        assert!(args.iter().any(|a| a == "--no-mtime"), "{args:?}");
    }

    #[test]
    fn a_leftover_complete_file_is_never_taken_as_already_downloaded() {
        let args = args_for(&DownloadOpts::default());
        assert!(args.iter().any(|a| a == "--force-overwrites"), "{args:?}");
        assert!(!args.iter().any(|a| a == "--no-overwrites" || a == "-w"), "{args:?}");
    }

    #[test]
    fn the_format_selector_never_pulls_more_than_1080() {
        // Defect D-18: the old selector downloaded 4K and then downscaled it,
        // costing minutes of a news deadline for no visible gain.
        let args = args_for(&DownloadOpts::default());
        let i = args.iter().position(|a| a == "-f").expect("-f present");
        let selector = &args[i + 1];
        assert!(
            selector.contains("height<=1080"),
            "format selector must cap the height: {selector}"
        );
        assert!(
            !selector.contains("2160") && !selector.contains("1440"),
            "format selector must not ask for 4K: {selector}"
        );
        // Every fallback must be capped too, or the last one pulls 4K anyway.
        for alt in selector.split('/') {
            assert!(
                alt.contains("height<=1080") || alt == "best",
                "fallback {alt:?} is not height-capped"
            );
        }
    }

    /// P11.6: the second route asks for something else first (another codec,
    /// then a file the site merged), capped like the usual one; every other
    /// argument of the download is the same.
    #[test]
    fn the_second_route_asks_for_other_formats_and_changes_nothing_else() {
        let usual = args_for(&DownloadOpts::default());
        let second = args_for(&DownloadOpts { formats: FormatRoute::Second, ..Default::default() });
        let f = |args: &[String]| args[args.iter().position(|a| a == "-f").unwrap() + 1].clone();
        let selector = f(&second);
        assert_ne!(selector, f(&usual));
        let alts: Vec<&str> = selector.split('/').collect();
        assert_eq!(
            selector,
            "bestvideo[height<=1080][height>=720][vcodec!^=avc1]+bestaudio/best[height<=1080][height>=720][ext=mp4]/best[height<=1080]/best"
        );
        assert_eq!(alts[0], "bestvideo[height<=1080][height>=720][vcodec!^=avc1]+bestaudio", "another codec first, 720 to 1080: {selector}");
        assert_eq!(alts[1], "best[height<=1080][height>=720][ext=mp4]", "then a pre-merged MP4, 720 to 1080: {selector}");
        for alt in &alts {
            assert!(alt.contains("height<=1080") || *alt == "best", "fallback {alt:?} is not height-capped");
        }
        let without_f = |args: &[String]| {
            let i = args.iter().position(|a| a == "-f").unwrap();
            args.iter().enumerate().filter(|(n, _)| *n != i + 1).map(|(_, a)| a.clone()).collect::<Vec<_>>()
        };
        assert_eq!(without_f(&second), without_f(&usual));
    }

    #[test]
    fn resilience_flags_are_present() {
        let args = args_for(&DownloadOpts::default());
        for flag in [
            "--retries",
            "--fragment-retries",
            "--socket-timeout",
            "--concurrent-fragments",
            "--no-playlist",
        ] {
            assert!(args.iter().any(|a| a == flag), "{flag} missing: {args:?}");
        }
    }

    /// iefimerida.gr, 2026-10-05: four Streamable embeds read as a generic
    /// playlist; one job downloaded all four.
    #[test]
    fn only_the_first_video_of_a_page_is_downloaded_into_a_job() {
        let args = args_for(&DownloadOpts::default());
        let i = args.iter().position(|a| a == "--playlist-items").expect("--playlist-items");
        assert_eq!(args[i + 1], "1");
    }

    /// 2026-10-06: two YouTube jobs failed with HTTP 403; yt-dlp said "No
    /// supported JavaScript runtime could be found".
    #[test]
    fn deno_is_handed_to_yt_dlp_when_it_is_there() {
        let deno = PathBuf::from(r"C:\omni\bin\deno.exe");
        let args = args_for(&DownloadOpts { js_runtime: Some(&deno), ..Default::default() });
        let i = args.iter().position(|a| a == "--js-runtimes").expect("--js-runtimes");
        assert_eq!(args[i + 1], r"deno:C:\omni\bin\deno.exe");
        assert!(!args_for(&DownloadOpts::default()).iter().any(|a| a == "--js-runtimes"));

        let dir = tempfile::tempdir().unwrap();
        assert!(Downloader::new(dir.path().join("yt-dlp.exe"), None, &HostPacer::default()).deno.is_none());
        std::fs::write(dir.path().join("deno.exe"), b"x").unwrap();
        assert_eq!(Downloader::new(dir.path().join("yt-dlp.exe"), None, &HostPacer::default()).deno, Some(dir.path().join("deno.exe")));
        // The folder build (P2.10) is one level down: without this YouTube
        // would lose its JavaScript runtime and 403 at random.
        assert_eq!(
            Downloader::new(dir.path().join("yt-dlp").join("yt-dlp.exe"), None, &HostPacer::default()).deno,
            Some(dir.path().join("deno.exe"))
        );
    }

    #[test]
    fn a_probe_reads_the_extractor_and_the_id() {
        assert_eq!(
            parse_probe_line("\nInstagram Dd615LBN4QM\n"),
            Some(Probed { extractor: "Instagram".into(), id: "Dd615LBN4QM".into() })
        );
        assert_eq!(parse_probe_line("generic NA"), None);
        assert_eq!(parse_probe_line(""), None);
    }

    #[test]
    fn a_page_read_as_several_videos_lists_them_in_order() {
        let json = r#"{"_type": "playlist", "extractor": "generic", "entries": [
            {"_type": "url", "url": "https://streamable.com/e/531cym", "ie_key": "Streamable"},
            {"_type": "url", "url": "https://streamable.com/e/k98n7n", "ie_key": "Streamable"},
            {"_type": "url", "url": "https://streamable.com/e/531cym", "ie_key": "Streamable"},
            {"_type": "url", "url": "javascript:void(0)"}]}"#;
        assert_eq!(parse_page_videos(json), vec!["https://streamable.com/531cym", "https://streamable.com/k98n7n"]);
        assert!(parse_page_videos(r#"{"_type": "video", "id": "x", "webpage_url": "https://a/b"}"#).is_empty());
        assert!(parse_page_videos("ERROR: Unsupported URL").is_empty());
        assert!(is_video_platform("https://streamable.com/e/531cym"));
    }

    #[test]
    fn the_output_path_is_requested_explicitly() {
        // Defect D-18: the old code scanned the directory for a filename it
        // hoped matched. --print after_move:filepath is authoritative.
        let args = args_for(&DownloadOpts::default());
        let i = args.iter().position(|a| a == "--print").expect("--print");
        assert_eq!(args[i + 1], "after_move:filepath");
    }

    /// yt-dlp quotes a cookie line it rejects, value included, in its error text.
    #[test]
    fn an_error_of_a_run_that_had_a_jar_is_scrubbed_of_its_values() {
        let dir = tempfile::tempdir().unwrap();
        let jar = crate::cookie_file::CookieFile::write(dir.path(), ".x.com\tTRUE\t/\tTRUE\t0\tauth_token\tleaky-value-998877\n").unwrap();
        let quoted = "ERROR: invalid Netscape format cookies file: '.x.com\tFALSE\t/\tTRUE\t0\tauth_token\tleaky-value-998877'".to_string();

        let dl = Downloader::new("yt-dlp", None, &HostPacer::default()).with_cookie_jar(Some(&jar));
        let out = dl.scrubbed(quoted.clone());
        assert!(!out.contains("leaky-value-998877"), "{out}");
        assert!(out.contains("invalid Netscape format"));
        // The download path uses the same function with the values from its options.
        assert!(!scrub_error(quoted.clone(), jar.values()).contains("leaky-value-998877"));
        // Without a jar the line is left exactly as it was.
        assert_eq!(Downloader::new("yt-dlp", None, &HostPacer::default()).scrubbed(quoted.clone()), quoted);
    }

    #[test]
    fn without_a_jar_there_is_no_cookies_flag_and_with_one_it_is_passed_by_path() {
        let url = "https://x.com/i/status/1";
        let none = Downloader::build_args(url, Path::new("C:/temp/jobs/7"), &DownloadOpts::default());
        assert!(!none.iter().any(|a| a == "--cookies"), "{none:?}");
        let jar = PathBuf::from("C:/temp/jobs/7/cookies.txt");
        let with = Downloader::build_args(url, Path::new("C:/temp/jobs/7"), &DownloadOpts { cookie_jar: Some(&jar), ..Default::default() });
        let i = with.iter().position(|a| a == "--cookies").expect("--cookies");
        assert_eq!(with[i + 1], jar.to_string_lossy());
        assert_eq!(with.last().unwrap(), url, "the link stays last");
        assert_eq!(cookie_args(Some(&jar)), vec!["--cookies".to_string(), jar.to_string_lossy().into_owned()]);
        assert!(cookie_args(None).is_empty());
    }

    #[test]
    fn session_context_is_forwarded_when_given() {
        // Greek portals routinely 403 a bare fetch of a sniffed stream URL but
        // serve it with the article page as the referer.
        let jar = PathBuf::from("C:/temp/x.com.cookies.txt");
        let args = Downloader::build_args(
            "https://cdn.in.gr/media/2026/09/clip/master.m3u8",
            Path::new("C:/temp/jobs/7"),
            &DownloadOpts {
                referer: Some("https://www.in.gr/article"),
                user_agent: Some("Mozilla/5.0 omni"),
                cookie_header: Some("sid=1"),
                cookie_jar: Some(&jar),
                ..Default::default()
            },
        );
        let joined = args.join(" ");
        assert!(joined.contains("--referer https://www.in.gr/article"), "{joined}");
        assert!(joined.contains("--user-agent Mozilla/5.0 omni"), "{joined}");
        assert!(joined.contains("Cookie: sid=1"), "{joined}");
        assert!(joined.contains("--cookies"), "{joined}");
    }

    /// Regression: a newsbomb.gr article embedding an X post. The sniffer
    /// returned the post URL with the article's context, and X's video CDN
    /// answered 403 to the article referer; a plain fetch works.
    #[test]
    fn a_platform_link_found_in_an_article_is_fetched_without_the_article_context() {
        let jar = PathBuf::from("C:/temp/x.com.cookies.txt");
        let opts = DownloadOpts {
            referer: Some("https://www.newsbomb.gr/kosmos/story/1764192/article"),
            user_agent: Some("Mozilla/5.0 omni"),
            cookie_header: Some("_ga=GA1.2.1"),
            cookie_jar: Some(&jar),
            ..Default::default()
        };
        for url in [
            "https://x.com/i/status/2100509173943288138",
            "https://twitter.com/user/status/1",
            "https://www.youtube.com/watch?v=abc",
            "https://m.facebook.com/watch/?v=1",
            "https://www.instagram.com/reel/abc/",
        ] {
            let joined = Downloader::build_args(url, Path::new("C:/temp/jobs/7"), &opts).join(" ");
            assert!(!joined.contains("--referer"), "{url}: {joined}");
            assert!(!joined.contains("--user-agent"), "{url}: {joined}");
            assert!(!joined.contains("Cookie:"), "{url}: {joined}");
            // The station's own login for the platform (P3.5) still applies.
            assert!(joined.contains("--cookies"), "{url}: {joined}");
        }
    }

    #[test]
    fn platform_hosts_are_matched_exactly_not_by_substring() {
        assert!(is_video_platform("https://x.com/i/status/1"));
        assert!(is_video_platform("https://mobile.twitter.com/a/status/1"));
        assert!(is_video_platform("HTTPS://WWW.YOUTUBE.COM/watch?v=1"));
        assert!(!is_video_platform("https://www.newsbomb.gr/x.com/story"));
        assert!(!is_video_platform("https://notyoutube.com/watch?v=1"));
        assert!(!is_video_platform("https://box.com/v"));
        assert!(!is_video_platform("https://cdn.in.gr/master.m3u8?ref=youtube.com"));
    }

    #[test]
    fn the_url_is_always_the_last_argument() {
        // yt-dlp treats anything after the URL as another URL.
        let args = args_for(&DownloadOpts {
            referer: Some("https://example.gr"),
            insecure_tls: true,
            ..Default::default()
        });
        assert_eq!(args.last().unwrap(), "https://www.youtube.com/watch?v=abc");
    }

    /// P11.4e: a `yt-dlp.conf` or plugin folder in the service account's
    /// profile changed what yt-dlp did, and the merge used whatever ffmpeg it
    /// found. Every list a yt-dlp run is started with, with and without the
    /// daemon's ffmpeg.
    #[test]
    fn every_yt_dlp_run_starts_with_the_hardening_flags() {
        let ffmpeg = PathBuf::from(r"C:\omni\bin\ffmpeg.exe");
        let url = "https://www.example.gr/news/1";
        let dir = Path::new("C:/temp/jobs/7");
        let jar = Path::new("C:/temp/jobs/7/cookies.txt");
        for ffmpeg in [Some(ffmpeg.as_path()), None] {
            let dl = Downloader::new("yt-dlp", ffmpeg, &HostPacer::default());
            // (builder, the flag the list started with before the hardening flags)
            let table: Vec<(&str, Vec<String>, &str)> = vec![
                ("download", Downloader::build_args(url, dir, &dl.own_tools(DownloadOpts::default())), "-f"),
                ("probe", dl.probe_args(url), "--simulate"),
                ("page_videos", dl.page_videos_args(url), "--flat-playlist"),
                ("probe_media", dl.probe_media_args(url), "--simulate"),
                ("probe_candidate", dl.probe_candidate_args(url, Some("https://r.example"), Some("UA"), Some("a=1")), "--simulate"),
                ("probe_info", dl.probe_info_args(url), "--simulate"),
                ("cookie test", crate::cookie_file::test_args(ffmpeg, jar, url), "--cookies"),
            ];
            for (name, args, first) in table {
                assert_eq!(&args[..2], ["--ignore-config", "--no-plugin-dirs"], "{name}: {args:?}");
                let rest = match ffmpeg {
                    Some(_) => {
                        assert_eq!(&args[2..4], ["--ffmpeg-location", r"C:\omni\bin"], "{name}: {args:?}");
                        4
                    }
                    None => 2,
                };
                assert_eq!(args.iter().filter(|a| *a == "--ffmpeg-location").count(), usize::from(ffmpeg.is_some()), "{name}: {args:?}");
                assert_eq!(args[rest], first, "{name}: the rest of the list follows the hardening flags: {args:?}");
                assert_eq!(args.last().unwrap(), url, "{name}: the link stays last");
            }
        }
    }

    #[test]
    fn the_ffmpeg_location_is_one_yt_dlp_finds_the_same_ffmpeg_at() {
        let exe = |p: &str| hardening_args(Some(Path::new(p)));
        // Named ffmpeg: its folder (yt-dlp looks for `ffmpeg` inside).
        assert_eq!(exe(r"C:\omni\bin\ffmpeg.exe"), ["--ignore-config", "--no-plugin-dirs", "--ffmpeg-location", r"C:\omni\bin"]);
        assert_eq!(exe(r"\\server\tools\ffmpeg\FFMPEG.EXE")[3], r"\\server\tools\ffmpeg");
        assert_eq!(exe(r"C:\omni\bin\ffmpeg")[3], r"C:\omni\bin");
        // A renamed binary (a config override): the binary itself, since its
        // folder holds no `ffmpeg.exe`.
        assert_eq!(exe(r"D:\tools\ffmpeg-arm64.exe")[3], r"D:\tools\ffmpeg-arm64.exe");
        // A folder is passed as it is.
        assert_eq!(exe(r"C:\omni\bin")[3], r"C:\omni\bin");
        assert_eq!(exe(r"C:\tools\ffmpeg-7.1")[3], r"C:\tools\ffmpeg-7.1");
        // A bare name (found on PATH) names no location: none is given, and
        // yt-dlp finds it as before.
        assert_eq!(exe("ffmpeg.exe"), ["--ignore-config", "--no-plugin-dirs"]);
        assert_eq!(exe("ffmpeg"), ["--ignore-config", "--no-plugin-dirs"]);
        assert_eq!(hardening_args(None), ["--ignore-config", "--no-plugin-dirs"]);
    }

    /// The pinned yt-dlp knows the three flags by these names: a `--simulate`
    /// of an address that is not a URL (refused at once, no network) ends the
    /// same with them as without. An unknown flag ends otherwise, checked too:
    /// `--version` would not do, it exits 0 even with an unknown flag.
    #[tokio::test]
    async fn the_pinned_yt_dlp_accepts_the_hardening_flags() {
        let bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bin");
        let ytdl = omni_core::dependencies::ytdl_exe_in(&bin);
        let ffmpeg = bin.join("ffmpeg.exe");
        if !ytdl.is_file() || !ffmpeg.is_file() {
            eprintln!("WARNING: skipped, no yt-dlp or ffmpeg in {bin:?}");
            return;
        }
        let simulate = |pre: Vec<String>| {
            let ytdl = ytdl.clone();
            async move {
                let mut args = pre;
                args.extend(["--simulate".to_string(), "omni-not-a-url".to_string()]);
                let o = run(&ytdl, args, RunOpts::new(Duration::from_secs(120))).await.unwrap();
                (o.exit_code, o.stderr_tail)
            }
        };
        let plain = simulate(Vec::new()).await;
        assert!(plain.1.contains("is not a valid URL"), "{plain:?}");
        let hardened = simulate(hardening_args(Some(&ffmpeg))).await;
        assert_eq!(hardened.0, plain.0, "{hardened:?}");
        assert!(hardened.1.contains("is not a valid URL"), "{hardened:?}");
        let unknown = simulate(vec!["--no-such-flag-omni".to_string()]).await;
        assert_ne!(unknown.0, plain.0, "an unknown flag is refused, so the comparison above means something: {unknown:?}");
        assert!(unknown.1.contains("no such option"), "{unknown:?}");
    }

    #[test]
    fn progress_lines_parse_into_percent_speed_and_eta() {
        let p = parse_progress("5242880 10485760 2097152 5").expect("parses");
        assert!((p.percent - 50.0).abs() < 0.01, "{}", p.percent);
        assert_eq!(p.speed, "2.0 MB/s");
        assert_eq!(p.eta, "00:05");
    }

    #[test]
    fn unknown_progress_values_do_not_produce_nonsense() {
        // yt-dlp emits NA for the total until it knows the size; showing 0% is
        // honest, showing NaN% or 100% is not.
        let p = parse_progress("5242880 NA NA NA").expect("parses");
        assert_eq!(p.percent, 0.0);
        assert_eq!(p.speed, "--");
        assert_eq!(p.eta, "--:--");
        assert!(parse_progress("garbage").is_none());
    }

    #[tokio::test]
    async fn the_fallback_scan_ignores_partial_downloads() {
        let dir = tempfile::tempdir().unwrap();
        // A finished small file and a large in-progress one: picking the .part
        // would feed a truncated file to the transcoder.
        tokio::fs::write(dir.path().join("source.mp4"), vec![0u8; 1000])
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("source.mkv.part"), vec![0u8; 500_000])
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("source.ytdl"), vec![0u8; 900_000])
            .await
            .unwrap();

        let found = find_downloaded_file(dir.path()).await.expect("a file");
        assert_eq!(found.file_name().unwrap(), "source.mp4");
    }

    #[tokio::test]
    async fn leftovers_from_an_earlier_owner_are_cleared_before_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        // What a previous owner left behind: a bigger source and a delivered output.
        tokio::fs::write(dir.path().join("source.webm"), vec![0u8; 900_000]).await.unwrap();
        tokio::fs::write(dir.path().join("SOURCE.MP4"), vec![0u8; 800_000]).await.unwrap();
        tokio::fs::write(dir.path().join("7_output.mxf"), vec![0u8; 950_000]).await.unwrap();

        clear_stale_sources(dir.path()).await.unwrap();
        // This run's file arrives (smaller than the stale ones).
        tokio::fs::write(dir.path().join("source.mkv"), vec![0u8; 1000]).await.unwrap();

        let found = find_downloaded_file(dir.path()).await.expect("the fresh file");
        assert_eq!(found.file_name().unwrap(), "source.mkv");
        // Only source.* is cleared; other files are left alone.
        assert!(dir.path().join("7_output.mxf").exists());
    }

    #[tokio::test]
    async fn nothing_is_found_when_the_run_wrote_no_file() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("source.webm"), vec![0u8; 900_000]).await.unwrap();
        tokio::fs::write(dir.path().join("7_output.mxf"), vec![0u8; 950_000]).await.unwrap();
        clear_stale_sources(dir.path()).await.unwrap();
        assert!(find_downloaded_file(dir.path()).await.is_none());
    }

    #[tokio::test]
    async fn a_fresh_file_with_an_old_mtime_is_still_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.mp4");
        std::fs::write(&path, vec![0u8; 1000]).unwrap();
        // yt-dlp can stamp the server's Last-Modified on the download.
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_680_000_000);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();

        let found = find_downloaded_file(dir.path()).await.expect("the file");
        assert_eq!(found.file_name().unwrap(), "source.mp4");
    }

    #[test]
    fn the_lexical_fallback_accepts_only_a_direct_child_of_the_output_dir() {
        let root = if cfg!(windows) { Path::new(r"C:\jobs\7") } else { Path::new("/jobs/7") };
        assert!(lexically_directly_inside(&root.join("source.mp4"), root));
        assert!(!lexically_directly_inside(&root.join("sub").join("source.mp4"), root));
        assert!(!lexically_directly_inside(&root.parent().unwrap().join("source.mp4"), root));
        assert!(!lexically_directly_inside(Path::new("source.mp4"), root));
    }

    #[tokio::test]
    async fn the_fallback_scan_ignores_fresh_partial_fragments() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("source.mp4.part"), vec![0u8; 1000]).await.unwrap();
        tokio::fs::write(dir.path().join("source.mp4.part-Frag3"), vec![0u8; 1000]).await.unwrap();
        tokio::fs::write(dir.path().join("source.mp4.ytdl"), vec![0u8; 1000]).await.unwrap();

        assert!(find_downloaded_file(dir.path()).await.is_none());
    }

    #[test]
    fn a_printed_path_outside_the_output_dir_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let outside = other.path().join("source.mp4");
        std::fs::write(&outside, b"x").unwrap();
        let inside = dir.path().join("source.mp4");
        std::fs::write(&inside, b"x").unwrap();

        let lines = vec![inside.display().to_string(), outside.display().to_string()];
        // The last line is outside, so it is skipped and the inside one wins.
        let got = printed_path_in_dir(&lines, dir.path()).expect("inside file");
        assert_eq!(got.file_name().unwrap(), "source.mp4");
        assert!(got.canonicalize().unwrap().starts_with(dir.path().canonicalize().unwrap()));
        assert!(printed_path_in_dir(&lines[1..], dir.path()).is_none());
    }

    #[test]
    fn the_operator_facing_message_is_the_error_line_not_the_last_line() {
        let stderr = "[youtube] Extracting URL\n\
                      ERROR: [youtube] abc: Private video. Sign in if you've been granted access\n\
                      [debug] Exiting with code 1";
        let msg = first_error_line(stderr);
        assert!(msg.contains("Private video"), "{msg}");
        assert!(!msg.contains("Exiting with code"), "{msg}");
    }
}

#[cfg(test)]
mod platform_probe_tests {
    use super::*;

    #[test]
    fn platform_line_is_parsed() {
        let p = parse_platform_line("154.5|720|False|not_live\n").unwrap();
        assert_eq!((p.duration_secs, p.height, p.live), (Some(154.5), Some(720), false));
        let p = parse_platform_line("NA|NA|True|is_live").unwrap();
        assert_eq!((p.duration_secs, p.height, p.live), (None, None, true));
        assert!(parse_platform_line("90|480|NA|is_upcoming").unwrap().live);
        assert!(!parse_platform_line("90|480|NA|was_live").unwrap().live);
        assert!(parse_platform_line("garbage").is_none());
        assert!(parse_platform_line("").is_none());
    }

    #[test]
    fn the_download_carries_the_live_filter_and_the_skip_message_is_recognised() {
        let args = Downloader::build_args("https://www.youtube.com/watch?v=x", Path::new("out"), &DownloadOpts::default());
        let i = args.iter().position(|a| a == "--match-filters").expect("filter present");
        assert_eq!(args[i + 1], LIVE_FILTER);
        // The real yt-dlp 2026.08.19 line, with our filter in it.
        let real = format!("[download] Some title does not pass filter ({LIVE_FILTER}), skipping ..");
        assert!(skipped_as_live(&[real]));
        // Another filter's skip, or ordinary output, is not ours.
        assert!(!skipped_as_live(&["[download] t does not pass filter (duration>5), skipping ..".to_string()]));
        assert!(!skipped_as_live(&["[download] Destination: x.mp4".to_string()]));
    }

    #[tokio::test]
    async fn an_empty_exit_zero_download_is_live_when_the_probe_says_so() {
        // The real shape (yt-dlp 2026.08.19 on a live YouTube): exit 0, no output, no file.
        let nothing: Vec<String> = Vec::new();
        assert!(empty_download_is_live(&nothing, || async { Some(true) }).await);
        // Control: the probe says it is not live, or cannot say: the plain no-file error stands.
        assert!(!empty_download_is_live(&nothing, || async { Some(false) }).await);
        assert!(!empty_download_is_live(&nothing, || async { None }).await);
        // The skip line alone is enough, without asking.
        let line = format!("[download] t does not pass filter ({LIVE_FILTER}), skipping ..");
        assert!(empty_download_is_live(&[line], || async { panic!("not needed") }).await);
    }

    fn cand(url: &str, held: bool) -> Cand {
        Cand { url: url.to_string(), held, probe_able: is_video_platform(url) }
    }

    const YT_LIVE: &str = "https://www.youtube.com/watch?v=liveliveli1";
    const YT_LIVE2: &str = "https://www.youtube.com/watch?v=liveliveli2";
    const X_POST: &str = "https://x.com/i/status/2";
    const CL: &str = "https://player.glomex.com/integration/1/iframe-player.html?integrationId=i1&playlistId=cl-abc";

    /// The prober of a test: these addresses are live.
    async fn choose(cands: &[Cand], max: usize, live: &[&str]) -> Choice {
        let live: Vec<String> = live.iter().map(|s| s.to_string()).collect();
        choose_primary(cands, max, |u| {
            let l = live.contains(&u);
            async move { if l { Probe::Live } else { Probe::Ok } }
        })
        .await
    }

    #[tokio::test]
    async fn nothing_live_the_first_own_candidate_wins_as_before() {
        let c = [cand(X_POST, false), cand(YT_LIVE, false), cand(CL, true)];
        assert_eq!(choose(&c, 3, &[]).await, Choice::Primary { url: X_POST.into(), live: vec![], dead: vec![] });
        assert_eq!(choose_primary(&[], 3, |_| async { Probe::Unknown }).await, Choice::Nothing);
    }

    #[tokio::test]
    async fn a_live_own_candidate_is_skipped_for_the_next_own_one() {
        // Page scan: [glomex cl-, a LIVE youtube, an x post]: the x post, not the live one.
        let c = [cand(CL, true), cand(YT_LIVE, false), cand(X_POST, false)];
        assert_eq!(choose(&c, 3, &[YT_LIVE]).await, Choice::Primary { url: X_POST.into(), live: vec![YT_LIVE.into()], dead: vec![] });
    }

    #[tokio::test]
    async fn a_held_candidate_is_never_the_fallback_when_every_own_one_is_live() {
        // [youtube LIVE, glomex cl-]: all live, NOT the cl- list.
        let c = [cand(YT_LIVE, false), cand(CL, true)];
        assert_eq!(choose(&c, 3, &[YT_LIVE]).await, Choice::AllLive { live: vec![YT_LIVE.into()] });
        // Sniff path: [in-article youtube LIVE, sidebar youtube, glomex cl-].
        let side = "https://www.youtube.com/watch?v=sIdEbAr0001";
        let c = [cand(YT_LIVE, false), cand(side, true), cand(CL, true)];
        assert_eq!(choose(&c, 3, &[YT_LIVE]).await, Choice::AllLive { live: vec![YT_LIVE.into()] });
    }

    #[tokio::test]
    async fn a_page_with_only_held_candidates_takes_the_first_that_is_not_live() {
        let side = "https://www.youtube.com/watch?v=sIdEbAr0001";
        let c = [cand(YT_LIVE, true), cand(side, true), cand(CL, true)];
        assert_eq!(choose(&c, 3, &[YT_LIVE]).await, Choice::Primary { url: side.into(), live: vec![YT_LIVE.into()], dead: vec![] });
        let c = [cand(YT_LIVE, true), cand(YT_LIVE2, true)];
        assert!(matches!(choose(&c, 3, &[YT_LIVE, YT_LIVE2]).await, Choice::AllLive { .. }));
        // Today's behaviour: a lone held candidate stays the primary, unasked.
        let c = [cand(CL, true)];
        assert_eq!(choose_primary(&c, 3, |_| async { panic!("not asked") }).await, Choice::Primary { url: CL.into(), live: vec![], dead: vec![] });
    }

    #[tokio::test]
    async fn the_probe_budget_is_respected_and_an_unknown_answer_or_a_raw_stream_is_kept() {
        let c: Vec<Cand> = (1..=5).map(|i| cand(&format!("https://www.youtube.com/watch?v=liveliveli{i}"), false)).collect();
        let n = std::cell::Cell::new(0);
        let r = choose_primary(&c, MAX_LIVE_PROBES, |_| {
            n.set(n.get() + 1);
            async { Probe::Live }
        })
        .await;
        assert_eq!(n.get(), MAX_LIVE_PROBES);
        assert!(matches!(r, Choice::Primary { ref url, .. } if *url == c[3].url), "the fourth is taken unasked: {r:?}");
        // Unknown answer keeps the candidate.
        let two = [cand(X_POST, false), cand(YT_LIVE, false)];
        assert_eq!(choose_primary(&two, 3, |_| async { Probe::Unknown }).await, Choice::Primary { url: X_POST.into(), live: vec![], dead: vec![] });
        // A raw stream is taken without a question.
        let raw = [cand("https://cdn.example.com/a.m3u8", false), cand(YT_LIVE, false)];
        assert_eq!(
            choose_primary(&raw, 3, |_| async { panic!("raw streams are not asked") }).await,
            Choice::Primary { url: raw[0].url.clone(), live: vec![], dead: vec![] }
        );
    }

    const JW: &str = "https://cdn.example-player.test/players/clip-1.html";
    const YT: &str = "https://www.youtube.com/watch?v=abcdefghijk";
    const MP4: &str = "https://cdn.example-news.test/2012/clip-720.mp4";
    const MP4_B: &str = "https://cdn.example-news.test/brightcove/clip-16x9.mp4";

    fn all_probed(url: &str, held: bool) -> Cand {
        Cand { url: url.to_string(), held, probe_able: true }
    }

    /// A prober that answers from a table; an address not in it is readable.
    async fn ask(cands: &[Cand], max: usize, answers: &[(&str, Probe)]) -> Choice {
        let table: Vec<(String, Probe)> = answers.iter().map(|(u, p)| (u.to_string(), *p)).collect();
        choose_primary(cands, max, |u| {
            let p = table.iter().find(|(k, _)| *k == u).map(|(_, p)| *p).unwrap_or(Probe::Ok);
            async move { p }
        })
        .await
    }

    #[test]
    fn only_the_four_gone_codes_are_dead_candidates() {
        use ErrorCode::*;
        for dead in [PageNotFound, PrivateOrRemoved, UnsupportedUrl, NoStreamFound] {
            assert!(is_dead_candidate(dead), "{dead}");
        }
        for other in [
            Http403,
            Network,
            LoginRequired,
            GeoBlocked,
            LiveStream,
            DownloadTimeout,
            ExtractTimeout,
            ProbeFailed,
            TranscodeFailed,
            ComplianceFailed,
            DeliveryFailed,
            LowDisk,
            PipelineFailed,
        ] {
            assert!(!is_dead_candidate(other), "{other}");
        }
    }

    #[test]
    fn every_sniffed_candidate_is_probe_able_and_held_follows_the_rule() {
        let streams = vec![JW.to_string(), YT.to_string(), MP4.to_string()];
        let c = sniffed_cands(&streams, |u| u == YT);
        assert!(c.iter().all(|c| c.probe_able), "a raw stream or player page is asked too");
        assert_eq!(c.iter().map(|c| c.held).collect::<Vec<_>>(), [false, true, false]);
        assert_eq!(c[2].url, MP4);
    }

    #[tokio::test]
    async fn a_dead_player_page_and_a_dead_post_are_skipped_for_the_stream_that_plays() {
        // The Guardian shape: [player page 404, YouTube unavailable, mp4 reads].
        let c = [all_probed(JW, false), all_probed(YT, false), all_probed(MP4, false), all_probed(MP4_B, false)];
        let got = ask(&c, MAX_LIVE_PROBES, &[(JW, Probe::Dead(ErrorCode::PageNotFound)), (YT, Probe::Dead(ErrorCode::PrivateOrRemoved))]).await;
        assert_eq!(got, Choice::Primary { url: MP4.into(), live: vec![], dead: vec![JW.into(), YT.into()] });
    }

    #[tokio::test]
    async fn a_held_candidate_is_never_taken_when_every_own_one_is_dead() {
        let c = [all_probed(JW, false), all_probed(YT, true), all_probed(MP4, true)];
        let got = ask(&c, 3, &[(JW, Probe::Dead(ErrorCode::PageNotFound))]).await;
        // One own candidate is the whole tier: dead, so nothing, and not a held one.
        assert_eq!(got, Choice::AllDead { code: ErrorCode::PageNotFound, live: vec![] });
        let two = [all_probed(JW, false), all_probed(MP4_B, false), all_probed(YT, true)];
        assert!(matches!(
            ask(&two, 3, &[(JW, Probe::Dead(ErrorCode::PageNotFound)), (MP4_B, Probe::Dead(ErrorCode::UnsupportedUrl))]).await,
            Choice::AllDead { code: ErrorCode::PageNotFound, .. }
        ));
    }

    #[tokio::test]
    async fn at_most_three_candidates_are_asked_and_an_unknown_answer_keeps_the_candidate() {
        let urls: Vec<String> = (1..=5).map(|i| format!("https://cdn.example-news.test/v{i}.mp4")).collect();
        let c: Vec<Cand> = urls.iter().map(|u| all_probed(u, false)).collect();
        let asked = std::cell::Cell::new(0);
        let got = choose_primary(&c, MAX_LIVE_PROBES, |_| {
            asked.set(asked.get() + 1);
            async { Probe::Dead(ErrorCode::PageNotFound) }
        })
        .await;
        assert_eq!(asked.get(), 3);
        assert!(matches!(got, Choice::Primary { ref url, .. } if *url == urls[3]), "the fourth is taken unasked: {got:?}");
        let two = [all_probed(JW, false), all_probed(MP4, false)];
        assert_eq!(
            ask(&two, 3, &[(JW, Probe::Unknown)]).await,
            Choice::Primary { url: JW.into(), live: vec![], dead: vec![] }
        );
        // A lone candidate is never asked: its download decides.
        let lone = [all_probed(JW, false)];
        assert_eq!(
            choose_primary(&lone, 3, |_| async { panic!("not asked") }).await,
            Choice::Primary { url: JW.into(), live: vec![], dead: vec![] }
        );
    }

    #[tokio::test]
    async fn every_candidate_dead_is_all_dead_with_the_first_code_and_a_live_mix_follows_the_same_rule() {
        let c = [all_probed(JW, false), all_probed(YT, false), all_probed(MP4, false)];
        let dead = |code| Probe::Dead(code);
        let got = ask(&c, 3, &[(JW, dead(ErrorCode::NoStreamFound)), (YT, dead(ErrorCode::PrivateOrRemoved)), (MP4, dead(ErrorCode::PageNotFound))]).await;
        assert_eq!(got, Choice::AllDead { code: ErrorCode::NoStreamFound, live: vec![] });
        // Dead and live with none readable: all dead (the first dead code), the live one listed.
        let got = ask(&c, 3, &[(JW, Probe::Live), (YT, dead(ErrorCode::PrivateOrRemoved)), (MP4, dead(ErrorCode::PageNotFound))]).await;
        assert_eq!(got, Choice::AllDead { code: ErrorCode::PrivateOrRemoved, live: vec![JW.into()] });
        // Only live ones: all live, as before.
        let got = ask(&c, 3, &[(JW, Probe::Live), (YT, Probe::Live), (MP4, Probe::Live)]).await;
        assert!(matches!(got, Choice::AllLive { .. }));
    }

    // ------------------------------------------------------------ pacing (P11.4)

    fn finished(success: bool, stderr: &str) -> Result<RunOutcome> {
        Ok(RunOutcome {
            success,
            exit_code: Some(if success { 0 } else { 1 }),
            stderr_tail: stderr.to_string(),
            cancelled: false,
            ..cancelled_before_start()
        })
    }

    /// Every platform yt-dlp has its own extractor for is paced, under one key per family.
    #[test]
    fn every_video_platform_has_a_pacing_key() {
        for d in VIDEO_PLATFORMS {
            let url = format!("https://{d}/watch?v=1");
            assert!(platform_key(&url).is_some(), "{d}");
            assert!(is_video_platform(&url), "{d}");
        }
    }

    /// A platform link's run holds a turn while it runs; a news page's takes none.
    #[tokio::test(start_paused = true)]
    async fn a_platform_run_takes_a_turn_and_a_news_page_does_not() {
        let pacer = HostPacer::with_jitter(1, Duration::from_secs(8), || Duration::ZERO);
        let p = pacer.clone();
        let o = paced(&pacer, "https://youtu.be/abcdefghijk", None, None, None, false, || async move {
            assert_eq!(p.running("youtube"), 1, "the run did not hold its turn");
            finished(true, "")
        })
        .await
        .unwrap();
        assert!(o.success);
        assert_eq!(pacer.running("youtube"), 0, "the turn outlived the run");
        // The next start on YouTube waits for the gap: the run counted as a start.
        let t0 = tokio::time::Instant::now();
        paced(&pacer, "https://www.youtube.com/watch?v=abcdefghijk", None, None, None, false, || async move { finished(true, "") }).await.unwrap();
        assert_eq!(tokio::time::Instant::now() - t0, Duration::from_secs(8));

        let p = &pacer;
        let t0 = tokio::time::Instant::now();
        for page in ["https://www.news.example/story/1", "https://cdn.news.example/master.m3u8"] {
            paced(&pacer, page, None, None, None, false, || async move {
                for key in ["youtube", "x", "facebook", "instagram", "tiktok", "vimeo", "dailymotion", "streamable"] {
                    assert_eq!(p.running(key), 0, "{page} took a turn on {key}");
                }
                finished(true, "")
            })
            .await
            .unwrap();
        }
        assert_eq!(tokio::time::Instant::now(), t0, "a news page waited for a platform's turn");
    }

    /// The production bot check holds every new YouTube start for two minutes;
    /// an ordinary failure only the gap. Other platforms are not held.
    #[tokio::test(start_paused = true)]
    async fn a_rate_limited_run_holds_the_platform_for_two_minutes() {
        let pacer = HostPacer::with_jitter(1, Duration::from_secs(8), || Duration::ZERO);
        let bot = "ERROR: [youtube] abc123: Sign in to confirm you’re not a bot. Use --cookies-from-browser or --cookies for the authentication.";
        paced(&pacer, "https://youtu.be/abc123", None, None, None, false, || async move { finished(false, bot) }).await.unwrap();
        let t0 = tokio::time::Instant::now();
        paced(&pacer, "https://x.com/a/status/1", None, None, None, false, || async move { finished(true, "") }).await.unwrap();
        assert_eq!(tokio::time::Instant::now(), t0, "another platform was held");
        paced(&pacer, "https://youtu.be/def456", None, None, None, false, || async move { finished(false, "ERROR: Private video") }).await.unwrap();
        assert_eq!(tokio::time::Instant::now() - t0, RATE_LIMIT_COOL_DOWN);
        // An ordinary failure: the next start waits only the gap.
        let t1 = tokio::time::Instant::now();
        paced(&pacer, "https://youtu.be/ghi789", None, None, None, false, || async move { finished(true, "") }).await.unwrap();
        assert_eq!(tokio::time::Instant::now() - t1, Duration::from_secs(8));
    }

    /// A wait cut short by the job's token never starts the run, reads as cancelled,
    /// and is not told as a wait.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_wait_never_starts_the_run() {
        let pacer = HostPacer::with_jitter(1, Duration::from_secs(8), || Duration::ZERO);
        let _running = pacer.acquire("youtube", &CancellationToken::new()).await.unwrap();
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            c.cancel();
        });
        let told = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let t = told.clone();
        let note: WaitNote = std::sync::Arc::new(move |k, w| t.lock().unwrap().push((k, w)));
        let o = paced(&pacer, "https://youtu.be/abc", Some(&cancel), Some(&note), None, false, || async move { panic!("the run started") }).await.unwrap();
        assert!(o.cancelled && !o.success);
        assert!(told.lock().unwrap().is_empty());
    }

    /// A wait over two seconds is told once, with the key; a shorter one is not.
    #[tokio::test(start_paused = true)]
    async fn a_wait_over_two_seconds_is_told_once_with_its_key() {
        let pacer = HostPacer::with_jitter(1, Duration::from_secs(8), || Duration::ZERO);
        let told = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let t = told.clone();
        let note: WaitNote = std::sync::Arc::new(move |k, w| t.lock().unwrap().push((k, w)));
        paced(&pacer, "https://youtu.be/a", None, Some(&note), None, false, || async move { finished(true, "") }).await.unwrap();
        assert!(told.lock().unwrap().is_empty(), "no wait, nothing told");
        paced(&pacer, "https://youtu.be/b", None, Some(&note), None, false, || async move { finished(true, "") }).await.unwrap();
        assert_eq!(*told.lock().unwrap(), vec![("youtube", Duration::from_secs(8))]);

        let short = HostPacer::with_jitter(1, Duration::from_secs(2), || Duration::ZERO);
        told.lock().unwrap().clear();
        paced(&short, "https://youtu.be/a", None, Some(&note), None, false, || async move { finished(true, "") }).await.unwrap();
        paced(&short, "https://youtu.be/b", None, Some(&note), None, false, || async move { finished(true, "") }).await.unwrap();
        assert!(told.lock().unwrap().is_empty(), "a two-second wait was told");
    }

    /// The downloader's own probe goes through the pacer: a run that could not
    /// even start (no yt-dlp here) still spaced the next YouTube start.
    #[tokio::test(start_paused = true)]
    async fn the_downloaders_probe_is_paced() {
        let dir = tempfile::tempdir().unwrap();
        let pacer = HostPacer::with_jitter(1, Duration::from_secs(8), || Duration::ZERO);
        let dl = Downloader::new(dir.path().join("no-yt-dlp.exe"), None, &pacer);
        assert!(dl.probe("https://youtu.be/abcdefghijk", Duration::from_secs(5)).await.is_err());
        assert!(dl.probe_media("https://www.news.example/story/1", Duration::from_secs(5)).await.is_err());
        let t0 = tokio::time::Instant::now();
        drop(pacer.acquire("youtube", &CancellationToken::new()).await.unwrap());
        assert_eq!(tokio::time::Instant::now() - t0, Duration::from_secs(8));
    }

    /// A download run on `pacer` started now: progress at `progress` (never
    /// with `None`), the end at `end`, failing with `stderr` when it is not empty.
    fn long_download(
        pacer: &HostPacer,
        progress: Option<Duration>,
        end: Duration,
        stderr: &'static str,
    ) -> (tokio::task::JoinHandle<RunOutcome>, TurnSlot) {
        let turn: TurnSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (p, slot) = (pacer.clone(), turn.clone());
        let task = tokio::spawn(async move {
            let handler = slot.clone();
            paced(&p, "https://www.youtube.com/watch?v=longshow001", None, None, Some(&slot), false, || async move {
                let t0 = tokio::time::Instant::now();
                if let Some(at) = progress {
                    tokio::time::sleep(at).await;
                    // What the stdout handler does with yt-dlp's first progress line.
                    assert!(progress_line("1048576 734003200 2097152.0 349", &handler).is_some());
                }
                tokio::time::sleep_until(t0 + end).await;
                finished(stderr.is_empty(), stderr)
            })
            .await
            .unwrap()
        });
        (task, turn)
    }

    /// When the next YouTube run on `pacer` starts, from now.
    async fn next_start_after(pacer: &HostPacer) -> Duration {
        let t0 = tokio::time::Instant::now();
        let started = std::sync::Arc::new(std::sync::Mutex::new(None));
        let s = started.clone();
        paced(pacer, "https://youtu.be/urgentclip1", None, None, None, false, || async move {
            *s.lock().unwrap() = Some(tokio::time::Instant::now() - t0);
            finished(true, "")
        })
        .await
        .unwrap();
        let at = started.lock().unwrap().expect("the run started");
        at
    }

    /// P11.4: a download gives its turn back at its first progress line. One
    /// that shows progress at 5 s and ends at 300 s lets the next YouTube run
    /// start at max(5 s, gap + jitter), not at 300 s.
    #[tokio::test(start_paused = true)]
    async fn a_download_gives_its_turn_back_at_its_first_progress() {
        let pacer = HostPacer::with_jitter(1, Duration::from_secs(8), || Duration::from_secs(3));
        let (long, _) = long_download(&pacer, Some(Duration::from_secs(5)), Duration::from_secs(300), "");
        tokio::task::yield_now().await;
        assert_eq!(next_start_after(&pacer).await, Duration::from_secs(11), "gap 8 s + jitter 3 s");
        assert!(!long.is_finished(), "the long download is still running");
        assert!(long.await.unwrap().success);
        assert_eq!(pacer.running("youtube"), 0);

        // Gap shorter than the extraction: the progress line decides.
        let pacer = HostPacer::with_jitter(1, Duration::from_secs(2), || Duration::ZERO);
        let (long, _) = long_download(&pacer, Some(Duration::from_secs(5)), Duration::from_secs(300), "");
        tokio::task::yield_now().await;
        assert_eq!(next_start_after(&pacer).await, Duration::from_secs(5));
        long.await.unwrap();
    }

    /// A run that never shows progress (it failed in extraction, or found the
    /// file complete) holds the platform until it ends, and so does a probe.
    #[tokio::test(start_paused = true)]
    async fn a_run_without_progress_and_a_probe_hold_the_turn_to_the_end() {
        let pacer = HostPacer::with_jitter(1, Duration::from_secs(8), || Duration::ZERO);
        let (long, _) = long_download(&pacer, None, Duration::from_secs(300), "ERROR: [youtube] x: Video unavailable");
        tokio::task::yield_now().await;
        assert_eq!(next_start_after(&pacer).await, Duration::from_secs(300));
        assert!(!long.await.unwrap().success);

        let pacer = HostPacer::with_jitter(1, Duration::from_secs(8), || Duration::ZERO);
        let p = pacer.clone();
        let probe = tokio::spawn(async move {
            paced(&p, "https://youtu.be/probe000001", None, None, None, false, || async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                finished(true, "")
            })
            .await
            .unwrap()
        });
        tokio::task::yield_now().await;
        assert_eq!(next_start_after(&pacer).await, Duration::from_secs(60));
        probe.await.unwrap();
    }

    /// A download refused as a rate limit after it gave its turn back still
    /// holds the platform for the cool-down from its end.
    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_after_the_turn_was_given_back_still_cools_the_platform_down() {
        let pacer = HostPacer::with_jitter(2, Duration::from_secs(8), || Duration::ZERO);
        let bot = "ERROR: [youtube] longshow001: Sign in to confirm you’re not a bot. Use --cookies-from-browser or --cookies";
        let (long, turn) = long_download(&pacer, Some(Duration::from_secs(5)), Duration::from_secs(300), bot);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(turn.lock().unwrap().is_none(), "the turn was not given back at the progress line");
        assert_eq!(pacer.running("youtube"), 0);
        assert!(!long.await.unwrap().success);
        assert_eq!(next_start_after(&pacer).await, RATE_LIMIT_COOL_DOWN);
    }

    /// The turn goes back exactly once: at the progress line, not again at the
    /// end; and a run dropped (cancelled) before or after its progress line
    /// leaves nothing held.
    #[tokio::test(start_paused = true)]
    async fn the_turn_is_given_back_exactly_once_and_never_leaks() {
        let never = CancellationToken::new();
        // Two at a time, one held outside: a second release would take its count too.
        let pacer = HostPacer::with_jitter(2, Duration::ZERO, || Duration::ZERO);
        let outside = pacer.acquire("youtube", &never).await.unwrap();
        let (long, turn) = long_download(&pacer, Some(Duration::from_secs(5)), Duration::from_secs(30), "");
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(pacer.running("youtube"), 2);
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(pacer.running("youtube"), 1, "given back at the progress line");
        release_turn(&turn);
        assert_eq!(pacer.running("youtube"), 1, "a second release took another run's count");
        long.await.unwrap();
        assert_eq!(pacer.running("youtube"), 1, "released again at the end");
        // The handler's later progress lines change nothing either.
        assert!(progress_line("2097152 734003200 2097152.0 300", &turn).is_some());
        assert_eq!(pacer.running("youtube"), 1);
        drop(outside);
        assert_eq!(pacer.running("youtube"), 0);

        // Dropped between the progress line and the end, and before any progress.
        for progress in [Some(Duration::from_secs(5)), None] {
            let (long, turn) = long_download(&pacer, progress, Duration::from_secs(300), "");
            tokio::time::sleep(Duration::from_secs(10)).await;
            long.abort();
            assert!(long.await.unwrap_err().is_cancelled());
            assert!(turn.lock().unwrap().is_none(), "{progress:?}: the turn was left in its slot");
            assert_eq!(pacer.running("youtube"), 0, "{progress:?}: the turn leaked");
        }
    }

    /// Only a parsed progress line gives the turn back: yt-dlp's "NA" before it
    /// knows the size, or a malformed line, does not.
    #[tokio::test]
    async fn only_a_parsed_progress_line_gives_the_turn_back() {
        let pacer = HostPacer::with_jitter(1, Duration::ZERO, || Duration::ZERO);
        let turn: TurnSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
        *turn.lock().unwrap() = Some(pacer.acquire("youtube", &CancellationToken::new()).await.unwrap());
        assert!(progress_line("NA NA NA NA", &turn).is_none());
        assert!(progress_line("garbage", &turn).is_none());
        assert_eq!(pacer.running("youtube"), 1);
        assert!(progress_line("1024 NA NA NA", &turn).is_some());
        assert_eq!(pacer.running("youtube"), 0);
    }

    /// Review round 1: a caller that is not a job is answered at once during a
    /// cool-down, with a failure that says so and reads as RATE_LIMITED; the
    /// run is never started and the cool-down is not extended. It still waits
    /// through the ordinary gap.
    #[tokio::test(start_paused = true)]
    async fn a_fail_fast_run_is_answered_at_once_during_a_cool_down_and_waits_through_a_gap() {
        let pacer = HostPacer::with_jitter(1, Duration::from_secs(8), || Duration::ZERO);
        pacer.penalize("youtube", RATE_LIMIT_COOL_DOWN);
        let t0 = tokio::time::Instant::now();
        let o = paced(&pacer, "https://youtu.be/abc", None, None, None, true, || async move { panic!("the run started") })
            .await
            .unwrap();
        assert_eq!(tokio::time::Instant::now(), t0, "it waited for the cool-down");
        assert!(!o.success && !o.cancelled && !o.timed_out);
        assert!(o.stderr_tail.contains("youtube is cooling down for 120 s"), "{}", o.stderr_tail);
        assert_eq!(classify_download_error(&o.stderr_tail), ErrorCode::RateLimited);
        assert_eq!(pacer.cooling_down("youtube"), Some(RATE_LIMIT_COOL_DOWN), "the cool-down was extended");

        let fresh = HostPacer::with_jitter(1, Duration::from_secs(8), || Duration::ZERO);
        paced(&fresh, "https://youtu.be/a", None, None, None, true, || async move { finished(true, "") }).await.unwrap();
        let t1 = tokio::time::Instant::now();
        assert!(paced(&fresh, "https://youtu.be/b", None, None, None, true, || async move { finished(true, "") }).await.unwrap().success);
        assert_eq!(tokio::time::Instant::now() - t1, Duration::from_secs(8), "the ordinary gap is waited");
    }

    /// The downloader's own probes: one that is not a job's reports the
    /// cool-down at once (the Test URL, the ladder, the benchmark, the checks);
    /// a job's waits it out.
    #[tokio::test(start_paused = true)]
    async fn a_downloader_that_is_not_a_jobs_reports_a_cool_down_and_a_jobs_waits_it_out() {
        let dir = tempfile::tempdir().unwrap();
        let pacer = HostPacer::with_jitter(1, Duration::from_secs(8), || Duration::ZERO);
        pacer.penalize("youtube", RATE_LIMIT_COOL_DOWN);
        let t0 = tokio::time::Instant::now();
        let e = Downloader::new(dir.path().join("no-yt-dlp.exe"), None, &pacer)
            .probe_info("https://youtu.be/abcdefghijk", Duration::from_secs(30))
            .await
            .unwrap_err();
        assert_eq!(tokio::time::Instant::now(), t0);
        assert!(e.contains("cooling down"), "{e}");
        assert_eq!(classify_download_error(&e), ErrorCode::RateLimited);
        // A job's probe waits the two minutes, then runs (no yt-dlp here: it fails to start).
        let e = Downloader::new(dir.path().join("no-yt-dlp.exe"), None, &pacer)
            .for_a_job()
            .probe_info("https://youtu.be/abcdefghijk", Duration::from_secs(30))
            .await
            .unwrap_err();
        assert_eq!(tokio::time::Instant::now() - t0, RATE_LIMIT_COOL_DOWN);
        assert!(e.contains("could not run yt-dlp"), "{e}");
    }
}
