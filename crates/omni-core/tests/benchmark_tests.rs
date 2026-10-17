//! The benchmark's tables (plan P6.4): the corpus is stored once, one run at a
//! time, a report compares only runs of the same mode, and nothing stored or
//! written names a full address. Invented hosts only.

use anyhow::Result;
use chrono::{Duration, Utc};
use tempfile::TempDir;

use omni_core::benchmark::{self, Expected, Mode, ResultRow, CORPUS};
use omni_core::repository::{BenchStart, Repository};

fn repo(dir: &TempDir) -> Repository {
    Repository::new(dir.path().join("omni.db")).unwrap()
}

fn sync_ok(repo: &Repository, run: i64, idx: i64, ms: i64) -> Result<()> {
    let target = repo.benchmark_targets()?.into_iter().find(|t| t.idx == idx).unwrap();
    repo.record_benchmark_result(&ResultRow {
        run_id: run,
        target_id: target.id,
        extract_ok: Some(true),
        extract_ms: Some(ms),
        method: Some("yt-dlp".into()),
        candidates: Some(1),
        stream_host: Some("cdn.example.net".into()),
        total_ms: Some(ms),
        ..Default::default()
    })
}

#[test]
fn seeding_twice_leaves_65_rows_and_the_hub_page_expected_to_fail() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    assert!(r.benchmark_targets()?.is_empty(), "nothing is stored before the first seed");
    r.seed_benchmark_targets()?;
    r.seed_benchmark_targets()?;
    let rows = r.benchmark_targets()?;
    assert_eq!(rows.len(), 65);
    assert_eq!(rows.len(), CORPUS.len());
    let fails: Vec<_> = rows.iter().filter(|t| t.expected == Expected::ExpectedFail).collect();
    assert_eq!(fails.len(), 1);
    assert_eq!(fails[0].idx, 61);
    assert!(fails[0].note.contains("hub page"));
    // The ids do not move when the corpus is seeded again: results point at them.
    let again = r.benchmark_targets()?;
    assert_eq!(rows, again);
    Ok(())
}

#[test]
fn only_one_run_at_a_time_and_a_run_older_than_six_hours_is_abandoned() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let six_h = Duration::hours(6);
    let t0 = Utc::now();
    let BenchStart::Started(first) = r.begin_benchmark_run(Mode::Extract, "{}", t0, six_h, "cli")? else { panic!("first run must start") };

    // A second request while it runs is refused, naming the run that holds the place.
    let soon = t0 + Duration::minutes(5);
    assert_eq!(r.begin_benchmark_run(Mode::Full, "{}", soon, six_h, "cli")?, BenchStart::Busy(first));
    // Just under six hours it still counts as running.
    let almost = t0 + Duration::hours(6) - Duration::seconds(1);
    assert_eq!(r.begin_benchmark_run(Mode::Extract, "{}", almost, six_h, "cli")?, BenchStart::Busy(first));

    // Past six hours the old run is marked abandoned and the new one starts.
    let late = t0 + Duration::hours(6) + Duration::seconds(1);
    let BenchStart::Started(second) = r.begin_benchmark_run(Mode::Extract, "{}", late, six_h, "cli")? else {
        panic!("a stale run must not block a new one")
    };
    assert_ne!(first, second);
    assert_eq!(r.benchmark_run(first)?.unwrap().state, "abandoned");
    assert!(r.benchmark_run(first)?.unwrap().finished_at.is_some());
    assert_eq!(r.benchmark_run(second)?.unwrap().state, "running");

    // Finishing frees the place; an abandoned run cannot be finished afterwards.
    assert!(!r.finish_benchmark_run(first, "done", None, late)?);
    assert!(r.finish_benchmark_run(second, "done", None, late)?);
    assert!(matches!(r.begin_benchmark_run(Mode::Extract, "{}", late, six_h, "cli")?, BenchStart::Started(_)));
    Ok(())
}

#[test]
fn the_previous_run_is_the_latest_finished_one_of_the_same_mode() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    r.seed_benchmark_targets()?;
    let now = Utc::now();
    let mut ids = Vec::new();
    for (mode, state) in [(Mode::Extract, "done"), (Mode::Full, "done"), (Mode::Extract, "failed"), (Mode::Extract, "done"), (Mode::Full, "done")] {
        let BenchStart::Started(id) = r.begin_benchmark_run(mode, "{}", now, Duration::hours(6), "cli")? else { panic!() };
        sync_ok(&r, id, 1, 1000)?;
        let summary = benchmark::build_report(&r, id)?.map(|rep| serde_json::to_string(&rep.summary).unwrap());
        r.finish_benchmark_run(id, state, summary.as_deref(), now)?;
        ids.push(id);
    }
    // Run 4 (extract, done): the previous is run 1, not the full run 2 and not the failed run 3.
    assert_eq!(r.previous_benchmark_run(Mode::Extract, ids[3])?.unwrap().id, ids[0]);
    // Run 5 (full): the previous is run 2.
    assert_eq!(r.previous_benchmark_run(Mode::Full, ids[4])?.unwrap().id, ids[1]);
    // The first of a mode has none.
    assert!(r.previous_benchmark_run(Mode::Full, ids[1])?.is_none());

    // Through the report: run 4 is compared with run 1 only.
    let rep = benchmark::build_report(&r, ids[3])?.unwrap();
    assert_eq!(rep.deltas.as_ref().map(|d| d.previous_run_id), Some(ids[0]));
    assert_eq!(benchmark::build_report(&r, ids[1])?.unwrap().deltas.map(|d| d.previous_run_id), None);
    Ok(())
}

#[test]
fn no_full_address_is_in_a_run_a_result_or_a_report_file() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    r.seed_benchmark_targets()?;
    let now = Utc::now();
    let BenchStart::Started(id) = r.begin_benchmark_run(Mode::Extract, r#"{"targets":[1,2],"parallel":2}"#, now, Duration::hours(6), "cli")? else {
        panic!()
    };
    // What the runner stores for a sniffed stream: the host, taken from an address with a token.
    let host = benchmark::stream_host("https://cdn.example.net/live/master.m3u8?token=abc123").unwrap();
    let target = r.benchmark_targets()?.into_iter().find(|t| t.idx == 2).unwrap();
    r.record_benchmark_result(&ResultRow {
        run_id: id,
        target_id: target.id,
        extract_ok: Some(true),
        extract_ms: Some(900),
        method: Some("sniffer".into()),
        stream_host: Some(host),
        ..Default::default()
    })?;
    r.finish_benchmark_run(id, "done", None, now)?;

    let report = benchmark::build_report(&r, id)?.unwrap();
    let json = serde_json::to_string_pretty(&report)?;
    let md = benchmark::markdown(&report);
    for (what, text) in [("json", json.as_str()), ("markdown", md.as_str())] {
        assert!(!text.contains("http://") && !text.contains("https://"), "{what} report holds an address");
        assert!(!text.contains("token=abc123") && !text.contains("master.m3u8"), "{what} report holds a stream path");
    }
    assert!(md.contains("cdn.example.net"), "the host is shown");

    // The tables hold addresses only in `benchmark_targets` (the corpus itself).
    let conn = rusqlite::Connection::open(dir.path().join("omni.db"))?;
    for (table, columns) in [
        ("benchmark_runs", "params_json || ifnull(summary_json,'') || mode || state"),
        ("benchmark_results", "ifnull(method,'') || ifnull(error_code,'') || ifnull(stream_host,'')"),
    ] {
        let n: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE ({columns}) LIKE '%http%' OR ({columns}) LIKE '%://%'"),
            [],
            |row| row.get(0),
        )?;
        assert_eq!(n, 0, "{table} holds an address");
    }
    Ok(())
}

#[test]
fn a_run_is_stale_by_its_heartbeat_not_by_its_age() -> Result<()> {
    let dir = TempDir::new()?;
    let r = repo(&dir);
    let stale = Duration::minutes(60);
    let t0 = Utc::now();

    // Seven hours old, but alive a minute ago: it still holds the place.
    let BenchStart::Started(old) = r.begin_benchmark_run(Mode::Full, "{}", t0, stale, "cli")? else { panic!() };
    r.benchmark_heartbeat(old, true, t0 + Duration::hours(7) - Duration::minutes(1))?;
    let now = t0 + Duration::hours(7);
    assert_eq!(r.begin_benchmark_run(Mode::Extract, "{}", now, stale, "cli")?, BenchStart::Busy(old));
    let row = r.benchmark_run(old)?.unwrap();
    assert_eq!((row.state.as_str(), row.paused), ("running", true));

    // Silent for 61 minutes: abandoned, and a new run starts.
    let later = now + Duration::minutes(59);
    assert_eq!(r.begin_benchmark_run(Mode::Extract, "{}", later, stale, "cli")?, BenchStart::Busy(old), "a heartbeat exactly 60 minutes old is still alive");
    let BenchStart::Started(next) = r.begin_benchmark_run(Mode::Extract, "{}", later + Duration::minutes(1), stale, "cli")? else {
        panic!("61 minutes of silence must free the place")
    };
    assert_ne!(old, next);
    assert_eq!(r.benchmark_run(old)?.unwrap().state, "abandoned");
    Ok(())
}
