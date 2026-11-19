//! `omni-ingest report autonomy [--days N]` (plan P11.0): the autonomy report as
//! plain text, the figures the admin's Autonomy card shows. Reads the database the
//! daemon writes; works on an installed root while the daemon runs.
//!
//! Prints outcome counts, error codes, actions and sites only: the report holds no
//! address, slug, journalist or file name.

use anyhow::{Context, Result};
use clap::Subcommand;
use omni_core::config::AppConfig;
use omni_core::models::{AutonomyReport, OutcomeTotals};
use omni_core::paths::AppPaths;
use omni_core::repository::Repository;

/// Longest period asked for; the outcome rows are kept 400 days.
const MAX_DAYS: i64 = 365;

#[derive(Subcommand)]
pub enum ReportKind {
    /// How often a video reached air with nobody touching it, and why a
    /// person was needed when one was
    Autonomy {
        /// The last N days (1 to 365)
        #[arg(long, default_value_t = 14)]
        days: i64,
    },
}

pub fn run(paths: &AppPaths, kind: ReportKind) -> Result<()> {
    match kind {
        ReportKind::Autonomy { days } => {
            let days = days.clamp(1, MAX_DAYS);
            let config = AppConfig::load_from_file(&paths.config)
                .with_context(|| format!("Failed loading configuration from {:?}", paths.config))?;
            let repo = Repository::new(paths.resolve(&config.database_path))?;
            let report = repo.autonomy_report(chrono::Utc::now() - chrono::Duration::days(days))?;
            print!("{}", format_autonomy(&report, days));
        }
    }
    Ok(())
}

/// A share as the card shows it, one decimal; `—` when nothing could be measured.
fn rate(r: Option<f64>) -> String {
    r.map(|r| format!("{:.1} %", r * 100.0)).unwrap_or_else(|| "—".to_string())
}

fn split(label: &str, t: &OutcomeTotals) -> String {
    format!(
        "{label}: {} of {} untouched ({} finished)\n",
        t.delivered_untouched,
        t.delivered + t.delivered_manual,
        t.ended
    )
}

/// The report as text: the gate figure, the strict figure, the totals and the three tables.
pub fn format_autonomy(r: &AutonomyReport, days: i64) -> String {
    let t = &r.totals;
    let mut out = format!("Autonomy report, last {days} day(s)\n\n");
    if t.ended == 0 {
        out.push_str("No finished jobs in this period yet.\n");
        out.push_str(&format!("Waiting for a person now: {}\n", r.waiting_now));
        return out;
    }
    let aired = t.delivered + t.delivered_manual;
    out.push_str(&format!(
        "Delivered untouched: {} ({} of {aired} videos that reached air)\n",
        rate(r.untouched_rate),
        t.delivered_untouched
    ));
    out.push_str(&format!("Strict:              {} ({} of {} finished jobs)\n", rate(r.strict_rate), t.delivered_untouched, t.ended));
    out.push_str(&format!(
        "Finished: {} = delivered {} ({} untouched), into Dalet by hand {}, cancelled {}, discarded {}\n",
        t.ended, t.delivered, t.delivered_untouched, t.delivered_manual, t.cancelled, t.discarded
    ));
    out.push_str(&split("From mail", &r.from_mail));
    out.push_str(&split("Added on the desk or panel", &r.manual));
    out.push_str(&format!("Waiting for a person now: {}\n", r.waiting_now));

    let table = |out: &mut String, title: &str, rows: Vec<(String, String)>| {
        out.push_str(&format!("\n{title}\n"));
        if rows.is_empty() {
            out.push_str("  none\n");
        }
        let width = rows.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
        for (k, v) in rows {
            out.push_str(&format!("  {k:<width$}  {v:>9}\n"));
        }
    };
    table(
        &mut out,
        "Why a person was needed (first error code, jobs)",
        r.by_first_error.iter().map(|c| (c.key.clone(), c.count.to_string())).collect(),
    );
    table(
        &mut out,
        "What the operator did (first action, jobs)",
        r.by_touch_reason.iter().map(|c| (c.key.clone(), c.count.to_string())).collect(),
    );
    table(
        &mut out,
        "Sites (untouched / total)",
        r.by_site.iter().map(|s| (s.site.clone(), format!("{} / {}", s.delivered_untouched, s.total))).collect(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_core::models::{AutonomyCount, AutonomySite};

    fn totals(delivered: i64, untouched: i64, manual: i64, cancelled: i64, discarded: i64) -> OutcomeTotals {
        OutcomeTotals {
            ended: delivered + manual + cancelled + discarded,
            delivered,
            delivered_untouched: untouched,
            delivered_manual: manual,
            cancelled,
            discarded,
        }
    }

    fn fixed() -> AutonomyReport {
        let all = totals(9, 8, 1, 1, 2);
        AutonomyReport {
            untouched_rate: all.untouched_rate(),
            strict_rate: all.strict_rate(),
            totals: all,
            from_mail: totals(6, 6, 0, 0, 1),
            manual: totals(3, 2, 1, 1, 1),
            by_first_error: vec![
                AutonomyCount { key: "VIDEO_UNRESOLVED".into(), count: 3 },
                AutonomyCount { key: "NETWORK".into(), count: 1 },
            ],
            by_touch_reason: vec![AutonomyCount { key: "discard".into(), count: 2 }],
            by_site: vec![AutonomySite { site: "news.example".into(), needed_person: 2, delivered_untouched: 5, total: 7 }],
            waiting_now: 4,
        }
    }

    #[test]
    fn the_text_has_the_same_totals_and_rates_as_the_report() {
        let r = fixed();
        let text = format_autonomy(&r, 14);
        let untouched = format!("{:.1} %", r.untouched_rate.unwrap() * 100.0);
        let strict = format!("{:.1} %", r.strict_rate.unwrap() * 100.0);
        assert_eq!((untouched.as_str(), strict.as_str()), ("80.0 %", "61.5 %"));
        assert!(text.contains(&format!("Delivered untouched: {untouched} (8 of 10 videos that reached air)")), "{text}");
        assert!(text.contains(&format!("Strict:              {strict} (8 of 13 finished jobs)")), "{text}");
        assert!(
            text.contains("Finished: 13 = delivered 9 (8 untouched), into Dalet by hand 1, cancelled 1, discarded 2"),
            "{text}"
        );
        assert!(text.contains("From mail: 6 of 6 untouched (7 finished)"), "{text}");
        assert!(text.contains("Added on the desk or panel: 2 of 4 untouched (6 finished)"), "{text}");
        assert!(text.contains("Waiting for a person now: 4"), "{text}");
        assert!(text.contains("VIDEO_UNRESOLVED") && text.contains("discard") && text.contains("5 / 7"), "{text}");
    }

    #[test]
    fn an_empty_period_says_so_and_shows_no_rate() {
        let text = format_autonomy(&AutonomyReport::default(), 7);
        assert!(text.contains("No finished jobs in this period yet."), "{text}");
        assert!(!text.contains('%'), "no made-up rate: {text}");
    }

    #[test]
    fn a_period_with_no_video_on_air_has_a_dash_not_a_division_by_zero() {
        let all = totals(0, 0, 0, 2, 0);
        let r = AutonomyReport { untouched_rate: all.untouched_rate(), strict_rate: all.strict_rate(), totals: all, ..Default::default() };
        let text = format_autonomy(&r, 14);
        assert!(text.contains("Delivered untouched: — (0 of 0 videos that reached air)"), "{text}");
        assert!(text.contains("Strict:              0.0 % (0 of 2 finished jobs)"), "{text}");
    }
}
