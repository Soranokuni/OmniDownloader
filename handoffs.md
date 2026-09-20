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

---

# Forward handoff — entry points for Phases 2–9

Written 2026-09-20, after Phase 1. Everything above this line records what
happened; everything below is for whoever picks the work up next.

## Recommended order, and why it differs from the plan

`plan.md` section 16 schedules P3 (extraction) before P4 and P7. **Do Phase 2
next regardless of what else is scheduled**, because of where the system
currently stands:

The pipeline is now considerably more trustworthy than the thing in front of it.
`POST /api/setup` is unauthenticated, so anyone on the newsroom LAN can repoint
`watchfolder_path`. The hardening in Phases 0–1 guarantees a correct MXF is
produced and delivered atomically to *whatever directory an attacker last
named*. Phase 1 made that failure mode more reliable, not less.

Suggested sequence: **P2 → P6.1/P6.2 (logging + real health) → P4 → P3 → P7 →
P5 → P8 → P9.** Logging and honest health checks make every later phase
debuggable in situ; P4 is what the newsroom touches daily and its deterministic
parser is testable offline; P3 is the most open-ended and benefits from having
the benchmark harness P6.4 provides.

## Carried-forward backlog

Items the plan assigns to Phase 1 that are **not** done. None block Phase 2.

| Item | State | Where to start |
|---|---|---|
| P1.4 disk guard | `ErrorCode::LowDisk` exists with a 10-min backoff; nothing raises it | before DOWNLOAD in `pipeline.rs`; needs a free-space call — `GetDiskFreeSpaceExW` via the `windows` crate already in `omni-core` |
| P1.7 source archive | not started | `Delivered` already reports the final filename; archive after the DELIVER stage |
| P1.7 per-journalist layout | delivery is still flat | `WatchfolderDelivery::deliver` takes the destination dir — pass `watchfolder/{JOURNALIST}` and the collision logic works unchanged |
| P1.10 stage timings / SSE | stages are set and timed; `stage_timings_json` is never written | column exists in migration 2 |
| D-21 remainder | `JobStatus::parse` returns `Option`; `media_format` is still free text | `models.rs` |

## Phase 2 — security (do this next)

Verified state as of this commit:

- **W-01 / W-02.** `crates/omni-web/src/server.rs` builds one flat router with
  no auth layer. `/api/setup`, `/api/jobs/:id/{override,retry,discard}`,
  `/api/journalists*`, `/api/admin/*`, `/api/system/logs` and
  `/api/system/test-email` are all reachable unauthenticated. The last is a
  credential oracle for the mailbox.
- **W-03.** 12 `innerHTML` sites across `crates/omni-web/assets/*.html`. The
  injected values (`url`, `slug`, `notes`, `error_message`) originate in emails,
  so this is stored XSS reachable by anyone who can mail the ingest address.
- **W-04.** Still seeded at `repository.rs:68-74` —
  `admin@newsroom.local` / `admin123`. Removing it must land together with
  P2.4's first-run path, or a fresh install has no way in.
- **W-06.** `AppConfig::email_password` is still a plaintext `String`
  (`config.rs:164`), written to `config.json` by `save_to_file`.
- **W-08.** 15 external asset references across the panels.
- **`AuthMode`** (`config.rs:108-119`) is to be *deleted*, not extended — P2.1
  replaces it with `security.mcr_open_networks`.

Build on rather than redo:

- `Repository::create_session_for(user_id, token, chrono::Duration)` already
  takes an arbitrary lifetime, so P2.2's per-role session lengths need no new
  plumbing.
- Session expiry now stores and compares one timestamp format. **Do not
  reintroduce `CURRENT_TIMESTAMP` in a session query** — see Milestone 1 for why
  that let expired sessions authenticate for most of a day.
- Argon2id hashing is centralised in `Repository::create_user`; it is the only
  place that hashes, and it takes plaintext.

**The next migration number is 3.** `MIGRATIONS` in `migrations.rs` is
append-only — never edit or renumber an applied entry. P2.2 needs
`sessions.created_at / last_seen_at / ip / user_agent`.

## Phase 3 — extraction

- `UnifiedAdBlocker::init(dir)` is now **mandatory** before any sniff, and
  `global()` panics otherwise. The browser-pool work must not move
  initialisation later than `main`'s current call site.
- `ErrorCode::should_try_sniffer()` already encodes which failures are worth
  opening a browser for (`UNSUPPORTED_URL`, `HTTP_403`, `NO_STREAM_FOUND`) and
  which are not — a deleted or geo-blocked video should never cost a browser
  launch. P3.1's router should use it rather than re-deriving the rule.
- `crates/omni-browser/src/agent.rs` (`ComputerUseAgentPlaceholder`) is called
  from `src/main.rs:562` for locker detection. P8 deletes the module, so
  whoever does that must replace this call site with the `LockerResolver`
  trait — it is the only remaining user.
- `omni_core::urlnorm::registrable_domain()` is what `domain_stats` and the
  cookie jars should key on; it already folds `youtu.be` → `youtube.com` and
  `twitter.com` → `x.com`.

## Phase 4 — email

- `watcher.rs:157` and `:212` still call `add_job`. Move them to
  `Repository::enqueue(&NewJob, dedup_window_hours)`, which returns
  `Enqueued::{Created, DuplicateActive, DuplicateRecent}` — P4.5 and the P5
  reply templates need that distinction, and `add_job` flattens it to an id.
- `NewJob` already carries `email_message_id` and `extraction_method`, and
  migration 2 added `journalists.aliases` (JSON array) for P4.3's Greek name
  resolution. `processed_mail` does **not** exist yet.
- E-03 is still live: the deployed `config.json` overrides
  `DEFAULT_SYSTEM_PROMPT` (`config.rs:7`) with a one-liner. P4.4 moves the
  prompt to `assets/prompts/` and leaves only `llm.prompt_id` in config.
- Deterministic-first is a decided constraint, not a preference: the parser must
  produce the same job set with the LLM unreachable.

## Phase 6 — observability and benchmarks

**Read this before writing the P6.3 interlacing check.** `idet` reports
**Progressive** for a 25p source's output, and that is *correct*: 25p → 25i
produces PsF, where both fields come from one source frame, so there is no
inter-field motion to detect. The container correctly says `field_order=tt`.
An "idet must report TFF" assertion would false-alarm on the single most common
web source rate. Measured: a 50p source does produce genuine interlacing
(TFF 151, BFF 0, Progressive 0).

The check that actually catches the D-08 class of defect is **unique frame count
under `mpdecimate`** — not frame count, and not `idet`. See
`crates/omni-broadcast/tests/end_to_end_pipeline_tests.rs`, which already does
this and can be lifted into the PowerShell suite.

`scripts/in_house_test.ps1` still generates 50p only (T-03) — the one rate that
hid D-08. It needs 25p/30p/25i-tff/mono/5.1/no-audio sources. The Rust
end-to-end test already covers 25p/30p/50p, so the two should not duplicate
effort.

## Conventions this codebase now assumes

Breaking any of these will pass review by accident and fail in the newsroom:

1. **`Repository::enqueue` is the only correct way to create a job.** A direct
   INSERT produces a row invisible to deduplication.
2. **Every external tool goes through `omni_core::process::run`.** No bare
   `Command::spawn` remains in the pipeline; anything else has no timeout and
   leaks its process tree.
3. **The pipeline never sets a terminal status** — the worker holding the lease
   does. `set_stage`, `heartbeat`, `finish` and `requeue_after` all verify
   ownership, so a worker whose lease was reaped mid-stage cannot write to a job
   another worker now owns.
4. **Failures are wrapped with `.context(code.as_str())`** so the worker recovers
   the `ErrorCode` from the anyhow chain instead of re-parsing a message.
5. **Never a filename-prefix match for cleanup.** Job 1's prefix also matches
   jobs 10–19 and 100–199. Per-job directories only.
6. **Never delete or overwrite a file in the watchfolder.** Collisions get a
   `_N` suffix.
7. **Timestamps are `Option`.** Render `None` as blank, never as "now".
8. **`MIGRATIONS` is append-only.**

## Open questions for the product owner

None blocking today, but each is needed before the phase that depends on it:

1. **P1.7 per-journalist subfolders** require MCR to reconfigure Dalet to watch
   subdirectories. If Dalet cannot, `delivery.layout=flat` stays — worth
   confirming before building the layout.
2. **P2.1's `mcr_open_networks`** needs the actual newsroom subnet in CIDR form.
3. **P3.5's cookie jar** assumes the station has an X account whose cookies can
   be exported.
4. **Loudness target** is −23 LUFS / −1 dBTP. If playout already normalises,
   `audio.loudnorm_enabled` should ship `false` — double normalisation is worse
   than none.
