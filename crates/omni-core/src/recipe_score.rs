//! Scoring and lifecycle rules for the recipe store (plan P10.10).
//!
//! Pure: no SQL, no clock (`now` is always a parameter), no randomness. The
//! repository stores the numbers; every decision about them is made here so it
//! can be tested without a database.
//!
//! Each (recipe, domain) pair carries a Beta(alpha, beta) belief that the
//! recipe finds the right video on that site. Evidence fades with a 30-day
//! half-life, so a site that changes its pages is relearned instead of being
//! trusted for ever on old successes. Ranking uses the lower confidence bound,
//! so a recipe with one lucky success does not outrank one with a long record.

use chrono::{DateTime, Duration, Utc};
use std::cmp::Ordering;

/// Non-retired recipes allowed per (site | player, scope_key).
pub const SCOPE_CAP: usize = 4;
/// Non-retired, non-shipped recipes in the whole store.
pub const NONSHIPPED_LIVE_CAP: usize = 256;
/// Non-shipped recipes in any state.
pub const NONSHIPPED_ROW_CAP: usize = 320;
/// Rows in `recipe_sites`.
pub const SITE_ROW_CAP: usize = 4096;
pub const TOMBSTONE_CAP: usize = 2048;
/// "These steps were rejected on this domain" entries.
pub const BAR_CAP: usize = 2048;
/// Canonical recipe JSON, in bytes.
pub const MAX_RECIPE_JSON: usize = 4096;

/// Half-life of evidence.
const HALF_LIFE_DAYS: f64 = 30.0;
const Z_LCB: f64 = 1.645;
/// A recipe unused for this long is retired as idle.
pub const IDLE_DAYS: i64 = 90;
/// All-suspended for this long retires a recipe.
pub const SUSPENDED_DAYS: i64 = 30;
/// Retired recipes are deleted after this long.
pub const RETIRED_KEEP_DAYS: i64 = 90;
/// Site rows with no event for this long are deleted.
pub const STALE_ROW_DAYS: i64 = 180;
pub const TOMBSTONE_DAYS: i64 = 180;
pub const BAR_DAYS: i64 = 180;

macro_rules! text_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
            pub fn parse(s: &str) -> Option<Self> {
                match s { $($text => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

text_enum!(
    /// State of one (recipe, domain) row.
    SiteState { Candidate => "candidate", Active => "active", Suspended => "suspended" }
);
text_enum!(SuspendReason { Rejected => "rejected", Failing => "failing" });
text_enum!(
    /// State of a recipe; `Retired` is only ever set by an explicit transition.
    RecipeState { Candidate => "candidate", Active => "active", Suspended => "suspended", Retired => "retired" }
);
text_enum!(RetiredReason {
    Idle => "idle", Failing => "failing", Rejected => "rejected",
    Evicted => "evicted", Superseded => "superseded", Admin => "admin"
});
text_enum!(Origin { Shipped => "shipped", Admin => "admin", Llm => "llm" });
text_enum!(ScopeKind { Site => "site", Player => "player", Generic => "generic" });

impl RetiredReason {
    /// A recipe retired or tombstoned for this reason never comes back by itself.
    pub fn refuses_return(self) -> bool {
        matches!(self, RetiredReason::Rejected | RetiredReason::Admin)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SiteRow {
    pub alpha: f64,
    pub beta: f64,
    /// Last event; the decay reference.
    pub scored_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub approvals: i64,
    pub rejections: i64,
    pub uses: i64,
    pub consecutive_fail: i64,
    pub pass_streak: i64,
    pub state: SiteState,
    pub suspended_reason: Option<SuspendReason>,
    pub suspended_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub last_ok_at: Option<DateTime<Utc>>,
}

impl SiteRow {
    /// The prior: Beta(1, 1), a candidate.
    pub fn new(now: DateTime<Utc>) -> Self {
        SiteRow {
            alpha: 1.0,
            beta: 1.0,
            scored_at: now,
            created_at: now,
            approvals: 0,
            rejections: 0,
            uses: 0,
            consecutive_fail: 0,
            pass_streak: 0,
            state: SiteState::Candidate,
            suspended_reason: None,
            suspended_at: None,
            last_used_at: None,
            last_ok_at: None,
        }
    }

    pub fn is_suspended(&self) -> bool {
        self.state == SiteState::Suspended
    }
}

/// Evidence above the prior fades with a 30-day half-life. A negative gap
/// (clock moved back) counts as none.
pub fn decay(alpha: f64, beta: f64, dt_days: f64) -> (f64, f64) {
    let dt = if dt_days > 0.0 { dt_days } else { 0.0 };
    let f = (-dt / HALF_LIFE_DAYS).exp2();
    (1.0 + (alpha - 1.0) * f, 1.0 + (beta - 1.0) * f)
}

/// Mean minus 1.645 standard deviations of Beta(alpha, beta).
pub fn lcb(alpha: f64, beta: f64) -> f64 {
    let n = alpha + beta;
    let mean = alpha / n;
    let var = alpha * beta / (n * n * (n + 1.0));
    mean - Z_LCB * var.sqrt()
}

fn days_between(from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
    (to - from).num_milliseconds() as f64 / 86_400_000.0
}

fn decayed_to(row: &SiteRow, now: DateTime<Utc>) -> (f64, f64) {
    decay(row.alpha, row.beta, days_between(row.scored_at, now))
}

pub fn site_rank(row: &SiteRow, now: DateTime<Utc>) -> f64 {
    let (a, b) = decayed_to(row, now);
    lcb(a, b)
}

/// One score for a recipe across all its sites; no rows gives the prior's.
pub fn pooled_rank(rows: &[SiteRow], now: DateTime<Utc>) -> f64 {
    let (mut a, mut b) = (1.0, 1.0);
    for r in rows {
        let (ra, rb) = decayed_to(r, now);
        a += ra - 1.0;
        b += rb - 1.0;
    }
    lcb(a, b)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Approval,
    Rejection,
    CanaryPass,
    CanaryFail,
    NoCandidate,
    AutoDelivered,
}

/// Failures in a row that suspend a recipe on a site.
const SUSPEND_AFTER_FAILS: i64 = 3;
/// Canary passes in a row that promote a candidate or lift a failing suspension.
const PROMOTE_AFTER_PASSES: i64 = 2;

pub fn apply(row: &SiteRow, event: Event, now: DateTime<Utc>) -> SiteRow {
    let mut r = row.clone();
    let (a, b) = decayed_to(row, now);
    r.alpha = a;
    r.beta = b;
    r.scored_at = now;

    // Approval and a clean automatic delivery lift the same way.
    let lift = |r: &mut SiteRow| match (r.state, r.suspended_reason) {
        (SiteState::Candidate, _) => r.state = SiteState::Active,
        (SiteState::Suspended, Some(SuspendReason::Failing)) => {
            r.state = SiteState::Active;
            r.suspended_reason = None;
            r.suspended_at = None;
        }
        _ => {}
    };

    match event {
        Event::Approval | Event::AutoDelivered => {
            if event == Event::Approval {
                r.alpha += 2.0;
                r.approvals += 1;
            } else {
                r.alpha += 1.0;
            }
            r.uses += 1;
            r.last_used_at = Some(now);
            r.last_ok_at = Some(now);
            r.consecutive_fail = 0;
            lift(&mut r);
        }
        Event::Rejection => {
            r.beta += 5.0;
            r.rejections += 1;
            r.uses += 1;
            r.last_used_at = Some(now);
            r.pass_streak = 0;
            r.consecutive_fail = 0;
            let already = r.state == SiteState::Suspended && r.suspended_reason == Some(SuspendReason::Rejected);
            if !already {
                r.suspended_at = Some(now);
            }
            r.state = SiteState::Suspended;
            r.suspended_reason = Some(SuspendReason::Rejected);
        }
        Event::CanaryPass => {
            r.alpha += 0.5;
            r.pass_streak += 1;
            r.consecutive_fail = 0;
            r.last_ok_at = Some(now);
            if r.pass_streak >= PROMOTE_AFTER_PASSES {
                lift(&mut r);
            }
        }
        Event::CanaryFail | Event::NoCandidate => {
            r.beta += 1.0;
            if event == Event::NoCandidate {
                r.uses += 1;
                r.last_used_at = Some(now);
            }
            r.consecutive_fail += 1;
            r.pass_streak = 0;
            if r.consecutive_fail >= SUSPEND_AFTER_FAILS && r.state != SiteState::Suspended {
                r.state = SiteState::Suspended;
                r.suspended_reason = Some(SuspendReason::Failing);
                r.suspended_at = Some(now);
            }
        }
    }
    r
}

/// State of a non-retired recipe from its site rows. A retired recipe stays
/// retired: retirement is never derived.
pub fn recipe_state(origin: Origin, current: RecipeState, site_states: &[SiteState]) -> RecipeState {
    if current == RecipeState::Retired {
        return RecipeState::Retired;
    }
    if origin == Origin::Shipped || site_states.contains(&SiteState::Active) {
        return RecipeState::Active;
    }
    if site_states.is_empty() || site_states.contains(&SiteState::Candidate) {
        return RecipeState::Candidate;
    }
    RecipeState::Suspended
}

/// What ordering needs from one item.
#[derive(Debug, Clone, PartialEq)]
pub struct RankKey {
    pub rank: f64,
    pub last_ok_at: Option<DateTime<Utc>>,
    pub hash: String,
}

/// Best first: rank descending, most recent success first (never-succeeded
/// last), then hash ascending. Total, so any input order gives the same output.
pub fn rank_cmp(a: &RankKey, b: &RankKey) -> Ordering {
    b.rank
        .total_cmp(&a.rank)
        .then_with(|| match (a.last_ok_at, b.last_ok_at) {
            (Some(x), Some(y)) => y.cmp(&x),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        })
        .then_with(|| a.hash.cmp(&b.hash))
}

/// Sort `items` best first. Ranks are computed by the caller at one `now`.
pub fn rank_order<T>(items: &mut [T], key: impl Fn(&T) -> RankKey) {
    items.sort_by(|a, b| rank_cmp(&key(a), &key(b)));
}

#[derive(Debug, Clone)]
pub struct RecipeMeta {
    pub origin: Origin,
    pub state: RecipeState,
    pub created_at: DateTime<Utc>,
    /// When a retired recipe was brought back; its idle clock starts there.
    pub revived_at: Option<DateTime<Utc>>,
}

/// The time-driven retirement a recipe is due, if any. Shipped and already
/// retired recipes are never due. Idle is checked first.
pub fn due_retirement(recipe: &RecipeMeta, rows: &[SiteRow], now: DateTime<Utc>) -> Option<RetiredReason> {
    if recipe.origin == Origin::Shipped || recipe.state == RecipeState::Retired {
        return None;
    }
    let last_use = rows
        .iter()
        .filter_map(|r| r.last_used_at)
        .chain([recipe.created_at])
        .chain(recipe.revived_at)
        .max()
        .unwrap_or(recipe.created_at);
    if last_use <= now - Duration::days(IDLE_DAYS) {
        return Some(RetiredReason::Idle);
    }
    if !rows.is_empty() && rows.iter().all(|r| r.is_suspended()) {
        let newest = rows.iter().filter_map(|r| r.suspended_at).max()?;
        if newest <= now - Duration::days(SUSPENDED_DAYS) {
            return Some(if rows.iter().any(|r| r.suspended_reason == Some(SuspendReason::Rejected)) {
                RetiredReason::Rejected
            } else {
                RetiredReason::Failing
            });
        }
    }
    None
}

/// A non-retired site recipe as the promotion planner sees it.
#[derive(Debug, Clone)]
pub struct PromoSource {
    pub id: i64,
    pub scope_key: String,
    pub family: Option<String>,
    pub origin: Origin,
    /// Its row for its own domain (the scope key) is active.
    pub own_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Promotion {
    pub source_ids: Vec<i64>,
    pub origin: Origin,
}

/// `sources` all share one steps hash. A shared player recipe is worth making
/// when two different sites, both of the player family `family`, have each
/// proven the same steps on their own domain.
pub fn plan_promotion(sources: &[PromoSource], family: &str) -> Option<Promotion> {
    let same: Vec<&PromoSource> = sources.iter().filter(|s| s.family.as_deref() == Some(family)).collect();
    let mut keys: Vec<&str> = same.iter().filter(|s| s.own_active).map(|s| s.scope_key.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    if keys.len() < 2 {
        return None;
    }
    let origin = if same.iter().any(|s| s.origin == Origin::Admin) { Origin::Admin } else { Origin::Llm };
    let mut source_ids: Vec<i64> = same.iter().map(|s| s.id).collect();
    source_ids.sort_unstable();
    Some(Promotion { source_ids, origin })
}

fn restriction(r: &SiteRow) -> u8 {
    match (r.state, r.suspended_reason) {
        (SiteState::Suspended, Some(SuspendReason::Rejected)) => 3,
        (SiteState::Suspended, _) => 2,
        (SiteState::Candidate, _) => 1,
        (SiteState::Active, _) => 0,
    }
}

fn later<T: Ord + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// Two rows for one domain become one: evidence adds, counters add, times take
/// the latest (creation the earliest), the more restrictive state wins.
pub fn merge_rows(a: &SiteRow, b: &SiteRow, now: DateTime<Utc>) -> SiteRow {
    let (aa, ab) = decayed_to(a, now);
    let (ba, bb) = decayed_to(b, now);
    let pick = if restriction(b) > restriction(a) { b } else { a };
    let both_suspended_alike = a.state == b.state && a.suspended_reason == b.suspended_reason && a.is_suspended();
    SiteRow {
        alpha: 1.0 + (aa - 1.0) + (ba - 1.0),
        beta: 1.0 + (ab - 1.0) + (bb - 1.0),
        scored_at: now,
        created_at: a.created_at.min(b.created_at),
        approvals: a.approvals + b.approvals,
        rejections: a.rejections + b.rejections,
        uses: a.uses + b.uses,
        consecutive_fail: a.consecutive_fail + b.consecutive_fail,
        pass_streak: a.pass_streak + b.pass_streak,
        state: pick.state,
        suspended_reason: pick.suspended_reason,
        suspended_at: if both_suspended_alike { later(a.suspended_at, b.suspended_at) } else { pick.suspended_at },
        last_used_at: later(a.last_used_at, b.last_used_at),
        last_ok_at: later(a.last_ok_at, b.last_ok_at),
    }
}

// Types the repository exchanges with its callers.

#[derive(Debug, Clone)]
pub struct NewRecipe {
    pub scope_kind: ScopeKind,
    pub scope_key: String,
    /// Player family of the page a site recipe came from; `None` = unknown.
    pub family: Option<String>,
    pub recipe_json: String,
    /// Scope plus recipe.
    pub hash: String,
    /// The steps alone.
    pub steps_hash: String,
    pub dsl_version: u32,
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted(i64),
    Merged(i64),
    Revived(i64),
    /// The reason it may not come back (`rejected` or `admin`).
    Refused(String),
    OverCap,
}

#[derive(Debug, Clone)]
pub struct RankedRecipe {
    pub id: i64,
    pub hash: String,
    pub steps_hash: String,
    pub scope_kind: ScopeKind,
    pub scope_key: String,
    pub origin: Origin,
    pub recipe_json: String,
    pub dsl_version: u32,
    /// This domain's row, when it has one.
    pub site_state: Option<SiteState>,
    pub rank: f64,
}

#[derive(Debug, Clone)]
pub struct ShippedRecipe {
    pub name: String,
    pub hash: String,
    pub steps_hash: String,
    pub recipe_json: String,
    pub dsl_version: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecipeGcReport {
    pub stale_rows: usize,
    pub retired_idle: usize,
    pub retired_failing: usize,
    pub retired_rejected: usize,
    pub deleted_recipes: usize,
    pub tombstones_expired: usize,
    pub tombstones_trimmed: usize,
    pub bars_expired: usize,
    pub bars_trimmed: usize,
    pub rows_trimmed: usize,
}

impl RecipeGcReport {
    pub fn changed(&self) -> bool {
        *self != RecipeGcReport::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecipeCounts {
    /// Non-retired, non-shipped.
    pub live: usize,
    /// Retired, non-shipped.
    pub retired: usize,
    pub shipped: usize,
    pub sites: usize,
    pub tombstones: usize,
    pub bars: usize,
    pub json_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap()
    }
    fn days(n: i64) -> Duration {
        Duration::days(n)
    }
    const E: f64 = 1e-9;

    #[test]
    fn decay_goldens() {
        let (a, b) = decay(5.0, 3.0, 0.0);
        assert!((a - 5.0).abs() < E && (b - 3.0).abs() < E);
        let (a, b) = decay(5.0, 3.0, 30.0);
        assert!((a - 3.0).abs() < E && (b - 2.0).abs() < E);
        let (a, b) = decay(5.0, 3.0, 60.0);
        assert!((a - 2.0).abs() < E && (b - 1.5).abs() < E);
        let f = 2f64.powf(-0.25);
        let (a, b) = decay(5.0, 3.0, 7.5);
        assert!((a - (1.0 + 4.0 * f)).abs() < E, "{a}");
        assert!((b - (1.0 + 2.0 * f)).abs() < E, "{b}");
        // 4.363585661014858 and 2.681792830507429
        assert!((a - 4.363585661014858).abs() < E && (b - 2.681792830507429).abs() < E);
        // the prior never moves; a negative gap is no decay
        assert_eq!(decay(1.0, 1.0, 100.0), (1.0, 1.0));
        let (a, b) = decay(5.0, 3.0, -10.0);
        assert!((a - 5.0).abs() < E && (b - 3.0).abs() < E);
    }

    #[test]
    fn lcb_goldens() {
        let g = |a: f64, b: f64| {
            let n = a + b;
            a / n - 1.645 * (a * b / (n * n * (n + 1.0))).sqrt()
        };
        assert!((lcb(1.0, 1.0) - (0.5 - 1.645 * (1.0f64 / 12.0).sqrt())).abs() < E);
        assert!((lcb(1.0, 1.0) - 0.02512940359153).abs() < E);
        assert!((lcb(3.0, 1.0) - (0.75 - 1.645 * (0.0375f64).sqrt())).abs() < E);
        assert!((lcb(1.0, 6.0) - (1.0 / 7.0 - 1.645 * (6.0f64 / (49.0 * 8.0)).sqrt())).abs() < E);
        assert!((lcb(10.0, 10.0) - (0.5 - 1.645 * (100.0f64 / (400.0 * 21.0)).sqrt())).abs() < E);
        for (a, b) in [(1.0, 1.0), (3.0, 1.0), (1.0, 6.0), (10.0, 10.0), (2.5, 7.5)] {
            assert!((lcb(a, b) - g(a, b)).abs() < E);
        }
        // a long record outranks one lucky success
        assert!(lcb(11.0, 1.0) > lcb(2.0, 1.0));
    }

    #[test]
    fn site_and_pooled_rank_decay_to_now() {
        let mut r = SiteRow::new(t0());
        r.alpha = 5.0;
        r.beta = 3.0;
        let (a, b) = decay(5.0, 3.0, 30.0);
        assert!((site_rank(&r, t0() + days(30)) - lcb(a, b)).abs() < E);
        assert!((site_rank(&r, t0()) - lcb(5.0, 3.0)).abs() < E);
        assert!((pooled_rank(&[], t0()) - lcb(1.0, 1.0)).abs() < E);
        let mut r2 = SiteRow::new(t0());
        r2.alpha = 4.0;
        r2.beta = 2.0;
        // 1 + (5-1) + (4-1), 1 + (3-1) + (2-1)
        assert!((pooled_rank(&[r.clone(), r2], t0()) - lcb(8.0, 4.0)).abs() < E);
        // a single row pools to its own rank
        assert!((pooled_rank(&[r.clone()], t0() + days(10)) - site_rank(&r, t0() + days(10))).abs() < E);
    }

    #[test]
    fn approval_numbers_counters_and_candidate_to_active() {
        let r = SiteRow::new(t0());
        let now = t0() + days(1);
        let r = apply(&r, Event::Approval, now);
        assert_eq!((r.alpha, r.beta), (3.0, 1.0));
        assert_eq!((r.approvals, r.uses, r.rejections), (1, 1, 0));
        assert_eq!((r.last_used_at, r.last_ok_at, r.scored_at), (Some(now), Some(now), now));
        assert_eq!(r.state, SiteState::Active);
    }

    #[test]
    fn auto_delivered_counts_a_use_not_an_approval() {
        let r = apply(&SiteRow::new(t0()), Event::AutoDelivered, t0());
        assert_eq!((r.alpha, r.approvals, r.uses), (2.0, 0, 1));
        assert_eq!(r.state, SiteState::Active);
        assert_eq!(r.last_ok_at, Some(t0()));
    }

    #[test]
    fn rejection_suspends_and_scores_five() {
        let r = apply(&SiteRow::new(t0()), Event::Rejection, t0());
        assert_eq!((r.alpha, r.beta, r.rejections, r.uses), (1.0, 6.0, 1, 1));
        assert_eq!(r.state, SiteState::Suspended);
        assert_eq!(r.suspended_reason, Some(SuspendReason::Rejected));
        assert_eq!(r.suspended_at, Some(t0()));
        assert_eq!(r.last_used_at, Some(t0()));
        assert_eq!(r.last_ok_at, None);
        // a second rejection does not move suspended_at
        let r2 = apply(&r, Event::Rejection, t0() + days(3));
        assert_eq!(r2.suspended_at, Some(t0()));
        assert_eq!(r2.rejections, 2);
    }

    #[test]
    fn rejected_suspension_is_not_lifted_by_approval_or_passes() {
        let r = apply(&SiteRow::new(t0()), Event::Rejection, t0());
        let r = apply(&r, Event::Approval, t0() + days(1));
        assert_eq!(r.state, SiteState::Suspended);
        assert_eq!(r.suspended_reason, Some(SuspendReason::Rejected));
        let mut r = r;
        for i in 0..5 {
            r = apply(&r, Event::CanaryPass, t0() + days(2 + i));
        }
        assert_eq!(r.state, SiteState::Suspended);
        assert_eq!(r.suspended_reason, Some(SuspendReason::Rejected));
        assert!(r.pass_streak >= 5);
    }

    #[test]
    fn one_canary_pass_does_not_promote_two_do() {
        let r = apply(&SiteRow::new(t0()), Event::CanaryPass, t0());
        assert_eq!(r.state, SiteState::Candidate);
        assert_eq!((r.alpha, r.pass_streak, r.uses), (1.5, 1, 0));
        assert_eq!(r.last_used_at, None, "a canary is not a use");
        assert_eq!(r.last_ok_at, Some(t0()));
        let r = apply(&r, Event::CanaryPass, t0() + days(1));
        assert_eq!(r.state, SiteState::Active);
        assert_eq!(r.pass_streak, 2);
    }

    #[test]
    fn a_fail_between_two_passes_resets_the_streak() {
        let r = apply(&SiteRow::new(t0()), Event::CanaryPass, t0());
        let r = apply(&r, Event::CanaryFail, t0());
        assert_eq!(r.pass_streak, 0);
        let r = apply(&r, Event::CanaryPass, t0());
        assert_eq!(r.state, SiteState::Candidate, "the streak restarted");
        let r = apply(&r, Event::CanaryPass, t0());
        assert_eq!(r.state, SiteState::Active);
    }

    #[test]
    fn the_third_failure_suspends_the_second_does_not() {
        for ev in [Event::CanaryFail, Event::NoCandidate] {
            let mut r = apply(&SiteRow::new(t0()), Event::Approval, t0());
            assert_eq!(r.state, SiteState::Active);
            r = apply(&r, ev, t0());
            r = apply(&r, ev, t0());
            assert_eq!((r.state, r.consecutive_fail), (SiteState::Active, 2), "{ev:?}");
            let now = t0() + days(2);
            r = apply(&r, ev, now);
            assert_eq!(r.state, SiteState::Suspended, "{ev:?}");
            assert_eq!(r.suspended_reason, Some(SuspendReason::Failing));
            assert_eq!(r.suspended_at, Some(now));
            // more failures do not move suspended_at
            let r2 = apply(&r, ev, now + days(5));
            assert_eq!(r2.suspended_at, Some(now));
            assert_eq!(r2.suspended_reason, Some(SuspendReason::Failing));
        }
        // a candidate suspends on three failures too
        let mut r = SiteRow::new(t0());
        for _ in 0..3 {
            r = apply(&r, Event::CanaryFail, t0());
        }
        assert_eq!(r.state, SiteState::Suspended);
    }

    #[test]
    fn no_candidate_counts_a_use_a_canary_fail_does_not() {
        let r = apply(&SiteRow::new(t0()), Event::NoCandidate, t0());
        assert_eq!((r.uses, r.last_used_at, r.beta), (1, Some(t0()), 2.0));
        let r = apply(&SiteRow::new(t0()), Event::CanaryFail, t0());
        assert_eq!((r.uses, r.last_used_at, r.beta), (0, None, 2.0));
    }

    #[test]
    fn failing_suspension_is_lifted_by_approval_and_by_two_passes() {
        let mut s = SiteRow::new(t0());
        for _ in 0..3 {
            s = apply(&s, Event::CanaryFail, t0());
        }
        assert!(s.is_suspended());
        let a = apply(&s, Event::Approval, t0() + days(1));
        assert_eq!((a.state, a.suspended_reason, a.suspended_at), (SiteState::Active, None, None));
        assert_eq!(a.consecutive_fail, 0);
        let p = apply(&s, Event::CanaryPass, t0() + days(1));
        assert_eq!(p.state, SiteState::Suspended, "one pass is not enough");
        let p = apply(&p, Event::CanaryPass, t0() + days(1));
        assert_eq!((p.state, p.suspended_reason, p.suspended_at), (SiteState::Active, None, None));
    }

    #[test]
    fn apply_decays_before_adding() {
        let mut r = SiteRow::new(t0());
        r.alpha = 5.0;
        r.beta = 3.0;
        let r = apply(&r, Event::Approval, t0() + days(30));
        assert!((r.alpha - (3.0 + 2.0)).abs() < E);
        assert!((r.beta - 2.0).abs() < E);
    }

    #[test]
    fn recipe_state_derivation() {
        use RecipeState as R;
        use SiteState::*;
        assert_eq!(recipe_state(Origin::Shipped, R::Candidate, &[Suspended]), R::Active);
        assert_eq!(recipe_state(Origin::Llm, R::Candidate, &[]), R::Candidate);
        assert_eq!(recipe_state(Origin::Llm, R::Candidate, &[Suspended, Active]), R::Active);
        assert_eq!(recipe_state(Origin::Llm, R::Active, &[Suspended, Candidate]), R::Candidate);
        assert_eq!(recipe_state(Origin::Admin, R::Active, &[Suspended, Suspended]), R::Suspended);
        assert_eq!(recipe_state(Origin::Llm, R::Retired, &[Active]), R::Retired);
        assert_eq!(recipe_state(Origin::Shipped, R::Retired, &[Active]), R::Retired);
    }

    fn key(hash: &str, rank: f64, ok: Option<i64>) -> RankKey {
        RankKey { rank, last_ok_at: ok.map(|d| t0() + days(d)), hash: hash.to_string() }
    }

    #[test]
    fn rank_order_is_total_and_independent_of_input_order() {
        let base = vec![
            key("d", 0.5, Some(3)),
            key("a", 0.5, Some(3)),
            key("b", 0.5, Some(9)),
            key("c", 0.5, None),
            key("e", 0.9, None),
            key("f", 0.1, Some(100)),
            key("g", 0.5, None),
        ];
        let mut want = base.clone();
        rank_order(&mut want, |k| k.clone());
        let order: Vec<&str> = want.iter().map(|k| k.hash.as_str()).collect();
        // rank desc; then last_ok desc, none last; then hash asc
        assert_eq!(order, ["e", "b", "a", "d", "c", "g", "f"]);
        let mut seed = 12345u64;
        for _ in 0..20 {
            let mut v = base.clone();
            for i in (1..v.len()).rev() {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                v.swap(i, (seed >> 33) as usize % (i + 1));
            }
            rank_order(&mut v, |k| k.clone());
            assert_eq!(v, want);
        }
    }

    fn meta(origin: Origin) -> RecipeMeta {
        RecipeMeta { origin, state: RecipeState::Candidate, created_at: t0(), revived_at: None }
    }

    fn suspended_row(reason: SuspendReason, at: DateTime<Utc>, used: Option<DateTime<Utc>>) -> SiteRow {
        let mut r = SiteRow::new(t0());
        r.state = SiteState::Suspended;
        r.suspended_reason = Some(reason);
        r.suspended_at = Some(at);
        r.last_used_at = used;
        r
    }

    #[test]
    fn suspended_recipes_retire_at_thirty_days_not_before() {
        let at = t0() + days(10);
        let rows = [suspended_row(SuspendReason::Failing, at, Some(at))];
        assert_eq!(due_retirement(&meta(Origin::Llm), &rows, at + days(30) - Duration::seconds(1)), None);
        assert_eq!(due_retirement(&meta(Origin::Llm), &rows, at + days(30)), Some(RetiredReason::Failing));
        let rows = [
            suspended_row(SuspendReason::Failing, at, Some(at)),
            suspended_row(SuspendReason::Rejected, at - days(1), Some(at)),
        ];
        assert_eq!(due_retirement(&meta(Origin::Admin), &rows, at + days(30)), Some(RetiredReason::Rejected));
        // one live row keeps the recipe
        let rows = [suspended_row(SuspendReason::Failing, at, Some(at)), SiteRow::new(at)];
        let mut live = rows.clone();
        live[1].last_used_at = Some(at);
        assert_eq!(due_retirement(&meta(Origin::Llm), &live, at + days(40)), None);
        // the newest suspension counts
        let rows = [
            suspended_row(SuspendReason::Failing, at, Some(at)),
            suspended_row(SuspendReason::Failing, at + days(20), Some(at)),
        ];
        assert_eq!(due_retirement(&meta(Origin::Llm), &rows, at + days(40)), None);
    }

    #[test]
    fn a_revival_restarts_the_idle_clock() {
        let mut m = meta(Origin::Llm);
        m.revived_at = Some(t0() + days(101));
        assert_eq!(due_retirement(&m, &[], t0() + days(102)), None);
        assert_eq!(due_retirement(&m, &[], t0() + days(191)), Some(RetiredReason::Idle));
    }

    #[test]
    fn idle_recipes_retire_at_ninety_days_not_before() {
        let m = meta(Origin::Llm);
        assert_eq!(due_retirement(&m, &[], t0() + days(89)), None);
        assert_eq!(due_retirement(&m, &[], t0() + days(90)), Some(RetiredReason::Idle));
        // last use, not creation, is the reference once a row has one
        let mut r = SiteRow::new(t0());
        r.last_used_at = Some(t0() + days(50));
        assert_eq!(due_retirement(&m, &[r.clone()], t0() + days(139)), None);
        assert_eq!(due_retirement(&m, &[r.clone()], t0() + days(140)), Some(RetiredReason::Idle));
        // idle is checked before suspension
        let rows = [suspended_row(SuspendReason::Rejected, t0(), Some(t0()))];
        assert_eq!(due_retirement(&m, &rows, t0() + days(100)), Some(RetiredReason::Idle));
        // shipped and retired are never due
        assert_eq!(due_retirement(&meta(Origin::Shipped), &[], t0() + days(900)), None);
        let mut gone = meta(Origin::Llm);
        gone.state = RecipeState::Retired;
        assert_eq!(due_retirement(&gone, &[], t0() + days(900)), None);
    }

    fn src(id: i64, key: &str, fam: Option<&str>, origin: Origin, active: bool) -> PromoSource {
        PromoSource { id, scope_key: key.to_string(), family: fam.map(str::to_string), origin, own_active: active }
    }

    #[test]
    fn promotion_needs_two_distinct_active_sites_of_one_family() {
        let one = [src(1, "a.example", Some("jwplayer"), Origin::Llm, true)];
        assert_eq!(plan_promotion(&one, "jwplayer"), None);
        let one_inactive = [
            src(1, "a.example", Some("jwplayer"), Origin::Llm, true),
            src(2, "b.example", Some("jwplayer"), Origin::Llm, false),
        ];
        assert_eq!(plan_promotion(&one_inactive, "jwplayer"), None);
        // two recipes of one site are one site
        let same_site = [
            src(1, "a.example", Some("jwplayer"), Origin::Llm, true),
            src(2, "a.example", Some("jwplayer"), Origin::Llm, true),
        ];
        assert_eq!(plan_promotion(&same_site, "jwplayer"), None);
        let two = [
            src(2, "b.example", Some("jwplayer"), Origin::Llm, true),
            src(1, "a.example", Some("jwplayer"), Origin::Llm, true),
            src(3, "c.example", Some("jwplayer"), Origin::Llm, false),
        ];
        let p = plan_promotion(&two, "jwplayer").unwrap();
        assert_eq!(p, Promotion { source_ids: vec![1, 2, 3], origin: Origin::Llm });
        // a different family does not qualify
        assert_eq!(plan_promotion(&two, "videojs"), None);
        let mixed = [
            src(1, "a.example", Some("jwplayer"), Origin::Llm, true),
            src(2, "b.example", Some("videojs"), Origin::Llm, true),
            src(3, "c.example", None, Origin::Llm, true),
        ];
        assert_eq!(plan_promotion(&mixed, "jwplayer"), None);
        // admin if any source is admin
        let adm = [
            src(1, "a.example", Some("jwplayer"), Origin::Llm, true),
            src(2, "b.example", Some("jwplayer"), Origin::Admin, true),
        ];
        assert_eq!(plan_promotion(&adm, "jwplayer").unwrap().origin, Origin::Admin);
    }

    #[test]
    fn merged_rows_add_evidence_and_keep_the_stricter_state() {
        let mut a = SiteRow::new(t0());
        a.alpha = 5.0;
        a.beta = 2.0;
        a.approvals = 2;
        a.uses = 3;
        a.pass_streak = 1;
        a.state = SiteState::Active;
        a.last_used_at = Some(t0() + days(3));
        a.last_ok_at = Some(t0() + days(2));
        let mut b = SiteRow::new(t0() + days(1));
        b.scored_at = t0() + days(30);
        b.alpha = 4.0;
        b.beta = 3.0;
        b.rejections = 1;
        b.uses = 2;
        b.consecutive_fail = 2;
        b.last_used_at = Some(t0() + days(9));
        let now = t0() + days(30);
        let m = merge_rows(&a, &b, now);
        let (aa, ab) = decay(5.0, 2.0, 30.0);
        assert!((m.alpha - (1.0 + (aa - 1.0) + 3.0)).abs() < E);
        assert!((m.beta - (1.0 + (ab - 1.0) + 2.0)).abs() < E);
        assert_eq!((m.approvals, m.rejections, m.uses), (2, 1, 5));
        assert_eq!((m.pass_streak, m.consecutive_fail), (1, 2));
        assert_eq!(m.created_at, t0());
        assert_eq!(m.scored_at, now);
        assert_eq!(m.last_used_at, Some(t0() + days(9)));
        assert_eq!(m.last_ok_at, Some(t0() + days(2)));
        assert_eq!(m.state, SiteState::Candidate, "candidate is stricter than active");

        // rejected beats failing beats candidate beats active, whichever side
        let failing = suspended_row(SuspendReason::Failing, t0() + days(4), None);
        let rejected = suspended_row(SuspendReason::Rejected, t0() + days(7), None);
        for (x, y) in [(&a, &failing), (&failing, &a)] {
            let m = merge_rows(x, y, now);
            assert_eq!((m.state, m.suspended_reason, m.suspended_at), (SiteState::Suspended, Some(SuspendReason::Failing), Some(t0() + days(4))));
        }
        for (x, y) in [(&failing, &rejected), (&rejected, &failing)] {
            let m = merge_rows(x, y, now);
            assert_eq!((m.state, m.suspended_reason, m.suspended_at), (SiteState::Suspended, Some(SuspendReason::Rejected), Some(t0() + days(7))));
        }
    }
}
