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

---

## Milestone 2 — Phase 1 core complete (queue and pipeline reliability)

**Date:** 2026-09-19
**Commits:** `8577cd0`, `37eecff`, `9ae53ca`, `fbe4bf0`, `7fbbc59`, `19e5701`
**State:** `cargo build --release` 0 warnings, **141 tests** green.

### What landed

| Item | Defects | Summary |
|---|---|---|
| P1.1 | D-01, D-02, D-10, D-20, D-21 | Job state machine: leases, heartbeats, reaper, crash recovery, idempotent enqueue |
| P1.2 | — | `omni_core::urlnorm` — one dedup key per asset |
| P1.3 | D-03 | bmxtranswrap under the process runner; retry policy wired into the worker |
| P1.4 | D-04 | Per-job `temp/jobs/{id}/` workspaces + start-up sweep |
| P1.5 | D-07, D-08, D-09 | Transcoder decision matrix, probe-before-transcode, EBU R128 |
| P1.6 | D-06 | Pre-delivery compliance gate |
| P1.7 | D-05 | Non-destructive, durable watchfolder delivery; `--clip` material package name |
| P1.8 | D-17, D-18 | yt-dlp hardening |
| P1.9 | — | Error codes, retry policy, Greek MCR hints |

### Verification that went beyond "the tests pass"

**D-08 was proven with real ffmpeg, not just argument assertions.** A 250-frame
25p source through the old chain and the new one:

| | frames | unique frames (mpdecimate) |
|---|---|---|
| old chain | 250 | **125** |
| new chain | 250 | **250** |

Both files report 25 fps and `field_order=tt`, because `-r 25` duplicates the
halved output back up. **A frame-count check cannot catch this defect** — which
is exactly why it survived. The symptom on air was judder on every pan, not a
wrong-looking file.

**A note for whoever implements P6.3:** `idet` reports **Progressive** for a 25p
source's output, and that is *correct* — PsF carries both fields from one source
frame. A naïve "idet must say TFF" assertion would false-alarm on the most
common web source rate. A 50p source does produce genuine interlacing
(measured: TFF 151, BFF 0, Progressive 0).

**The compliance gate is validated against real files**, not only hand-authored
JSON: 25p/30p/50p sources go through the actual transcoder and must pass, and a
deliberately wrong file (720p, 4:2:0, one stereo stream) built with ffmpeg must
fail on `width`, `height`, `pixel_format` and `audio_stream_count` by name.

**D-01's stress test has a negative control.** Inverting permit-then-lease to
lease-then-permit reproduces the defect exactly: *"8 jobs were RUNNING at once
with only 2 worker permits"*.

**`--clip` was verified against the real bmxtranswrap 1.6.0** before the gate
started checking it — ffprobe reads it back as
`format.tags.material_package_name`.

### Things found while implementing that were not in the defect register

1. **`sync_all` on a read-only handle fails on Windows** with
   `ERROR_ACCESS_DENIED`; `FlushFileBuffers` needs write access. Delivery
   therefore copies through a handle it owns rather than using
   `tokio::fs::copy` and re-opening. Found by the test, not by reasoning.
2. **yt-dlp's geo-block wording varies by extractor.** The pattern assumed
   *"is not available in your country"*; yt-dlp also emits *"The uploader has
   not made this video available in your country"*, which would have been filed
   as an unclassified failure. Caught by testing against real message text.
3. **Delivery's same-volume rename shortcut was a liability**, not an
   optimisation: it meant the copy path — the one that has to be correct on the
   Dalet SMB share — only ran when the watchfolder happened to be remote. It is
   now the only path.

### Deliberately not done yet (Phase 1 remainder)

- **P1.4's disk guard.** The per-job workspace and sweep are in; the
  `free_space > max(20 GB, 3 × estimate)` pre-download check is not. `LOW_DISK`
  exists as an error code with a 10-minute backoff, but nothing raises it yet.
- **P1.7's source archive** (`archive/{YYYY}/{MM}/{DD}/`) and the per-journalist
  watchfolder layout. Delivery is still flat; `Delivered` already reports the
  final filename, so the layout change is contained.
- **P1.10's full stage machine.** Stages are set and timed and the job timeline
  records them, but `stage_timings_json` is not yet populated and SSE still
  emits bare strings rather than structured events.

### Still open from earlier phases

The Phase 2 security defects are untouched: unauthenticated `/api/setup` and
job/journalist/log routes (W-01, W-02), `innerHTML` rendering of email-derived
strings (W-03), the seeded `admin@newsroom.local / admin123` (W-04), the
plaintext mail password (W-06), and the CDN assets (W-08). **The panels are the
weakest part of the system right now** — the pipeline is in much better shape
than the thing in front of it.

### Notes for the next agent

- `Repository::enqueue` is the only correct way to create a job. `add_job` still
  exists and routes through it; a direct INSERT would create a job invisible to
  deduplication.
- Every new external tool call goes through `omni_core::process::run`. There are
  no bare `Command::spawn` calls left in the pipeline; keep it that way.
- The pipeline never sets a terminal status. The worker holding the lease does,
  so the two cannot race into an inconsistent row. `set_stage`, `heartbeat`,
  `finish` and `requeue_after` all verify ownership.
- Failures are wrapped with `.context(code.as_str())` so the worker can recover
  the `ErrorCode` from the anyhow chain rather than re-parsing a message.
