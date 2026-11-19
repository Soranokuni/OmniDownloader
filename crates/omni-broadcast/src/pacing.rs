//! Per-platform pacing of yt-dlp runs (P11.4).
//!
//! A mail with several YouTube links used to start up to three yt-dlp runs at
//! YouTube at once, and YouTube answered the burst with its bot check («Sign in
//! to confirm you're not a bot»): half the jobs of a busy hour went to review
//! after one try, while the same links passed one at a time. Each platform
//! family now gets at most `per_host_concurrency` runs at a time, and two
//! starts on it are at least `min_gap` plus up to four seconds of jitter
//! apart. A run the platform refuses as a rate limit holds every new start
//! there for two minutes, so the jobs behind it wait instead of failing in turn.
//! The bot check reacts to the burst of page and API requests at the start of
//! a run, so a download gives its turn back at its first progress line (see
//! `downloader::paced`): a 70-minute show does not hold the platform while it
//! transfers. A probe holds its turn until it ends.
//!
//! Only the platform's key is ever logged, never a link. A link that is not on
//! a video platform has no key and is never paced.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// `pacing.per_host_concurrency` when the daemon sets none.
pub const DEFAULT_PER_HOST_CONCURRENCY: usize = 1;
/// `pacing.min_gap_secs` when the daemon sets none.
pub const DEFAULT_MIN_GAP_SECS: u64 = 8;
/// The most the jitter adds to the gap.
pub const MAX_JITTER: Duration = Duration::from_secs(4);
/// No new start on a platform for this long after it refused a run as a rate limit.
pub const RATE_LIMIT_COOL_DOWN: Duration = Duration::from_secs(120);
/// A wait longer than this is written to the job's history.
pub const NOTE_WAIT_OVER: Duration = Duration::from_secs(2);
/// A caller that is not a job ([`HostPacer::acquire_unless_cooling`]) does not
/// wait out a cool-down with more than this left: it is told at once.
pub const FAIL_FAST_COOL_DOWN_OVER: Duration = Duration::from_secs(5);

/// One key per platform family, for the hosts yt-dlp has its own extractor for.
const FAMILIES: &[(&str, &[&str])] = &[
    ("youtube", &["youtube.com", "youtu.be", "youtube-nocookie.com"]),
    ("x", &["x.com", "twitter.com"]),
    ("facebook", &["facebook.com", "fb.watch"]),
    ("instagram", &["instagram.com"]),
    ("tiktok", &["tiktok.com"]),
    ("vimeo", &["vimeo.com"]),
    ("dailymotion", &["dailymotion.com", "dai.ly"]),
    ("streamable", &["streamable.com"]),
];

/// The platform family `url` is on, matched by exact host or a subdomain of it
/// (`m.youtube.com`, `music.youtube.com`), never by substring: `notyoutube.com`
/// and `youtube.com.evil.example` are not YouTube. `None` for anything else.
pub fn platform_key(url: &str) -> Option<&'static str> {
    let host = crate::downloader::host_of(url);
    FAMILIES
        .iter()
        .find(|(_, hosts)| hosts.iter().any(|d| host == *d || host.ends_with(&format!(".{d}"))))
        .map(|(key, _)| *key)
}

/// The history line for a job that waited `waited` for its turn on `key`.
pub fn wait_line(key: &str, waited: Duration) -> String {
    format!("Σε σειρά για {key}: {}″ αναμονή", waited.as_secs_f64().round() as u64)
}

/// The error line of a run that was not started because `key` is cooling down
/// for `left`: it classifies as RATE_LIMITED ("try again later"), as the
/// refusal that set the cool-down did.
pub fn cooling_down_line(key: &str, left: Duration) -> String {
    format!(
        "ERROR: {key} is cooling down for {} s after it limited our requests; try again later",
        left.as_secs_f64().ceil() as u64
    )
}

/// The wait for a turn was cancelled (MCR's cancel, a lost lease, a stop).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cancelled while waiting for a turn on the platform")]
pub struct Cancelled;

/// Why [`HostPacer::acquire_unless_cooling`] gave no turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Cancelled,
    /// The platform is cooling down after a rate limit, for this long still.
    CoolingDown(Duration),
}

type Jitter = Box<dyn Fn() -> Duration + Send + Sync>;

#[derive(Default)]
struct KeyState {
    running: usize,
    /// No start before this: the last start's gap.
    not_before: Option<Instant>,
    /// No start before this either: a cool-down after a rate limit ([`HostPacer::penalize`]).
    cool_until: Option<Instant>,
    /// Tickets waiting, first come first served.
    queue: VecDeque<u64>,
}

#[derive(Default)]
struct State {
    keys: HashMap<&'static str, KeyState>,
    next_ticket: u64,
}

struct Inner {
    per_key: usize,
    gap: Duration,
    jitter: Jitter,
    state: Mutex<State>,
    /// Woken on every change a waiter may be waiting for.
    changed: Notify,
}

/// The daemon's pacer, one for every yt-dlp run in the process; a clone shares it.
#[derive(Clone)]
pub struct HostPacer(Arc<Inner>);

impl Default for HostPacer {
    fn default() -> Self {
        Self::new(DEFAULT_PER_HOST_CONCURRENCY, Duration::from_secs(DEFAULT_MIN_GAP_SECS))
    }
}

impl std::fmt::Debug for HostPacer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostPacer").field("per_key", &self.0.per_key).field("gap", &self.0.gap).finish()
    }
}

/// Up to [`MAX_JITTER`], from the sub-second part of the wall clock.
fn clock_jitter() -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    MAX_JITTER.mul_f64(f64::from(nanos) / 1e9)
}

impl HostPacer {
    /// At most `per_key` runs at a time per platform (at least one), starts
    /// `gap` plus jitter apart; a zero gap has no jitter either.
    pub fn new(per_key: usize, gap: Duration) -> Self {
        Self::with_jitter(per_key, gap, clock_jitter)
    }

    /// [`Self::new`] with the jitter from `jitter` (capped at [`MAX_JITTER`]).
    pub fn with_jitter(per_key: usize, gap: Duration, jitter: impl Fn() -> Duration + Send + Sync + 'static) -> Self {
        Self(Arc::new(Inner {
            per_key: per_key.max(1),
            gap,
            jitter: Box::new(jitter),
            state: Mutex::new(State::default()),
            changed: Notify::new(),
        }))
    }

    pub fn per_key(&self) -> usize {
        self.0.per_key
    }

    pub fn min_gap(&self) -> Duration {
        self.0.gap
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Runs holding a permit on `key` now.
    pub fn running(&self, key: &str) -> usize {
        self.state().keys.get(key).map_or(0, |k| k.running)
    }

    /// Wait for a turn on `key`: first come first served, at most the
    /// concurrency at a time, and no sooner than the gap after the last start
    /// or a cool-down allows. Holds no lock while it waits. Cancelled through
    /// `cancel` or by dropping the future, the waiter leaves the line and the
    /// next one moves up.
    pub async fn acquire(&self, key: &'static str, cancel: &CancellationToken) -> Result<PacePermit, Cancelled> {
        self.acquire_inner(key, cancel, false).await.map_err(|_| Cancelled)
    }

    /// [`Self::acquire`] for a caller that is not a job (the repair ladder, the
    /// admin's Test URL, the benchmark, canaries, the self-check, a jar test):
    /// the ordinary gap is waited, but a cool-down with more than
    /// [`FAIL_FAST_COOL_DOWN_OVER`] left is reported at once, before the turn
    /// and while waiting for it, instead of waited out against the caller's own
    /// time limit.
    pub async fn acquire_unless_cooling(&self, key: &'static str, cancel: &CancellationToken) -> Result<PacePermit, Refused> {
        self.acquire_inner(key, cancel, true).await
    }

    /// How long `key` still cools down after a rate limit; `None` when it does not.
    pub fn cooling_down(&self, key: &str) -> Option<Duration> {
        let now = Instant::now();
        self.state().keys.get(key).and_then(|k| k.cool_until).filter(|t| *t > now).map(|t| t - now)
    }

    async fn acquire_inner(&self, key: &'static str, cancel: &CancellationToken, fail_fast: bool) -> Result<PacePermit, Refused> {
        let asked = Instant::now();
        let mut ticket = {
            let mut st = self.state();
            let id = st.next_ticket;
            st.next_ticket += 1;
            st.keys.entry(key).or_default().queue.push_back(id);
            Ticket { pacer: self.clone(), key, id, granted: false }
        };
        loop {
            // A cancelled caller never gets a turn: a start it would abort
            // would still push every other job's next start a gap away.
            if cancel.is_cancelled() {
                return Err(Refused::Cancelled);
            }
            if fail_fast {
                if let Some(left) = self.cooling_down(key).filter(|l| *l > FAIL_FAST_COOL_DOWN_OVER) {
                    return Err(Refused::CoolingDown(left));
                }
            }
            // Registered before the state is read, so no change between the
            // read and the wait is missed.
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let until = match self.try_start(key, ticket.id) {
                Ok(()) => {
                    ticket.granted = true;
                    return Ok(PacePermit { pacer: self.clone(), key, waited: asked.elapsed() });
                }
                Err(until) => until,
            };
            let sleep = async {
                match until {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                // A cancel that fires together with a wake-up wins.
                biased;
                _ = cancel.cancelled() => return Err(Refused::Cancelled),
                _ = &mut changed => {}
                _ = sleep => {}
            }
        }
    }

    /// Start ticket `id` now if it is its turn; otherwise when to look again
    /// (`None`: when something changes).
    fn try_start(&self, key: &'static str, id: u64) -> Result<(), Option<Instant>> {
        let mut st = self.state();
        let ks = st.keys.entry(key).or_default();
        if ks.queue.front() != Some(&id) || ks.running >= self.0.per_key {
            return Err(None);
        }
        let now = Instant::now();
        if let Some(t) = ks.not_before.max(ks.cool_until).filter(|t| *t > now) {
            return Err(Some(t));
        }
        ks.queue.pop_front();
        ks.running += 1;
        if !self.0.gap.is_zero() {
            let next = now + self.0.gap + (self.0.jitter)().min(MAX_JITTER);
            ks.not_before = Some(ks.not_before.map_or(next, |t| t.max(next)));
        }
        drop(st);
        // The next in line may start now (concurrency above one) or learn its time.
        self.0.changed.notify_waiters();
        Ok(())
    }

    /// No new start on `key` for `for_how_long` from now (never shortens an
    /// earlier hold). Waiters wait longer; running runs are not touched.
    pub fn penalize(&self, key: &'static str, for_how_long: Duration) {
        let until = Instant::now() + for_how_long;
        {
            let mut st = self.state();
            let ks = st.keys.entry(key).or_default();
            ks.cool_until = Some(ks.cool_until.map_or(until, |t| t.max(until)));
        }
        self.0.changed.notify_waiters();
    }

    fn release(&self, key: &'static str) {
        {
            let mut st = self.state();
            if let Some(ks) = st.keys.get_mut(key) {
                ks.running = ks.running.saturating_sub(1);
            }
        }
        self.0.changed.notify_waiters();
    }

    fn leave(&self, key: &'static str, id: u64) {
        {
            let mut st = self.state();
            if let Some(ks) = st.keys.get_mut(key) {
                ks.queue.retain(|t| *t != id);
            }
        }
        self.0.changed.notify_waiters();
    }
}

/// A place in a key's line; leaves the line when dropped before its turn.
struct Ticket {
    pacer: HostPacer,
    key: &'static str,
    id: u64,
    granted: bool,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if !self.granted {
            self.pacer.leave(self.key, self.id);
        }
    }
}

/// A turn on a platform: held while the run lasts (a download's only until its
/// transfer begins), given back when dropped
/// (on every way out, a panic included).
pub struct PacePermit {
    pacer: HostPacer,
    key: &'static str,
    waited: Duration,
}

impl PacePermit {
    pub fn key(&self) -> &'static str {
        self.key
    }

    /// How long the turn was waited for.
    pub fn waited(&self) -> Duration {
        self.waited
    }
}

impl Drop for PacePermit {
    fn drop(&mut self) {
        self.pacer.release(self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAP: Duration = Duration::from_secs(8);

    fn no_jitter(per_key: usize, gap: Duration) -> HostPacer {
        HostPacer::with_jitter(per_key, gap, || Duration::ZERO)
    }

    /// Two starts on one key are the gap apart (plus the jitter), and a
    /// permit already dropped does not shorten it.
    #[tokio::test(start_paused = true)]
    async fn two_starts_on_one_key_are_the_gap_and_the_jitter_apart() {
        let never = CancellationToken::new();
        let p = HostPacer::with_jitter(2, GAP, || Duration::from_secs(3));
        let t0 = Instant::now();
        let a = p.acquire("youtube", &never).await.unwrap();
        assert_eq!(a.waited(), Duration::ZERO);
        drop(a);
        let b = p.acquire("youtube", &never).await.unwrap();
        assert_eq!(Instant::now() - t0, GAP + Duration::from_secs(3));
        assert_eq!(b.waited(), GAP + Duration::from_secs(3));
    }

    /// The default jitter stays within its four seconds.
    #[test]
    fn the_clock_jitter_is_at_most_four_seconds() {
        for _ in 0..100 {
            assert!(clock_jitter() <= MAX_JITTER);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn two_keys_do_not_wait_for_each_other() {
        let never = CancellationToken::new();
        let p = no_jitter(1, GAP);
        let t0 = Instant::now();
        let _yt = p.acquire("youtube", &never).await.unwrap();
        let _x = p.acquire("x", &never).await.unwrap();
        assert_eq!(Instant::now(), t0);
        assert_eq!((p.running("youtube"), p.running("x")), (1, 1));
    }

    /// Concurrency 1: the second waits for the first permit to drop, even
    /// past the gap. Concurrency 2 (no gap): both start at once.
    #[tokio::test(start_paused = true)]
    async fn with_one_the_second_waits_for_the_first_and_with_two_both_start() {
        let never = CancellationToken::new();
        let p = no_jitter(1, Duration::from_secs(1));
        let first = p.acquire("youtube", &never).await.unwrap();
        let t0 = Instant::now();
        let p2 = p.clone();
        let second = tokio::spawn(async move {
            let never = CancellationToken::new();
            p2.acquire("youtube", &never).await.map(|q| q.waited())
        });
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(!second.is_finished(), "started while the first still ran");
        drop(first);
        let waited = second.await.unwrap().unwrap();
        assert_eq!(waited, Duration::from_secs(60));
        assert_eq!(Instant::now() - t0, Duration::from_secs(60));

        let p = no_jitter(2, Duration::ZERO);
        let t0 = Instant::now();
        let _a = p.acquire("youtube", &never).await.unwrap();
        let _b = p.acquire("youtube", &never).await.unwrap();
        assert_eq!(Instant::now(), t0);
        assert_eq!(p.running("youtube"), 2);
    }

    /// A cool-down holds the next start for its length, and a waiter already
    /// in line waits it out too.
    #[tokio::test(start_paused = true)]
    async fn penalize_delays_the_next_start_by_the_given_time() {
        let never = CancellationToken::new();
        let p = no_jitter(1, GAP);
        drop(p.acquire("youtube", &never).await.unwrap());
        let t0 = Instant::now();
        p.penalize("youtube", RATE_LIMIT_COOL_DOWN);
        let q = p.acquire("youtube", &never).await.unwrap();
        assert_eq!(Instant::now() - t0, RATE_LIMIT_COOL_DOWN);
        drop(q);

        // A waiter sleeping towards the gap is held by a cool-down set meanwhile.
        let t1 = Instant::now();
        let p2 = p.clone();
        let waiter = tokio::spawn(async move { p2.acquire("youtube", &CancellationToken::new()).await.map(|q| q.waited()) });
        tokio::time::sleep(Duration::from_secs(1)).await;
        p.penalize("youtube", Duration::from_secs(30));
        waiter.await.unwrap().unwrap();
        assert_eq!(Instant::now() - t1, Duration::from_secs(31));
        // A shorter cool-down never shortens a longer hold.
        p.penalize("youtube", Duration::from_secs(100));
        p.penalize("youtube", Duration::from_secs(5));
        let t2 = Instant::now();
        drop(p.acquire("youtube", &never).await.unwrap());
        assert_eq!(Instant::now() - t2, Duration::from_secs(100));
    }

    /// A cancelled waiter returns at once and frees its place; the one behind
    /// it still gets its turn.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_waiter_returns_promptly_and_the_next_still_gets_its_turn() {
        let never = CancellationToken::new();
        let p = no_jitter(1, Duration::ZERO);
        let holder = p.acquire("youtube", &never).await.unwrap();
        let cancel = CancellationToken::new();
        let (pa, ca) = (p.clone(), cancel.clone());
        let a = tokio::spawn(async move { pa.acquire("youtube", &ca).await.map(|_| ()) });
        tokio::task::yield_now().await;
        let pb = p.clone();
        let b = tokio::spawn(async move { pb.acquire("youtube", &CancellationToken::new()).await.map(|q| q.waited()) });
        tokio::task::yield_now().await;
        let t0 = Instant::now();
        cancel.cancel();
        assert_eq!(a.await.unwrap(), Err(Cancelled));
        assert_eq!(Instant::now(), t0, "the cancelled waiter did not return at once");
        drop(holder);
        b.await.unwrap().unwrap();
        assert_eq!(p.running("youtube"), 0);
        assert!(p.state().keys["youtube"].queue.is_empty(), "a ticket was left in the line");

        // A waiter dropped (its job's select chose the cancel branch) leaves the line too.
        let holder = p.acquire("youtube", &never).await.unwrap();
        let dropped = tokio::time::timeout(Duration::from_secs(1), p.acquire("youtube", &never)).await;
        assert!(dropped.is_err());
        drop(holder);
        assert!(p.state().keys["youtube"].queue.is_empty(), "a dropped waiter kept its place");
        let t1 = Instant::now();
        drop(p.acquire("youtube", &never).await.unwrap());
        assert_eq!(Instant::now(), t1);
    }

    /// No gap: starts are never spaced, and only the concurrency applies.
    #[tokio::test(start_paused = true)]
    async fn a_zero_gap_never_waits_for_the_gap() {
        let never = CancellationToken::new();
        let p = HostPacer::with_jitter(1, Duration::ZERO, || MAX_JITTER);
        let t0 = Instant::now();
        for _ in 0..5 {
            drop(p.acquire("youtube", &never).await.unwrap());
        }
        assert_eq!(Instant::now(), t0);
    }

    /// Three waiters start in the order they asked.
    #[tokio::test(start_paused = true)]
    async fn three_waiters_start_in_the_order_they_asked() {
        let never = CancellationToken::new();
        let p = no_jitter(1, Duration::from_secs(2));
        let holder = p.acquire("youtube", &never).await.unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for n in 0..3 {
            let (p, order) = (p.clone(), order.clone());
            tasks.push(tokio::spawn(async move {
                let permit = p.acquire("youtube", &CancellationToken::new()).await.unwrap();
                order.lock().unwrap().push(n);
                tokio::time::sleep(Duration::from_secs(1)).await;
                drop(permit);
            }));
            // Each is in line before the next asks.
            tokio::task::yield_now().await;
        }
        drop(holder);
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
    }

    /// A permit dropped on an error path (or a panic) frees the key.
    #[tokio::test(start_paused = true)]
    async fn a_permit_dropped_on_an_error_path_frees_the_key() {
        let p = no_jitter(1, Duration::ZERO);
        async fn failing_run(p: &HostPacer) -> anyhow::Result<()> {
            let _permit = p.acquire("x", &CancellationToken::new()).await?;
            anyhow::bail!("yt-dlp could not start")
        }
        assert!(failing_run(&p).await.is_err());
        assert_eq!(p.running("x"), 0);
        let p2 = p.clone();
        let panicked = tokio::spawn(async move {
            let _permit = p2.acquire("x", &CancellationToken::new()).await.unwrap();
            panic!("a run that panics");
        })
        .await;
        assert!(panicked.is_err());
        assert_eq!(p.running("x"), 0);
        let t0 = Instant::now();
        drop(p.acquire("x", &CancellationToken::new()).await.unwrap());
        assert_eq!(Instant::now(), t0);
    }

    /// Review round 1: a token already cancelled never gets a turn, and the
    /// start it never made does not push the next one a gap away.
    #[tokio::test(start_paused = true)]
    async fn an_already_cancelled_token_never_gets_a_turn_and_does_not_move_the_gap() {
        let p = no_jitter(1, GAP);
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert_eq!(p.acquire("youtube", &cancelled).await.err(), Some(Cancelled));
        assert_eq!(p.acquire_unless_cooling("youtube", &cancelled).await.err(), Some(Refused::Cancelled));
        assert_eq!(p.running("youtube"), 0);
        assert!(p.state().keys["youtube"].queue.is_empty());
        let t0 = Instant::now();
        drop(p.acquire("youtube", &CancellationToken::new()).await.unwrap());
        assert_eq!(Instant::now(), t0, "the refused start moved the gap");
    }

    /// Review round 1: a caller that is not a job is told of a cool-down at
    /// once (before and while it waits), but waits through the ordinary gap and
    /// a cool-down with 5 s or less left; a job waits the cool-down out.
    #[tokio::test(start_paused = true)]
    async fn a_non_job_caller_is_told_of_a_cool_down_at_once_and_a_job_waits_it_out() {
        let never = CancellationToken::new();
        let p = no_jitter(1, GAP);
        p.penalize("youtube", RATE_LIMIT_COOL_DOWN);
        let t0 = Instant::now();
        assert_eq!(p.acquire_unless_cooling("youtube", &never).await.err(), Some(Refused::CoolingDown(RATE_LIMIT_COOL_DOWN)));
        assert_eq!(Instant::now(), t0, "it waited");
        assert_eq!(p.cooling_down("youtube"), Some(RATE_LIMIT_COOL_DOWN));
        assert_eq!(p.cooling_down("x"), None);
        // Another platform is not cooling down.
        drop(p.acquire_unless_cooling("x", &never).await.unwrap());
        // A job waits it out.
        drop(p.acquire("youtube", &never).await.unwrap());
        assert_eq!(Instant::now() - t0, RATE_LIMIT_COOL_DOWN);
        assert_eq!(p.cooling_down("youtube"), None);
        // The ordinary gap after that start is waited.
        let t1 = Instant::now();
        drop(p.acquire_unless_cooling("youtube", &never).await.unwrap());
        assert_eq!(Instant::now() - t1, GAP);
        // 5 s or less of a cool-down left: waited too.
        p.penalize("youtube", Duration::from_secs(5));
        let t2 = Instant::now();
        drop(p.acquire_unless_cooling("youtube", &never).await.unwrap());
        assert_eq!(Instant::now() - t2, GAP, "the gap after the last start is the longer wait here");
        // A cool-down set while it waits for the gap: told then, not at its end.
        let p2 = p.clone();
        let waiter = tokio::spawn(async move { p2.acquire_unless_cooling("youtube", &CancellationToken::new()).await.map(|_| ()) });
        tokio::time::sleep(Duration::from_secs(1)).await;
        p.penalize("youtube", RATE_LIMIT_COOL_DOWN);
        let t3 = Instant::now();
        assert_eq!(waiter.await.unwrap(), Err(Refused::CoolingDown(RATE_LIMIT_COOL_DOWN)));
        assert_eq!(Instant::now(), t3);
        assert!(p.state().keys["youtube"].queue.is_empty(), "the refused waiter left its ticket");
    }

    #[test]
    fn the_cooling_down_line_classifies_as_rate_limited() {
        let line = cooling_down_line("youtube", Duration::from_millis(94_200));
        assert_eq!(line, "ERROR: youtube is cooling down for 95 s after it limited our requests; try again later");
        assert_eq!(crate::errors::classify_download_error(&line), crate::errors::ErrorCode::RateLimited);
    }

    /// Exact hosts and their subdomains, never a substring.
    #[test]
    fn platform_keys_match_hosts_exactly() {
        for (url, key) in [
            ("https://www.youtube.com/watch?v=abc", Some("youtube")),
            ("https://youtu.be/abc", Some("youtube")),
            ("https://m.youtube.com/watch?v=abc", Some("youtube")),
            ("https://music.youtube.com/watch?v=abc", Some("youtube")),
            ("https://www.youtube-nocookie.com/embed/abc", Some("youtube")),
            ("https://x.com/someone/status/1", Some("x")),
            ("https://twitter.com/someone/status/1", Some("x")),
            ("https://mobile.twitter.com/someone/status/1", Some("x")),
            ("https://www.facebook.com/watch/?v=1", Some("facebook")),
            ("https://fb.watch/abc/", Some("facebook")),
            ("https://www.instagram.com/reel/abc/", Some("instagram")),
            ("https://www.tiktok.com/@someone/video/1", Some("tiktok")),
            ("https://vimeo.com/1", Some("vimeo")),
            ("https://www.dailymotion.com/video/x1", Some("dailymotion")),
            ("https://dai.ly/x1", Some("dailymotion")),
            ("https://streamable.com/abc", Some("streamable")),
            ("HTTPS://WWW.YOUTUBE.COM/watch?v=abc", Some("youtube")),
            ("https://user@www.youtube.com:443/watch?v=abc", Some("youtube")),
            ("https://notyoutube.com/watch?v=abc", None),
            ("https://youtube.com.evil.example/watch?v=abc", None),
            ("https://www.news.example/youtube.com/story", None),
            ("https://notx.com/a", None),
            ("https://www.news.example/story/1", None),
            ("https://cdn.news.example/video.m3u8", None),
            ("attachment://m1@example.test/1", None),
        ] {
            assert_eq!(platform_key(url), key, "{url}");
        }
    }

    #[test]
    fn the_history_line_names_the_key_and_the_seconds() {
        assert_eq!(wait_line("youtube", Duration::from_millis(12_400)), "Σε σειρά για youtube: 12″ αναμονή");
        assert_eq!(wait_line("x", Duration::from_millis(2_600)), "Σε σειρά για x: 3″ αναμονή");
    }
}
