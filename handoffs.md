# Handoffs

Running log of milestones against `plan.md`. Each entry records what landed,
what was verified (and how), and what the next agent should know before
touching the code.

---

## Milestone 1 — Phase 0 complete (safety net)

**Date:** 2026-09-19
**Commits:** `75321e9`, `9940d92`, `6f1f844`, `48d75e4` (on top of baseline `5004d19`)
**State:** `cargo build --release` 0 warnings, `cargo test --workspace` 21/21 suites green.

### What landed

| Item | Defect | Summary |
|---|---|---|
| P0.1 | W-10 | `omni_core::paths::AppPaths` anchors every relative path to the install directory |
| P0.2 | — | `omni_core::migrations`: ordered, append-only migrations with pre-migration backup |
| P0.3 | D-11 | Real stored timestamps; also fixed a session-expiry comparison hole |
| P0.4 | T-01 | Tautological compliance tests replaced with contract tests over pure arg builders |
| P0.5 | W-05 | Setup wizard no longer double-hashes the admin password |
| P0.6 | D-03 | `omni_core::process`: timeouts, cancellation, Windows Job Object process-tree kill |

### Things found while implementing that were not in the defect register

1. **Session expiry could be bypassed for most of a day.** `create_session`
   stored `expires_at` via `to_rfc3339()` (`2026-09-19T16:00:00+00:00`) and the
   lookup compared it against `CURRENT_TIMESTAMP` (`2026-09-19 17:00:00`).
   SQLite compares those as text, and `'T'` (0x54) sorts above `' '` (0x20), so
   any session expiring *earlier the same day* still authenticated. Both sides
   now use one format. The regression test deliberately uses a one-minute-old
   expiry — a whole-day expiry was rejected even by the old code, so a `-1 day`
   test would not have caught it.

2. **`CREATE_BREAKAWAY_FROM_JOB` breaks process spawning.** When the daemon is
   itself inside a job object that lacks `JOB_OBJECT_LIMIT_BREAKAWAY_OK` (CI
   agents, some terminals, some SCM configurations) the flag makes
   `CreateProcess` fail outright with `ERROR_ACCESS_DENIED`. Windows 8+ nests
   job objects, so it is not needed. Do not "helpfully" add it back.

3. **A child's pipes are inherited by its children.** `read_line` on a child's
   stdout only reaches EOF once the *whole tree* exits. With the job object that
   is immediate, but if assignment ever fails the drain would block the stage
   for the orphan's full lifetime — the exact hang the timeout exists to
   prevent. `process::run` therefore bounds the drain (2 s after a kill, 10 s
   otherwise) and gives up on the stderr tail rather than the pipeline.

### Verification performed

- **P0.1:** ran `omni-ingest.exe adblock status` with CWD `C:\Windows\System32`.
  Reported install root `D:\OmniDownloader`, loaded 40 420 blocked domains from
  `D:\OmniDownloader\data\adblock`, created no `System32\data`.
- **P0.6:** `process_tree_kill_tests` spawns `cmd` → detached 10-minute `ping`
  and asserts no marked ping survives the timeout and that `run()` returns
  within 20 s. **Negative control run:** with `kill_tree = false` the run never
  completes at all (the orphan holds the harness pipes), so the test is not
  tautological.
- **P0.4:** verified by mutation. Changing `-g` to `15` fails
  `video_matches_sony_xdcam_hd422_pal_1080i50`; emitting 7 audio streams fails
  `audio_is_always_exactly_eight_discrete_mono_streams`.
- **P0.3:** `stored_timestamps_are_real_and_advance_on_update` sleeps 1.1 s
  between insert and update and asserts both that `updated_at > created_at` and
  that `created_at` is *not* approximately now.

### Deliberately not done yet

- **D-08 (`tinterlace` halves the frame rate) is still present.** It is now
  isolated in `transcoder::VIDEO_FILTER_CHAIN` with a doc comment naming the
  defect, so the P1.5 fix is a visible diff against a tested baseline. Do not
  ship a claim that the transcoder is correct until P1.5 lands *and* the
  in-house suite grows 25p/30p/25i sources (T-03) — the current synthetic
  source is 50p, which is the one rate that hides the bug.
- **D-07 (ffprobe failure read as "no audio") is still present** in
  `Transcoder::get_media_info`, which returns `(0.0, 0)` on failure. The pure
  builder already distinguishes "no audio stream" from "unknown" via
  `TranscodeInput::audio_channels: Option<u32>`; P1.5 must stop `transcode()`
  from collapsing a failed probe into `Some(0)`.
- The default `admin@newsroom.local / admin123` seed still exists (P2.4).

### Notes for the next agent

- `config.json` is **gitignored** — it carries mailbox credentials.
  `config.example.json` ships the shape, without secrets and without the
  one-line `system_prompt` that is defect E-03.
- `bin/` (214 MB of ffmpeg/ffprobe/bmxtranswrap/yt-dlp) is gitignored. A fresh
  clone needs those four binaries placed there before the pipeline will run.
- `Job.created_at` / `updated_at` and friends are now `Option<DateTime<Utc>>`.
  The UI must render `None` as a blank, never as "now".
- Use `omni_core::process::run` for every new external tool call. Do not add a
  bare `Command::spawn`.
