# OmniDownloader Hardening & Optimization Plan

**Audience:** the implementing agent (and the human reviewer).
**Scope:** reliability, performance, security, extraction accuracy, email/LLM integration, the three web panels, notifications, benchmarks, operations.
**Status of decisions (confirmed by the product owner on 2026-09-19):**

| Decision | Choice |
|---|---|
| Mailbox transport | Office 365 via OAuth2 (Microsoft Graph, client credentials) |
| LLM role | Deterministic-first parser, local LLM only as assist, strict validation, works with LLM offline |
| EBU R128 loudness normalization | In scope |
| Scripted file-locker downloader | In scope, **last phase**, feature-flagged, generic browser-driven, must degrade gracefully (owner wants near-zero maintenance) |
| Notifications (email reply + Teams webhook) | In scope |
| Per-journalist subfolders + source archive with retention | In scope |
| MCR panel access | Open without login only from an IP allowlist; everything else requires login; admin and destructive actions always require login |

---

## 0. Ground rules for the implementing agent

1. **Read `AGENTS.md` and `SKILL.md` first.** The broadcast constants in section 2 of AGENTS.md are non-negotiable (Sony XDCAM HD422 1080i50, 50 Mbps CBR, GOP 12, yuv422p, Rec.709, EBU R48 8×mono PCM 24-bit 48 kHz, RDD9 OP1a via bmxtranswrap, atomic `.tmp` → rename delivery).
2. **Pure Rust, single binary.** No Python, Node, npm, or sidecars. New third-party crates are allowed if they are pure Rust or vendor their C (like rusqlite `bundled`). Every new crate must be justified in the PR description.
3. **Never touch `IngestGuard/` or `athena_ingest/`** (legacy reference only, if present).
4. **Work phase by phase, in order.** Each phase ends with: `cargo build --release` with 0 warnings, `cargo test --workspace` green, `scripts/in_house_test.ps1` green, and the phase's own acceptance checklist ticked. Do not start the next phase with a red build.
5. **Every bug fix gets a regression test** that fails before and passes after.
6. **Do not change the on-air file format.** If a change to the ffmpeg/bmxtranswrap command line is proposed, it must be justified against AGENTS.md section 2 and validated by the compliance gate (Phase 1.6) and the in-house suite.
7. **Secrets never land in git, logs, or config.json in plaintext.** See Phase 2.6.
8. **Commit granularity:** one commit per work item (numbered below, e.g. `P1.3`). Commit messages start with the item id.
9. **When in doubt about a broadcast rule, stop and ask.** When in doubt about anything else, pick the option that reduces operator maintenance and document the assumption at the top of the PR.

---

## 1. Current-state audit (defect register)

Severity: **S1** = will corrupt output, lose jobs, or expose the station; **S2** = wrong behaviour operators will hit weekly; **S3** = quality/maintainability.

### 1.1 Queue and pipeline

| Id | Sev | Where | Defect |
|---|---|---|---|
| D-01 | S1 | `src/main.rs:325-337` | Job is leased (status → DOWNLOADING) **before** the semaphore permit is acquired. With `max_concurrent_downloads=2` and 20 pending jobs, 18 sit at DOWNLOADING with no worker, and the MCR panel shows phantom activity. |
| D-02 | S1 | `src/main.rs`, `repository.rs` | No crash/restart recovery. Jobs in DOWNLOADING/TRANSCODING/REWRAPPING at shutdown are orphaned forever. |
| D-03 | S1 | `downloader.rs`, `transcoder.rs`, `rewrapper.rs` | No stage timeouts, no `kill_on_drop`, no process-tree kill. A hung yt-dlp/ffmpeg holds a worker slot indefinitely. |
| D-04 | S1 | `delivery.rs:51-61` | `cleanup_job_temp_files` matches prefix `"{id}_"`, so job 1 deletes temp files of jobs 10–19, 100–199, … Concurrent jobs corrupt each other. |
| D-05 | S1 | `delivery.rs:37-39` | Existing `{slug}.mxf` in the watchfolder is deleted and overwritten. Slug collisions (same index+journalist+keyword) silently replace an asset already in Dalet. |
| D-06 | S1 | `pipeline.rs` | No output compliance gate. Nothing verifies the MXF before delivery. |
| D-07 | S1 | `transcoder.rs:80` | `get_media_info` failure → `(0.0, 0)` → treated as "no audio" → silent clip delivered with no warning. |
| D-08 | S1 | `transcoder.rs:137` | `tinterlace=interleave_top` halves the frame rate. For a 25p source this yields 12.5 fps that `-r 25` then duplicates (judder). Only 50p sources (like the synthetic test) produce correct 25i. Interlaced sources are re-interlaced (field tearing). |
| D-09 | S2 | `transcoder.rs:97-121` | Only `[0:a:0]` is used; 5.1 sources lose centre channel; wrong-language track may be chosen; no loudness control. |
| D-10 | S2 | `repository.rs:73` | `queue.url UNIQUE`. The same link can never be queued twice (different journalist, re-send after discard failure, override to an already-known URL fails with a DB error). |
| D-11 | S2 | `repository.rs:398-421`, `:655-671` | `created_at`/`updated_at`/`timestamp` are replaced with `Utc::now()` on read. Archive "Completed At" is always "now". |
| D-12 | S2 | `main.rs:364-392` | yt-dlp is always tried first, even for news portals where it takes 10–30 s to fail via the generic extractor, before the sniffer runs. |
| D-13 | S2 | `sniffer.rs:93-97`, `browser.rs` | A new browser per job, no pool, no launch timeout, browser never explicitly closed → zombie `msedge.exe` on task cancellation. `--no-sandbox` unnecessary on Windows. |
| D-14 | S2 | `sniffer.rs:426-439` | Early exit at score ≥95 after 3 s picks the first DOM embed even when the real HLS (score 100) or a better-positioned embed appears later. Ties resolved arbitrarily. |
| D-15 | S2 | `sniffer.rs` | Cross-origin iframes run out-of-process; their network events are invisible to the page session (root cause of in.gr #60). |
| D-16 | S2 | `dependencies.rs:148-169` | yt-dlp nightly update: no checksum, binary swapped while jobs may be running, no rollback. |
| D-17 | S2 | `downloader.rs:74` | `--no-check-certificates` always on. |
| D-18 | S2 | `downloader.rs:69-72` | Format string downloads 4K then downscales; no `--retries`, `--socket-timeout`, `--concurrent-fragments`; output file found by directory scan instead of `--print after_move:filepath`. |
| D-19 | S2 | `main.rs:281-287` | Updater fires whenever the hourly tick lands in hour 03; fine, but it has no jitter and no "skip if a job is running". |
| D-20 | S3 | `watcher.rs:210`, `repository.rs:96` | `journalists.default_priority` is never applied. |
| D-21 | S3 | `models.rs` | `Job.media_format` is a free-text default; `JobStatus::from_str_lossy` maps unknown to RequiresReview (hides DB corruption). |

### 1.2 Email and LLM

| Id | Sev | Where | Defect |
|---|---|---|---|
| E-01 | S1 | `config.rs:152`, `watcher.rs:82-88` | Basic-auth IMAP to `outlook.office365.com`. Microsoft retired basic auth for Exchange Online; this login fails on standard tenants. |
| E-02 | S1 | `watcher.rs:120-121` | Email flagged `\Seen` even when LLM/DB processing failed → links lost. |
| E-03 | S1 | `config.json:19` | Deployed `system_prompt` is a one-liner, overriding the few-shot prompt in code; model is `llama3:latest`, not the code default. Production parsing quality is far below what was tested. |
| E-04 | S2 | `llm.rs` | No structured-output mode, no validation that returned URLs exist in the email (hallucinated URLs get queued), 240 s timeout blocks the poll loop. |
| E-05 | S2 | `watcher.rs:113-114` | `Handle::block_on` inside `spawn_blocking`; nested runtime entry is fragile. |
| E-06 | S2 | `watcher.rs:237-262` | HTML bodies are passed raw (tags intact) when no text/plain part exists. |
| E-07 | S3 | `watcher.rs` | No idempotency by `Message-ID`; a mail re-delivered by the server is processed twice. |
| E-08 | S3 | — | Video attachments in emails are ignored (journalists do attach .mp4/.mov). |

### 1.3 Web, auth, security

| Id | Sev | Where | Defect |
|---|---|---|---|
| W-01 | S1 | `server.rs:53`, `routes.rs:540` | `POST /api/setup` unauthenticated: anyone on the LAN can change watchfolder path, mail credentials, LLM endpoint. |
| W-02 | S1 | `server.rs:34-40` | Job override/retry/discard, journalist create/delete, `/api/system/logs`, `/api/system/test-email` (an IMAP credential oracle) are unauthenticated. |
| W-03 | S1 | `mcr.html:293-331`, `user.html:150-192`, `admin.html:246-316` | Stored XSS: `url`, `slug`, `notes`, `error_message`, `full_name`, `email`, audit `message` are injected via `innerHTML` unescaped. Content originates from emails and journalist input. |
| W-04 | S1 | `repository.rs:117-125` | Default `admin@newsroom.local / admin123` seeded automatically. |
| W-05 | S1 | `omni-cli/src/lib.rs:163-164` | Setup wizard hashes the admin password, then `create_user` hashes the hash → admin cannot log in. |
| W-06 | S1 | `config.rs`, `AppConfig::save_to_file` | Mail password (and future OAuth secret) stored in plaintext JSON. |
| W-07 | S2 | `server.rs:16-19`, `auth.rs:34` | CORS `Any`, no CSRF protection, cookie without `Secure`, no login rate limit, no security headers. |
| W-08 | S2 | all `assets/*.html` | Tailwind, Lucide and Google Fonts loaded from public CDNs. MCR workstations may be offline; supply-chain exposure. |
| W-09 | S2 | `routes.rs:465-481` | `mail_status`/`llm_status` hardcoded to "Active"/"Ready". |
| W-10 | S2 | `config.rs:321-328`, `adblock.rs:127`, `routes.rs:431` | Relative paths resolve against the process CWD. Under the Windows service the CWD is `System32`. |
| W-11 | S2 | `main.rs` | Logs go to stdout only; nothing is captured when running as a service. No Windows Event Log integration. |
| W-12 | S3 | `routes.rs:292`, `mcr.html` | `GET /api/jobs` returns the whole table on every SSE tick; no paging. |
| W-13 | S3 | `service/lib.rs:111` | Service installs as LocalSystem, which cannot authenticate to SMB shares → watchfolder on a NAS/Dalet share will fail. |

### 1.4 Tests and benchmarks

| Id | Sev | Where | Defect |
|---|---|---|---|
| T-01 | S2 | `broadcast_compliance_tests.rs` | All three tests assert constants against themselves; they cannot fail. |
| T-02 | S2 | `scripts/benchmark_corpus.ps1` | "Success" means "a stream URL was found", not "a compliant MXF was delivered". No per-stage timing. |
| T-03 | S3 | `in_house_test.ps1:86-92` | Synthetic source is 50p only, so D-08 was never caught. |
| T-04 | S3 | corpus #61 | `theguardian.com/world/world+content/video` is a listing page, not a video; it is not a valid target. |

### 1.5 Benchmark failures, root causes

| # | Site | Root cause | Fix (phase) |
|---|---|---|---|
| 56 | iefimerida `?amp` | AMP markup (`amp-iframe`, `amp-youtube`, `amp-video`) is not scanned; canonical URL not retried | P3.4 |
| 60 | in.gr | Player is in a cross-origin iframe (OOPIF); CDP page session does not see its requests | P3.2 |
| 61 | Guardian hub | Invalid target (listing page) + Sourcepoint consent in iframe | P6.4 corpus fix; P3.4 Sourcepoint |
| 65 | x.com | X requires an authenticated session for video | P3.5 cookie jar |

---

## 2. Target architecture

```
omni-ingest.exe
├── omni-core        config v2, migrations, repository (state machine), secrets (DPAPI), process runner (timeouts + job objects), paths, url normalizer, transliteration
├── omni-broadcast   extraction router, downloader, probe, transcoder (decision matrix + loudnorm), rewrapper, verifier (compliance gate), delivery (layout + archive), pipeline (stage machine)
├── omni-browser     browser pool, sniffer (OOPIF-aware, AMP, consent), adblock, locker resolver (generic download capture, P8)
├── omni-email       mail sources (Graph OAuth2 primary, IMAP secondary), deterministic parser, LLM assist, attachments, notifications (Graph sendMail/reply)
├── omni-notify      (new, small) Teams webhook + notification outbox with retries
├── omni-web         axum server, auth layer (roles + IP allowlist + CSRF), routes, embedded static assets (no CDN), SSE
├── omni-service     Windows SCM, service account install, Event Log
├── omni-cli         setup wizard, service cmds, benchmark runner, parser test bench, secrets set
└── src/main.rs      daemon supervisor: web, mail, scheduler, worker pool, reaper, updater, retention
```

Key design principles applied throughout:

- **Explicit state machine with leases and heartbeats** for every job; every stage has a timeout and a recorded start time.
- **Acquire capacity first, then lease.** Workers never take a job they cannot start.
- **Deterministic before probabilistic.** Domain routing table and per-domain success history decide the extraction order; LLM is optional assist.
- **Verify before deliver.** ffprobe-based compliance report is stored on the job; failure never reaches the watchfolder.
- **Self-tuning, not hand-tuned.** Per-domain stats, cookie jars, and blocklists are managed in the DB and admin panel; no code edits for operational tuning.
- **Everything relative to the install directory**, never the CWD.

---

## 3. Phase 0 — Safety net (est. 1.5 days)

### P0.1 Anchor all paths to the install directory (W-10)
- Add `omni_core::paths::AppPaths { root, config, data, temp, bin, logs, archive }`.
- `root` = directory of `std::env::current_exe()` unless `--root <dir>` or `OMNI_ROOT` is given.
- `AppConfig::resolve_path` resolves relative paths against `root`, not CWD.
- `UnifiedAdBlocker::global()` becomes `UnifiedAdBlocker::init(paths.data.join("adblock"))` called once from `run_daemon`/CLI before use; `global()` panics with a clear message if not initialized.
- `DependencyManager::new` receives an absolute `bin` dir from `AppPaths`.
- Test: run the binary from `C:\Windows\System32` as CWD with `--config` relative; assert DB and adblock cache land under the exe directory.

### P0.2 Schema migrations framework
- Add `schema_version` table. Replace `run_migrations` with an ordered `MIGRATIONS: &[(u32, &str)]` applied in a transaction; each migration runs once.
- Before applying migrations, copy `omni.db` to `data/backups/omni.db.{schema_version}.{timestamp}` (skip if file > 2 GB; log a warning instead).
- Keep `journal_mode=WAL`, `busy_timeout=30000`, `foreign_keys=ON`, add `synchronous=NORMAL`.
- Test: create a v1 DB from the current schema fixture, open with the new code, assert all new columns exist and data survived.

### P0.3 Real timestamps (D-11)
- Store `created_at`/`updated_at` as RFC3339 UTC text (`strftime('%Y-%m-%dT%H:%M:%fZ','now')`); read them back with `DateTime::parse_from_rfc3339`, falling back to SQLite `CURRENT_TIMESTAMP` format for old rows.
- Same for `audit_logs.timestamp` and `sessions.expires_at` (which must also compare correctly with `expires_at > ?` using RFC3339 strings, not `CURRENT_TIMESTAMP`).
- Test: insert, sleep 1100 ms, update; assert `updated_at > created_at`.

### P0.4 Replace tautological tests (T-01)
- Delete the three constant-vs-constant tests. Replace with `transcoder_command_contract` tests that build the ffmpeg argument vector via a pure function `Transcoder::build_args(probe: &SourceProbe, ...) -> Vec<String>` and assert the exact broadcast flags are present, for each branch of the decision matrix (P1.5).

### P0.5 Fix setup wizard double hash (W-05)
- `omni-cli/src/lib.rs:163-164`: pass the plaintext password to `create_user`; delete the `hash_password` call. Test: create via wizard code path, then `verify_password`.

### P0.6 Process runner
- `omni_core::process::run(cmd, RunOpts { timeout, stdout: Capture|Lines(cb), stderr_tail_bytes: 8192, kill_tree: true })`.
- Windows: assign the child to a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (use the `windows` crate: `CreateJobObjectW`, `SetInformationJobObject`, `AssignProcessToJobObject`) so yt-dlp's ffmpeg children die with it. `kill_on_drop(true)` on the tokio `Command`.
- Always `CREATE_NO_WINDOW`.
- Returns `RunOutcome { status, elapsed, stderr_tail, timed_out }`. Errors include the stderr tail.
- Test: spawn `cmd /c ping -n 30 127.0.0.1` with 1 s timeout → returns timed_out, no leftover process (check via `tasklist`).

**Phase 0 acceptance:** build clean; tests green; running from any CWD works; DB migrated with backup present.

---

## 4. Phase 1 — Queue and pipeline reliability (est. 5 days)

### P1.1 New job state machine and schema (D-01, D-02, D-10, D-20)

Schema (migration 2):

```sql
ALTER TABLE queue ADD COLUMN url_normalized TEXT;
ALTER TABLE queue ADD COLUMN stage TEXT NOT NULL DEFAULT 'QUEUED';         -- QUEUED, EXTRACT, DOWNLOAD, TRANSCODE, REWRAP, VERIFY, DELIVER, DONE
ALTER TABLE queue ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE queue ADD COLUMN max_attempts INTEGER NOT NULL DEFAULT 3;
ALTER TABLE queue ADD COLUMN lease_owner TEXT;                              -- "{hostname}:{pid}:{worker_n}"
ALTER TABLE queue ADD COLUMN lease_expires_at TEXT;
ALTER TABLE queue ADD COLUMN stage_started_at TEXT;
ALTER TABLE queue ADD COLUMN error_code TEXT;                               -- see P1.9
ALTER TABLE queue ADD COLUMN source_path TEXT;                              -- downloaded source (archived)
ALTER TABLE queue ADD COLUMN compliance_json TEXT;                          -- P1.6 report
ALTER TABLE queue ADD COLUMN candidates_json TEXT;                          -- P3.3 alternatives
ALTER TABLE queue ADD COLUMN extraction_method TEXT;                        -- direct|adapter:<name>|sniffer|attachment|locker
ALTER TABLE queue ADD COLUMN delivered_at TEXT;
ALTER TABLE queue ADD COLUMN completed_at TEXT;
ALTER TABLE queue ADD COLUMN email_message_id TEXT;                         -- Graph message id or Message-ID
ALTER TABLE queue ADD COLUMN stage_timings_json TEXT;                       -- {"download_ms":..,"transcode_ms":..}
CREATE INDEX idx_queue_status_prio ON queue(status, priority DESC, created_at ASC);
CREATE INDEX idx_queue_url_norm ON queue(url_normalized);
CREATE TABLE job_events (id INTEGER PRIMARY KEY, job_id INTEGER NOT NULL, at TEXT NOT NULL, stage TEXT, level TEXT, message TEXT, FOREIGN KEY(job_id) REFERENCES queue(id) ON DELETE CASCADE);
CREATE INDEX idx_job_events_job ON job_events(job_id, id);
```

Drop the `UNIQUE` on `queue.url` (SQLite: recreate table via `CREATE TABLE queue_new … INSERT … DROP … RENAME`).

`JobStatus` becomes: `Pending, Running, Completed, Failed, RequiresReview, ManualDownload, Cancelled, CompletedManual`. The fine-grained stage lives in `stage`. Keep `as_str` values backwards-compatible for `PENDING/COMPLETED/FAILED/REQUIRES_REVIEW/MANUAL_DOWNLOAD`; map legacy `DOWNLOADING/TRANSCODING/REWRAPPING/EXTRACTING` rows to `Running` + corresponding `stage` in migration 2 and then reset them to `Pending` (they are orphans from the old code).

Repository API:
- `lease_job(owner, lease_secs) -> Option<Job>`: `BEGIN IMMEDIATE; SELECT … WHERE status='PENDING' ORDER BY priority DESC, created_at ASC LIMIT 1; UPDATE … SET status='RUNNING', stage='EXTRACT', lease_owner=?, lease_expires_at=now+lease, stage_started_at=now, attempts=attempts+1; COMMIT`.
- `heartbeat(job_id, owner, lease_secs)` every 30 s from the worker; fails if owner mismatch (job was reaped) → worker aborts its work.
- `set_stage(job_id, stage)`, `record_event(job_id, level, msg)`, `finish(job_id, outcome)`.
- `reap_expired_leases()`: RUNNING with `lease_expires_at < now` → if `attempts < max_attempts` then PENDING with event "lease expired, requeued", else REQUIRES_REVIEW with `error_code=LEASE_EXPIRED`.
- `recover_on_startup(hostname)`: all RUNNING rows whose `lease_owner` starts with `hostname:` → PENDING (attempt already counted) with event "recovered after restart". Called before the worker pool starts.
- Priority at enqueue = `max(explicit priority, journalists.default_priority)` (D-20).

Worker pool (`src/main.rs` → move to `omni_broadcast::worker`):
```
loop:
  permit = semaphore.acquire_owned().await       // capacity FIRST
  job = repo.lease_job(owner, 120)               // then lease
  if none: drop(permit); sleep(poll_ms with jitter 1000-1500); continue
  spawn(process(job, permit))                     // permit dropped when done
```
Heartbeat task per job; if heartbeat fails (owner mismatch) → cancel the stage via `CancellationToken`.

Idempotent enqueue (`repo.enqueue(NewJob)`): compute `url_normalized` (P1.2); if a non-terminal job with the same `url_normalized` exists → return `Enqueue::DuplicateActive(existing_id)`; if a COMPLETED job with the same `url_normalized` **and** same journalist exists within `dedup_window_hours` (default 24) → `Enqueue::DuplicateRecent(existing_id)` (still audit-logged, and the email parser reports it in the reply); otherwise insert.

Tests: stress test updated (8 workers, 40 jobs, permits=2, assert at most 2 RUNNING at any instant); kill-recovery test (lease, drop owner, run `recover_on_startup`, assert PENDING); reaper test.

### P1.2 URL normalizer (`omni_core::urlnorm`)
- Lowercase scheme+host; strip `www.`? No — keep host as is except `m.youtube.com`→`www.youtube.com`, `youtu.be/{id}`→`www.youtube.com/watch?v={id}`, `youtube.com/shorts/{id}`→`watch?v={id}`; drop query keys `utm_*, fbclid, gclid, si, feature, t, igshid, ref_src, ref_url`; drop fragment; trim trailing `/`; `x.com`↔`twitter.com` → `x.com`; `?amp` and `/amp/` removed (canonical is retried by the sniffer anyway).
- Tests with a table of 20 inputs.

### P1.3 Stage timeouts and cancellation (D-03)
Config (`pipeline` section): `extract_timeout_secs=90`, `download_timeout_secs=1800`, `transcode_timeout_factor=6.0` (× source duration, min 600 s), `rewrap_timeout_secs=600`, `deliver_timeout_secs=900`, `max_source_duration_secs=5400` (longer → REQUIRES_REVIEW with `SOURCE_TOO_LONG`, MCR can force).
Each stage runs under `tokio::time::timeout` + `CancellationToken` and the P0.6 runner. On timeout: kill tree, clean temp, set `error_code=<STAGE>_TIMEOUT`, requeue if attempts remain (download/extract) or REQUIRES_REVIEW (transcode/rewrap, since retrying won't help without a change).

### P1.4 Per-job temp workspace (D-04)
- `temp/jobs/{job_id}/` directory per job. Cleanup = `remove_dir_all` of that directory only. Delete the prefix-based cleanup.
- On startup: remove `temp/jobs/*` directories whose job is not RUNNING.
- Disk guard before download: require `free_space(temp) > max(20 GB, 3 × estimated_size)`; estimated size from yt-dlp `--dump-json` `filesize_approx` if available, else 4 GB. Below threshold → job stays PENDING with event `LOW_DISK`, and the status endpoint raises a warning.

### P1.5 Transcoder decision matrix (D-07, D-08, D-09, loudness)

New `omni_broadcast::probe::SourceProbe` via `ffprobe -v error -print_format json -show_streams -show_format -show_entries stream=index,codec_type,codec_name,width,height,r_frame_rate,avg_frame_rate,field_order,pix_fmt,channels,channel_layout,sample_rate,duration,disposition,tags:format=duration,size`. If ffprobe fails or reports no video stream → `error_code=PROBE_FAILED` → REQUIRES_REVIEW. Never assume silence on probe failure (fixes D-07).

Video stream selection: the video stream with highest `width*height` that is not `disposition.attached_pic`.

Video filter chain (all branches end with `scale=1920:1080:force_original_aspect_ratio=decrease:interl=<I>:flags=bicubic,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black,format=yuv422p` where `<I>=1` only for the interlaced pass-through branch):

| Source | Chain before scale/pad | Rationale |
|---|---|---|
| Progressive, any fps | `fps=50,` … `,tinterlace=mode=interleave_top:flags=vlpf` | 50 progressive frames → 25 interlaced frames. 25p becomes PsF (both fields from the same frame), 30p/60p is rate-converted to 50p first (drop/dup), then interlaced. Fixes the 12.5 fps bug. |
| Interlaced TFF, 25 fps (`field_order=tt`) | (none) … `setfield=tff` with `scale=…:interl=1` | Preserve original fields; do not re-interlace. |
| Interlaced BFF, 25 fps (`field_order=bb`) | `yadif=mode=send_field:parity=bff,fps=50,` … `,tinterlace=interleave_top` | Convert field dominance via deinterlace at field rate then re-interlace TFF. |
| Interlaced at 29.97/30 | `yadif=mode=send_field,fps=50,` … `,tinterlace=interleave_top` | Rate conversion needs progressive intermediate. |
| Vertical/odd aspect | handled by scale+pad (pillarbox) | unchanged |

Output flags unchanged from AGENTS.md: `-c:v mpeg2video -b:v 50M -minrate 50M -maxrate 50M -bufsize 17825792 -profile:v 0 -level:v 2 -pix_fmt yuv422p -g 12 -bf 2 -flags +ildct+ilme -top 1 -r 25 -aspect 16:9 -color_primaries bt709 -color_trc bt709 -colorspace bt709`. Add `-color_range tv`, `-dc 10` (higher DC precision improves XDCAM quality at the same bitrate; verify with the compliance gate), `-video_track_timescale`? not needed for MXF. Keep `-trellis 0` for speed unless benchmarks show headroom.

Audio selection and processing:
1. Choose audio stream: prefer `disposition.default`, then language tag `el`, then `en`, then most channels. None → 8 × silence (event `NO_AUDIO_SOURCE`, and the MCR card shows a "silent" badge).
2. Downmix to stereo: mono → `pan=stereo|c0=c0|c1=c0`; stereo → as is; 5.1 → `pan=stereo|FL=FC+0.30FL+0.30BL|FR=FC+0.30FR+0.30BR` (ITU-R BS.775 style); other → `pan=stereo|c0=c0|c1=c1`.
3. `aresample=48000:async=1:first_pts=0`.
4. **EBU R128 loudness (in scope):** two-pass `loudnorm=I=-23:LRA=7:TP=-1:print_format=json` on the stereo downmix. Pass 1: `ffmpeg -i src -vn -af "<downmix>,aresample=48000,loudnorm=I=-23:LRA=7:TP=-1:print_format=json" -f null -` (parse the JSON from stderr). Pass 2 uses `measured_I/measured_LRA/measured_TP/measured_thresh/offset` with `linear=true`. If pass 1 fails or the source is shorter than 3 s, fall back to single-pass `loudnorm` (dynamic mode). Config `audio.loudnorm_enabled=true`, `audio.target_lufs=-23`, `audio.true_peak=-1`. Store measured/output loudness in `compliance_json`.
5. Split into L and R mono: `[a]asplit=2[al][ar];[al]pan=mono|c0=c0[l];[ar]pan=mono|c0=c1[r]`; map `[l]`, `[r]`, then 6 × `anullsrc=r=48000:cl=mono` (`-shortest`). Output `-c:a pcm_s24le -ar 48000`.

Expose `Transcoder::build_plan(&SourceProbe, &AudioMeasurement) -> TranscodePlan { args: Vec<String>, branch: &'static str }` as a pure function for unit tests (P0.4). Progress parsing: use `-progress pipe:1 -nostats` (key=value lines: `out_time_us`, `speed`) instead of regex on stderr.

Tests: build `build_plan` for probes {25p, 30p, 50p, 60p, 25i-tff, 25i-bff, 29.97i, mono, stereo, 5.1, no-audio} and assert branch + flags. Integration (in-house suite P6.3): synthetic sources at 25p/30p/50p/25i → output verified by P1.6 **and** frame count == round(duration × 25) ± 1 and `idet` classifies output as TFF.

### P1.6 Compliance gate (D-06)
`omni_broadcast::verify::verify_mxf(path, ffprobe) -> ComplianceReport { pass: bool, checks: Vec<Check{name, expected, actual, ok}> }`. Checks:
- format_name contains `mxf`; exactly 1 video stream; codec `mpeg2video`; 1920×1080; `pix_fmt=yuv422p`; `r_frame_rate=25/1`; `field_order=tt`; `profile` = 4:2:2; bit_rate within 45–55 Mbps when reported; `color_primaries/color_transfer/color_space = bt709`.
- exactly 8 audio streams; each `pcm_s24le`, 48000 Hz, 1 channel.
- duration ≥ 0.5 s and within ±2 % (or ±0.5 s) of source duration, unless `max_source_duration` trimming applied.
- file size > 1 MB and ≈ 50 Mbps × duration ± 20 %.
- Optional (config `verify.bmx_check=true`): run `bmxtranswrap --check-end` or `mxf2raw --info` if `mxf2raw.exe` is present in `bin/`; treat failure as a check failure.
Stage VERIFY runs it; `pass=false` → REQUIRES_REVIEW with `error_code=COMPLIANCE_FAILED`, report stored, nothing delivered. Test with a deliberately wrong file (stereo interleaved, 720p) → fails.

### P1.7 Delivery layout, collisions, archive (D-05, per-journalist, archive)
- Config `delivery.layout = "per_journalist" | "flat"` (default `per_journalist`), destination = `watchfolder/{JOURNALIST}/` (created if missing; journalist folder name sanitized `[A-Z0-9_]{1,32}`).
- Slug sanitization at enqueue: `[A-Z0-9_]` only, Greek transliterated (P4.3), max 60 chars, no leading digit issues (Dalet accepts digits; keep).
- Collision policy: if `{slug}.mxf` or `.{slug}.mxf.tmp` exists in the destination, or another job delivered that slug in the last 7 days → deliver as `{slug}_2.mxf`, `_3`, … Record the final filename on the job; event `SLUG_SUFFIXED`. Never delete a file in the watchfolder.
- Atomic delivery: always copy to `.{slug}.mxf.tmp` **in the destination directory** (skip the same-volume rename shortcut; it is fast anyway and the copy path is the one that must be correct on SMB), `sync_all()`, verify size equals source size, then `rename`. On SMB, a rename of a hidden dotfile is atomic within the directory.
- bmxtranswrap: add `--clip "{slug}"` so the MXF material package name equals the slug (Dalet shows it as title). Verify in P1.6 via `format.tags.material_package_name` if ffprobe exposes it; otherwise skip the check.
- **Archive (in scope):** after successful delivery, move the downloaded source to `archive/{YYYY}/{MM}/{DD}/{job_id}_{slug}.{ext}` and write `{job_id}_{slug}.json` sidecar (job row + compliance report + candidates + timings). Config `archive.enabled=true`, `archive.retention_days=14`, `archive.path="archive"`. Nightly retention task deletes expired directories; disk guard also purges oldest archive first if temp disk is low.

### P1.8 Downloader hardening (D-17, D-18)
- yt-dlp args: `-f "bestvideo[height<=1080][vcodec^=avc1]+bestaudio[acodec^=mp4a]/bestvideo[height<=1080]+bestaudio/best[height<=1080]/best" --merge-output-format mp4 --no-playlist --retries 5 --fragment-retries 10 --retry-sleep exp=1:30 --socket-timeout 30 --concurrent-fragments 4 --newline --progress-template "download:%(progress.downloaded_bytes)s/%(progress.total_bytes_estimate)s %(progress.speed)s %(progress.eta)s" --print after_move:filepath --no-warnings -o "<temp>/<job>/source.%(ext)s"`.
- `--no-check-certificates` only when config `download.insecure_tls=true` (default false).
- Session context: `--referer`, `--user-agent`, `--add-header "Cookie: …"` as today; plus `--cookies <jar>` when a domain cookie jar exists (P3.5).
- yt-dlp **JS runtime:** recent yt-dlp releases require an external JavaScript runtime (Deno) for full YouTube support. The implementer must check the bundled `yt-dlp --version` release notes and, if the option exists, bundle `deno.exe` in `bin/` and pass the corresponding `--js-runtimes` flag; add Deno to `DependencyManager::scan` and to the admin Tools card. Verify by downloading corpus #1 and confirming no "n challenge" warnings in stderr.
- Direct `.m3u8`/`.mp4`/`.mpd` URLs go through yt-dlp too (uniform progress and headers), with `--hls-use-mpegts` off.
- Output path from `--print after_move:filepath` (last stdout line that is an existing path). Remove the directory scan.
- Error classification from stderr tail (P1.9).

### P1.9 Error codes and retry policy
`error_code` values and behaviour:

| Code | Trigger (stderr/regex or condition) | Retry? | MCR hint |
|---|---|---|---|
| `UNSUPPORTED_URL` | `Unsupported URL` | no (go to sniffer) | "Try pasting the direct video link" |
| `HTTP_403` | `HTTP Error 403` | once via sniffer context | "Origin blocks direct download" |
| `LOGIN_REQUIRED` | `login|sign in|cookies` messages | no | "Needs a cookie jar for this site (Admin → Cookies)" |
| `GEO_BLOCKED` | `not available in your country` | no | — |
| `PRIVATE_OR_REMOVED` | `Private video|removed|unavailable` | no | "Ask the journalist for another link" |
| `LIVE_STREAM` | `is a live event|live stream` | no | "Live: wait for VOD" |
| `NETWORK` | timeouts, `Connection reset`, DNS | yes (backoff 30 s, 2 min, 5 min) | — |
| `NO_STREAM_FOUND` | sniffer empty | no | "Paste stream URL from browser DevTools" |
| `PROBE_FAILED`, `TRANSCODE_FAILED`, `REWRAP_FAILED`, `COMPLIANCE_FAILED`, `DELIVERY_FAILED`, `LOW_DISK`, `SOURCE_TOO_LONG`, `*_TIMEOUT`, `LEASE_EXPIRED` | as named | `DELIVERY_FAILED`, `LOW_DISK`, `NETWORK` retry; others no | specific text |

Retries happen by requeueing with `priority` unchanged and a `not_before` time (add column `not_before TEXT`; `lease_job` filters `not_before IS NULL OR not_before <= now`).

### P1.10 Pipeline orchestration (`pipeline.rs` rewrite)
Stages: `EXTRACT → DOWNLOAD → PROBE → TRANSCODE → REWRAP → VERIFY → DELIVER → ARCHIVE → DONE`. Each stage: set stage + `stage_started_at`, run under timeout, record `stage_timings_json`, emit `job_events`, broadcast SSE `{"type":"job","id":…,"status":…,"stage":…,"progress":…}` (structured JSON instead of bare strings; the UI updates a single card instead of refetching everything).

**Phase 1 acceptance:** kill `omni-ingest.exe` with `taskkill /F` mid-transcode, restart → job requeued and completes; 40-job synthetic soak with 2 workers → never more than 2 RUNNING, no cross-job temp deletion, all outputs pass the gate; 25p synthetic test yields 25 fps TFF output with correct frame count; slug collision produces `_2`; source archived with sidecar.

---

## 5. Phase 2 — Security hardening (est. 3 days)

### P2.1 Auth layer (W-01, W-02, MCR allowlist)
- `omni_web::auth::Principal { kind: User(User) | OpenMcr(ip) , role: Admin|Mcr|User }` extracted per request from the `omni_session` cookie, or — when no valid session — from the client IP if it matches `security.mcr_open_networks` (list of CIDRs, default `["127.0.0.1/32", "::1/128"]`; admin sets the newsroom subnet). Use `axum::extract::ConnectInfo<SocketAddr>`; if `security.trust_proxy_header=true`, read `X-Forwarded-For` (only when the direct peer is in `security.trusted_proxies`).
- Route policy (enforced by extractors `RequireRole(Admin)`, `RequireMcr` = Admin|Mcr user|OpenMcr, `RequireUser` = any logged-in):

| Route | Policy |
|---|---|
| `GET /mcr`, `GET /api/jobs`, `GET /api/jobs/:id`, `GET /api/journalists`, `GET /api/system/status`, `GET /api/events` | RequireMcr (open on allowlist) |
| `POST /api/jobs` | RequireUser **or** OpenMcr |
| `POST /api/jobs/:id/{override,retry,discard,cancel,choose-candidate,mark-manual-done}` | RequireMcr |
| `POST/DELETE /api/journalists*` | RequireMcr |
| `GET /api/jobs?my=true` | RequireUser |
| `GET /user`, `POST /api/auth/password` | RequireUser |
| `GET /admin`, `/api/admin/*`, `/api/system/logs`, `/api/system/test-*`, `/api/config*`, `/api/secrets*`, `/api/benchmarks*` | RequireRole(Admin) |
| `POST /api/setup` | Admin, **or** loopback client while no admin exists (first run) |
| `/login`, `/api/auth/login`, `/api/health` | public |

- Remove `auth_mode` enum; replace with the allowlist (`mcr_open_networks=[]` means "login for everyone").
- Tests: auth matrix test iterating every route × {anonymous, anonymous-on-allowlist, user, mcr, admin} asserting the expected status.

### P2.2 CSRF and session hardening (W-07)
- Cookie: `HttpOnly; SameSite=Strict; Path=/; Secure` (Secure only when TLS enabled, see P2.7). Session lifetime: `user` 12 h idle / 14 d absolute with "remember me"; `admin` 8 h absolute; `mcr` role 30 d. Sessions table gains `created_at, last_seen_at, ip, user_agent`; rotate token on login; `POST /api/auth/logout-all`.
- CSRF: all state-changing requests must carry header `X-Omni-Request: 1` and an `Origin`/`Referer` whose host matches the server `Host`. Reject otherwise (403). Front-end sets the header in one `api()` helper.
- CORS: remove the permissive layer entirely (same-origin app). If a future integration needs it, it is configured explicitly.
- Login rate limit: 5 failures per IP per 5 minutes and 10 per account per hour (in-memory `DashMap` with expiry); lockout responds 429 with `Retry-After`. Audit-log every failure with IP.
- Argon2id explicit params: m=64 MiB, t=3, p=1. Verify existing hashes still validate (PHC string carries params).
- Security headers middleware: `Content-Security-Policy: default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self'; connect-src 'self'; frame-ancestors 'none'`, `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`, `Cache-Control: no-store` on API responses.

### P2.3 Output escaping (W-03)
- Rewrite front-end rendering to build DOM nodes (`document.createElement`, `textContent`) or use a single `esc()` helper for every interpolated value; `href` values validated to `http(s):` before insertion. Add a regression test that creates a job with URL `https://x/"><img src=x onerror=alert(1)>` and asserts the served HTML/JS never emits the raw string unescaped (test the JSON API escaping + a static grep test that no `innerHTML = \`…${` remains in assets).

### P2.4 Remove default admin (W-04)
- No seeded admin. First run: `omni-ingest setup` creates one, or `/setup` served only to loopback clients while `users` has no admin. Once an admin exists, `/setup` returns 404. Startup logs a loud warning if no admin exists.

### P2.5 Embedded assets (W-08)
- Remove Tailwind CDN, Lucide CDN, Google Fonts. Ship `assets/app.css` (hand-written, ~400 lines, same dark glass look), `assets/app.js` (shared `api()`, `esc()`, SSE client, i18n), `assets/icons.svg` (sprite with the ~25 icons used), system font stack. All served by `rust-embed` with `Cache-Control: public, max-age=86400` and content hash in the query string.

### P2.6 Secrets at rest (W-06)
- `omni_core::secrets::SecretStore` backed by Windows DPAPI machine scope (`CryptProtectData`/`CryptUnprotectData` from the `windows` crate) → `data/secrets.bin` (JSON map, encrypted blob). Non-Windows builds (tests/CI): fallback file store with a warning.
- Secrets: `graph.client_secret`, `imap.password` (legacy), `teams.webhook_url`, cookie jars (P3.5), `web.tls_key` if stored inline.
- `AppConfig` fields for secrets are removed; config.json holds only `graph.client_id`, `graph.tenant_id`, `graph.mailbox`. Migration: on first start, if `email_password` is present in config.json, move it into the store and rewrite config.json without it.
- Admin panel and CLI (`omni-ingest secrets set graph.client_secret`) are the only writers. Values are never returned by any API (only `is_set: true/false`). Logging layer redacts any string equal to a secret value.

### P2.7 Optional TLS
- Config `web.tls.cert_path`, `web.tls.key_path` (PEM). When set, serve HTTPS via `axum-server` with rustls and set cookie `Secure`. Document the recommended setup (IT-issued cert or reverse proxy). Default remains HTTP on the LAN.

### P2.8 Service account and least privilege (W-13)
- `omni-ingest service install --account "DOMAIN\svc_omni" --password-prompt` → `ServiceInfo.account_name/password`. Document: the account needs Modify on the watchfolder share, the install directory's `data/`, `temp/`, `logs/`, `archive/`, and "Log on as a service". Default remains LocalSystem for single-machine setups, with a warning in the wizard when the watchfolder is a UNC path.
- Browser and child processes inherit the service account; ensure `temp/browser-profile` is writable.

### P2.9 yt-dlp update integrity (D-16)
- Download `yt-dlp.exe` **and** `SHA2-256SUMS` from the same GitHub release (`/releases/latest/download/`), verify the hash, save as `yt-dlp.exe.staged`. Apply only when no job is in DOWNLOAD/EXTRACT (worker pool pauses leasing, waits for in-flight downloads ≤ 10 min, swaps `yt-dlp.exe`→`yt-dlp.exe.prev`, staged→live, resumes). Admin "Rollback" restores `.prev`. Record version + hash in `tool_versions` table. Nightly window jittered ±20 min. `ytdl_channel` stays `stable` by default; nightly channel allowed but flagged "higher breakage risk" in the UI.

**Phase 2 acceptance:** auth matrix test green; XSS regression green; no external network request when loading any page (check with the browser devtools Network tab offline); secrets absent from config.json and logs; login lockout works; service runs under a dedicated account writing to a UNC watchfolder.

---

## 6. Phase 3 — Extraction accuracy and browser robustness (est. 4 days)

### P3.1 Extraction router (D-12)
`omni_broadcast::extract::Router::plan(url) -> Vec<Strategy>` ordered by:
1. **Attachment/local** (from P4.6) → skip extraction.
2. **Direct platforms** (yt-dlp first): youtube.com, youtu.be, m.youtube.com, x.com, twitter.com, instagram.com, facebook.com, fb.watch, tiktok.com, vimeo.com, dailymotion.com, vk.com, direct media URLs (`.m3u8/.mp4/.mpd/.mov`), `cdn.jwplayer.com`, `player.glomex.com`, `mediastream.ert.gr`.
3. **Site adapters** (data-driven rules, P3.6) for domains that have one.
4. **Sniffer first** for everything else, then yt-dlp on the sniffed stream (with context).
5. If a strategy fails with `UNSUPPORTED_URL`/`NO_STREAM_FOUND`, the next runs; `HTTP_403` from yt-dlp triggers the sniffer to obtain context and retry once.

Per-domain learning: table `domain_stats(domain, method, success, fail, avg_ms, last_success_at)`. The router reorders strategies for a domain when one method has ≥5 samples and ≥80 % success. Visible in Admin → Benchmarks → "Domain routing". This is what keeps maintenance near zero: the system learns which path works per site.

### P3.2 Browser pool and OOPIF capture (D-13, D-15)
- `BrowserPool` (singleton): one headless browser process, launched lazily, reused, recycled after `browser.max_pages_per_instance=40` or 2 h or RSS > 1.5 GB; hard-killed via Job Object at shutdown. Per job: a fresh incognito browser context (`Target.createBrowserContext`) → page → closed in `Drop`. `browser.max_parallel_pages=2`.
- Launch flags: `--headless=new --disable-gpu --mute-audio --no-first-run --disable-extensions --disable-background-networking --disable-sync --disable-features=IsolateOrigins,site-per-process,Translate --user-data-dir=<temp>/browser-profile --window-size=1366,900 --lang=el-GR,el,en`. **Drop `--no-sandbox`.** The `site-per-process` disable makes cross-origin iframes render in-process so their network requests are visible on the page's CDP session (fixes in.gr, and any player served from a CDN iframe). Additionally enable `Network` events on child targets via `Target.setAutoAttach {autoAttach: true, flatten: true, waitForDebuggerOnStart: false}` if chromiumoxide 0.7 exposes it; if not, the flag alone is sufficient — verify with corpus #60 and #59 (Glomex iframe).
- Navigation timeout 20 s, total sniff budget `browser.sniff_timeout_secs=35`; page `Emulation.setFocusEmulationEnabled`, `Page.setBypassCSP`? (no — keep CSP), block `image/font/stylesheet` resource types via `Network.setBlockedURLs` patterns `*.png *.jpg *.gif *.webp *.woff*` to speed up (media detection does not need them), plus the adblock patterns (raise the CDP pattern list to the top ~300 domains by hit-count from the blocklists, measured once; keep under Chrome's limits).
- Kill/cleanup test: cancel a sniff mid-flight, assert no orphan browser processes (compare `tasklist` before/after).

### P3.3 Candidate selection (D-14)
- Keep collecting for the full budget unless a candidate is **decisive**: a network `.m3u8` master/`.mpd`/`.mp4` ≥ score 85 whose initiator frame is the top frame or an iframe inside `article, main, .article-body, .entry-content` (record the element rect and the `Network.requestWillBeSent.initiator`), **and** no other decisive candidate appeared within 4 s of it.
- Each candidate gets: `score`, `kind` (network|embed|adapter), `position` (document order index of its embed/iframe; network candidates inherit the position of their frame), `in_article: bool`, `duration_secs: Option<f64>` (for the top 3: `ffprobe -show_format` with referer/UA/cookies; ads are typically < 35 s), `title`.
- Ranking: exclude `duration < 20 s` when a longer candidate exists; prefer `in_article`; then score; then position. Store all candidates on the job (`candidates_json`).
- If the top two survive with different `position` and both `in_article` (listicles like news247 #57) → deliver the top one **and** flag the job `MULTI_CANDIDATE` (not blocking); the MCR card shows a "Other videos on this page (n)" picker that re-runs the job with the chosen candidate (`POST /api/jobs/:id/choose-candidate`).
- Score fixes: `master.m3u8`/`playlist.m3u8` +5 over chunklists; `.ts`/`.m4s` segments ignored; `.m3u8` in query only stays 30.

### P3.4 DOM coverage: AMP, Sourcepoint, canonical (#56, #61)
- AMP selectors: `amp-youtube[data-videoid]`, `amp-video source[src], amp-video[src]`, `amp-iframe[src]`, `amp-facebook[data-href]`, `amp-twitter[data-tweetid]`, `amp-vimeo[data-videoid]`, `amp-dailymotion[data-videoid]`.
- If the page has `<link rel="canonical">` different from the current URL (after normalization) and nothing decisive is found by 10 s, navigate to the canonical and continue.
- Consent: add Sourcepoint (`button[title="Accept all"], .sp_choice_type_11`, inside `iframe[id^="sp_message_iframe"]` — iterate frames via `page.frames()` and evaluate in each), Cookiebot, Quantcast `.qc-cmp2-summary-buttons button[mode="primary"]`, Greek text `ΑΠΟΔΕΧΟΜΑΙ|ΑΠΟΔΟΧΗ ΟΛΩΝ|ΣΥΜΦΩΝΩ|ΟΚ`. Run the consent script in all frames, twice (0.5 s and 3 s).
- Lazy scroll: scroll to each `iframe, video, [class*=player]` element (`scrollIntoView`) rather than fixed steps; trigger `play()` on `video` after each.

### P3.5 Cookie jars (#65 x.com, login-walled sites)
- Table `cookie_jars(domain, netscape_txt ENCRYPTED via SecretStore, updated_at, updated_by)`. Admin → Cookies: upload a Netscape `cookies.txt` exported from a browser logged into the station's X/Facebook/Instagram account; test button runs `yt-dlp --cookies <jar> --dump-json <sample url>`.
- Downloader passes `--cookies <temp file with 0600-equivalent ACL>` when the URL's registrable domain has a jar; temp file deleted after the run.
- yt-dlp `LOGIN_REQUIRED` on a domain without a jar → REQUIRES_REVIEW hint "Upload cookies for {domain}".
- Document the operational note: X cookies expire in ~months; the admin health card shows "cookie jar for x.com last validated N days ago" and a nightly `--dump-json` validation against a fixed public post marks it stale.

### P3.6 Site adapters as data (zero-code tuning)
- `data/adapters.json` (shipped defaults, editable in Admin → Extraction): list of `{domain_glob, steps:[{kind:"css", selector, attr, template} | {kind:"regex", source:"html", pattern, template} | {kind:"click", selector}]}` evaluated by the sniffer before the generic DOM scan. Seed with today's hard-coded handlers (Glomex integration/playlist ids, JWPlayer media id, ERT `dt-uni-vod.php?f=` → `mediastream.ert.gr/vodedge/_definst_/mp4:dvrorigin/{f}/playlist.m3u8`, Facebook plugin href, WP Rocket `data-src`) so they remain in code as **defaults** but can be overridden without a release.
- Admin "Test URL" button runs the full router on a URL in dry-run mode and shows every candidate with scores and timings.

### P3.7 Sniffer unit tests
- HTML fixtures under `crates/omni-browser/tests/fixtures/*.html` (saved DOMs of protothema, gazzetta, ertnews, iefimerida-amp, news247 listicle, in.gr) served by a local `axum` test server; the sniffer must find the expected candidates within 8 s each. These run in `cargo test` when a Chrome/Edge binary is present, else are skipped with a warning (not silently).

**Phase 3 acceptance:** corpus (corrected, 64 valid targets) extraction ≥ 63/64; in.gr and iefimerida-amp pass; x.com passes with a jar; average sniff time ≤ 12 s; no orphan browser processes after 100 sniffs; memory stable (< 1.5 GB browser RSS).

---

## 7. Phase 4 — Email (Graph OAuth2), deterministic parser, LLM assist (est. 5 days)

### P4.1 Mail source abstraction
```rust
#[async_trait] pub trait MailSource: Send + Sync {
    async fn fetch_unprocessed(&self, limit: usize) -> Result<Vec<InboundMail>>;
    async fn mark_processed(&self, id: &str, outcome: Outcome) -> Result<()>;   // isRead=true + move to "Omni/Processed" or "Omni/Failed"
    async fn download_attachment(&self, mail_id: &str, att_id: &str, dest: &Path) -> Result<PathBuf>;
    async fn reply(&self, mail_id: &str, html: &str, text: &str) -> Result<()>;
    async fn health(&self) -> MailHealth;
}
pub struct InboundMail { id, internet_message_id, from_address, from_name, to, cc, subject, received_at, body_text, body_html: Option<String>, attachments: Vec<AttachmentMeta> }
```
Implementations: `GraphMailSource` (primary), `ImapMailSource` (kept for on-prem/test; basic auth only; not default).

### P4.2 Microsoft Graph client (E-01)
- Config `mail.provider="graph"`, `mail.graph.tenant_id`, `mail.graph.client_id`, `mail.graph.mailbox="ingest@example.gr"`, `mail.poll_interval_secs=30`, `mail.processed_folder="Omni/Processed"`, `mail.failed_folder="Omni/Failed"`. Secret `graph.client_secret` in SecretStore (certificate auth optional later).
- Token: `POST https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token` (`grant_type=client_credentials`, `scope=https://graph.microsoft.com/.default`), cached until `expires_in - 120 s`, refreshed on 401.
- Fetch: `GET /v1.0/users/{mailbox}/mailFolders/Inbox/messages?$filter=isRead eq false&$orderby=receivedDateTime asc&$top=20&$select=id,internetMessageId,subject,from,toRecipients,ccRecipients,receivedDateTime,body,hasAttachments` with header `Prefer: outlook.body-content-type="text"` (Graph converts HTML → text; fixes E-06). Attachments: `GET /messages/{id}/attachments?$select=id,name,contentType,size` and `GET /messages/{id}/attachments/{att}/$value` for large files (streamed to disk).
- Mark processed: `PATCH /messages/{id} {"isRead":true}` then `POST /messages/{id}/move {"destinationId":"<folder id>"}` (folders created on first run via `POST /mailFolders`). Failed → `Omni/Failed` and **left unread** so a human sees it (fixes E-02).
- Reply: `POST /messages/{id}/reply {"message":{"body":{"contentType":"HTML","content":…}}}` (Mail.Send).
- Required app permissions (document in `docs/office365-setup.md` with screenshots-free step list): `Mail.ReadWrite`, `Mail.Send` (application). Restrict with an Exchange Online application access policy so the app can only touch the ingest mailbox: `New-ApplicationAccessPolicy -AppId <id> -PolicyScopeGroupId <mail-enabled security group containing the ingest mailbox> -AccessRight RestrictAccess`. Admin → Mail card shows: token OK, last poll, last error, mailbox display name, unread count, folder ids.
- Rate limiting/backoff: honor `Retry-After` on 429; exponential backoff on 5xx up to 10 min; health degrades to "Degraded" after 3 consecutive failures and "Down" after 10 min, visible in the MCR status bar (fixes W-09).
- Idempotency (E-07): table `processed_mail(internet_message_id PRIMARY KEY, graph_id, processed_at, outcome, jobs_json)`. Skip already-processed ids even if the server shows them unread again.
- Concurrency (E-05): the poll loop is fully async (reqwest); no `spawn_blocking`, no nested `block_on`.

### P4.3 Deterministic parser (`omni_email::parser`)
Input: `InboundMail` + journalist roster. Output: `ParsedEmail { journalist: Resolved{surname, how}, sections: Vec<Section{index_str, title, jobs: Vec<ParsedJob{url, tier, keyword, confidence, marker}>}>, ignored_urls, warnings }`.

Algorithm:
1. **Decontaminate** (existing `decontaminate_email_body`) + strip quoted replies (`^>`, "From:/Από:" headers below a `-----Original Message-----`/`Στις … έγραψε:` line) + strip signatures after `-- ` or the roster's known signature lines.
2. **Journalist resolution**, first match wins, `how` recorded:
   a. Body override: `(?i)ΣΤΟ\s+ΟΝΟΜΑ\s+(ΤΗΣ|ΤΟΥ)\s+([\p{Greek}A-Za-z]+)` → alias lookup; `ΣΤΟ ΟΝΟΜΑ ΜΟΥ` → sender.
   b. Subject patterns: `ΘΕΜΑΤΑ\s+(\S+)`, `ΕΠΙΚΑΙΡΟΤΗΤΑ\s+(.+)`, `ΓΙΑ\s+(ΜΟΝΤΑΖ\s+)?(\S+)` → alias lookup on each token.
   c. Sender address → `journalists.emails`.
   d. Else `MCR`, warning `JOURNALIST_UNRESOLVED` (LLM assist may run, P4.4).
   Alias lookup: Greek-insensitive normalization (uppercase, strip tonos/dialytika via NFD + remove combining marks, final sigma → Σ), match against `journalists.aliases` (new column, JSON array: e.g. `["ΠΑΠΑΔΑΚΗ","ΠΑΠΑΔΑΚΗΣ","ΑΝΝΑ","ΑΝΝΑΣ","PAPADAKI"]`) and the ELOT-743 transliteration of the surname; prefix match ≥ 4 chars allowed for genitive forms (ΝΙΚΟΛΑΟΥ → ΝΙΚΟΛΑΟΥ).
3. **Section split**: a line matching `^\s*(\d{1,3})\s*[\.\)\-:]?\s*(.*)$` starts section `N` (literal number, never renumbered); the remainder of that line or the next non-URL line is the title. Text before the first numbered line with URLs forms an unnumbered section with index `1` only if no numbered sections exist; otherwise it is attached to section `1` with warning `PREAMBLE_URLS`.
4. **URL extraction** per section: regex for `https?://\S+` (trim trailing `).,;>»`); classify `Tier1` (youtube, youtu.be, instagram.com/(reel|p)/, facebook.com/(reel|watch|share/r|videos|…video.php), fb.watch, tiktok.com, x.com/*/status/, twitter.com/*/status/, vimeo, dailymotion), `Tier2` (configurable news-portal list: neakriti, lifo, protothema, newsit, iefimerida, athletiko, carandmotor, gazzetta, star, ertnews, in.gr, news247, cnn.gr, bbc, cnn), `Locker` (existing interceptor list + `1drv.ms`, `sharepoint.com`, `drive.google.com`, `dropbox.com`), `Image` (`.jpg|.jpeg|.png|.gif|.webp`), `Other`.
   - `ΓΙΑ ΠΛΑΝΑ:`/`GIA PLANA:`/`ΠΛΑΝΑ:`/`VIDEO:` marker: the URL(s) following the marker in that section are the assets; other URLs in the section are context (ignored).
   - Without a marker: if any Tier1 exists → all Tier1 URLs; else all Tier2; else `Other` URLs are queued with `confidence 0.4` (→ REQUIRES_REVIEW unless MCR config `auto_attempt_unknown_domains=true`, default true: the sniffer will try, and only failures reach review).
   - Lockers → `ManualDownload` jobs (P8 may resolve them automatically).
   - Image-only email → no jobs, outcome `PHOTOS_ONLY`, reply template says so.
5. **Index**: single asset → `N`; multiple → `NA, NB, …`.
6. **Keyword**: from the section title: transliterate (ELOT 743, `omni_core::translit`), uppercase, remove stopwords (Greek/English list: ΚΑΙ, ΤΟ, Η, Ο, ΣΤΟ, ΣΤΗ, ΜΕ, ΓΙΑ, ΑΠΟ, ΤΗΣ, ΤΟΥ, ΤΩΝ, ΝΑ, ΠΟΥ, THE, OF, AND, VIDEO, ΒΙΝΤΕΟ, ΠΛΑΝΑ, ΔΕΙΤΕ …), keep the first two tokens ≥ 3 chars, join, `[A-Z0-9]` only, max 20. No title → keyword from the URL slug (last path segment, same processing) → else `ASSET`.
7. **Confidence**: Tier1 with marker 1.0; Tier1 0.95; Tier2 0.8; Other 0.4; any job whose journalist was unresolved gets −0.2. `≥ 0.7` → PENDING else REQUIRES_REVIEW.
8. `warnings` are attached to the reply and the job notes.

Golden tests: `crates/omni-email/tests/fixtures/{name}.eml` + `{name}.expected.json` for at least 15 anonymized real emails covering: numbered multi-section, ΓΙΑ ΠΛΑΝΑ marker, unnumbered single link, journalist override in body, subject with ΘΕΜΑΤΑ ΑΝΝΑΣ, photos-only, WeTransfer, HTML-only body, Windows-1253 body, quoted reply chain, attachment-only, mixed Tier1+Tier2, 10-link listicle. The owner should provide real (anonymized) samples; until then the implementer writes synthetic ones matching the prompt's examples.

### P4.4 LLM assist (opt-in, validated)
- Config `llm.mode = "off" | "assist" | "primary"` (default `assist`), `llm.endpoint`, `llm.model`, `llm.timeout_secs=30`, `llm.max_tokens=400`.
- **assist** runs only when: journalist unresolved **and** the body contains a `ΓΙΑ|ΣΤΟ ΟΝΟΜΑ` phrase; or a section title exists but keyword extraction produced `ASSET`; or `llm.keyword_polish=true`. Prompt asks for `{journalist_surname_latin, keywords: {section_index: keyword}}` only. Use OpenAI-compatible `response_format: {"type":"json_schema", "json_schema": …}` when the server supports it (Ollama ≥ 0.5 / llama.cpp / vLLM do), else `{"type":"json_object"}` + schema in the prompt; `temperature 0`.
- **Validation:** surname must map to a roster alias (else discarded); keyword must match `^[A-Z0-9]{2,20}$` after transliteration; the model never supplies URLs or indices. Any failure → deterministic result stands; event `LLM_ASSIST_SKIPPED`.
- **primary** mode keeps the current full extraction prompt (moved to `assets/prompts/extract_v2.txt`, versioned; config.json no longer stores the prompt text, only `llm.prompt_id`) with the same validation **plus** every returned URL must appear verbatim (after normalization) in the email; the deterministic parser runs anyway and the union is compared; disagreements are logged as `job_events` for tuning.
- Health: `GET {endpoint}/models` ping every 60 s → status bar. LLM down in `assist` mode changes nothing operationally.
- Default model recommendation: a small instruct model with Greek coverage that fits the MCR machine (e.g. `gemma3:4b` or `qwen2.5:3b-instruct`); the admin can change it; the parser test bench (P7.2) shows the assist result side by side so the operator can judge.

### P4.5 Enqueue from email
- For each `ParsedJob`: slug `{index}_{JOURNALIST}_{KEYWORD}`; `repo.enqueue` (dedup outcomes recorded); `email_source = from`, `email_message_id`, notes = subject + warnings; priority = journalist default, +10 if subject contains `ΕΚΤΑΚΤΟ|BREAKING|URGENT`.
- `processed_mail.jobs_json` records the job ids for the summary reply (P5).

### P4.6 Attachments (E-08, low effort, high value)
- Attachments with `contentType` video/* or extension `.mp4 .mov .mxf .mts .m4v .avi .mkv` and size ≤ `mail.max_attachment_mb=2048` are downloaded to `temp/jobs/{id}/source.{ext}` and enqueued with `extraction_method=attachment`, URL `attachment://{message_id}/{att_id}` (normalized unique), keyword from filename. The pipeline skips EXTRACT/DOWNLOAD for `attachment://` sources. Outlook "large attachment" OneDrive links are `Locker` URLs (P8 handles `1drv.ms`/SharePoint share links with `?download=1`).

**Phase 4 acceptance:** Graph login and poll work against the station tenant (needs IT app registration; until then, tests use a mock HTTP server with recorded Graph responses); 15 golden fixtures pass; LLM off → identical job set for fixtures that do not need assist; failed processing leaves the mail unread in `Omni/Failed`; duplicate delivery of the same message creates no duplicate jobs.

---

## 8. Phase 5 — Notifications (est. 2 days)

### P5.1 Outbox (`omni-notify`)
Table `notifications(id, kind, target, payload_json, attempts, next_attempt_at, sent_at, last_error)`. Worker sends with backoff (30 s, 2 m, 10 m, 1 h, give up after 24 h). Kinds: `mail_reply`, `mail_new`, `teams`.

### P5.2 Email replies to journalists (via Graph, from the ingest mailbox)
- Trigger A (immediate): a job from this email reaches REQUIRES_REVIEW/MANUAL_DOWNLOAD/FAILED → reply "needs attention" listing the link, reason (Greek text per `error_code`), and what MCR will do.
- Trigger B (summary): when every job from the email is terminal → one reply with a table: index, keyword, result (`Παραδόθηκε ως 1_PAPADAKI_KNICKS.mxf` / `Χρειάζεται έλεγχο` / `Διπλότυπο του #123`), duration, and the warnings. Config `notify.reply_on_summary=true`, `notify.reply_on_review=true`.
- Templates: `assets/templates/reply_summary.{el,en}.html`, Greek default, plain-text alternative. No external images.
- Loop protection: never reply to auto-replies (`Auto-Submitted`, `X-Auto-Response-Suppress`, subjects starting `Automatic reply`), never reply to the ingest mailbox itself, at most one reply per message per trigger.
- Web-submitted jobs (User panel): if the user has an email, send a new mail on terminal state (`mail_new`).

### P5.3 Teams webhook for MCR
- Config secret `teams.webhook_url` (Workflows/incoming webhook). Adaptive Card on: REQUIRES_REVIEW/MANUAL_DOWNLOAD created (with a deep link to the MCR panel job), mail source Down > 10 min, LLM down (assist mode: info only), low disk, tool update applied/failed, compliance failure. Rate-limited to 1 card per event type per 5 min with counts ("3 more…").

### P5.4 In-panel notifications
- MCR panel: browser `Notification` API (opt-in toggle stored in localStorage) + short sound on new review items; a persistent bell counter.

**Phase 5 acceptance:** send a test email through the real mailbox → summary reply arrives with correct Greek text; Teams card arrives for a forced review; outbox retries when the webhook URL is temporarily wrong; no reply loops with an auto-responder.

---

## 9. Phase 6 — Observability, operations, benchmarks (est. 3 days)

### P6.1 Logging (W-11)
- `tracing-appender` rolling daily files `logs/omni-ingest.log` (keep 30 days), JSON lines with `job_id` span field; console output only in `run` mode. Level per target from config `log.filter`. Redaction layer for secrets (P2.6).
- Per-job log: `job_events` table (already) + last 8 KB of each tool's stderr stored in `job_events` on failure. MCR job drawer shows it.
- Windows Event Log: register source `OmniIngestService` at install; write Info on start/stop, Error on fatal, Warning on health degradation (via the `windows` crate `ReportEventW`).

### P6.2 Health and status
- `GET /api/health` → 200 `{ "status": "ok|degraded|down", "checks": {...} }` for external monitors (no auth, no details beyond status).
- `GET /api/system/status` (real): mail {state,last_poll,last_error}, llm {mode,state,model,last_ping}, browser {available,path,version}, tools {ffmpeg,ffprobe,bmxtranswrap,yt-dlp(+deno)} versions, disk {temp,watchfolder,archive free/total}, queue {pending,running,review,manual, oldest_pending_age}, workers {busy/total}, service {uptime, version, git sha}, last_update, cookie jars status.
- Startup self-test: every tool runs `-version`; watchfolder writable (`.omni-probe.tmp` create/delete); Graph token if configured; browser launch (optional, config). Failures mark health degraded and post to Teams; the daemon still starts (never block MCR on a non-essential failure).

### P6.3 In-house suite expansion (T-03)
`scripts/in_house_test.ps1` additions: synthetic sources 25p, 30p, 50p, 25i-tff, mono, 5.1, no-audio → run the pipeline via a new CLI `omni-ingest pipeline-test <file>` (dry-run into `temp/inhouse`) → compliance gate must pass; frame count and `idet` checks; loudness within ±1 LU of −23; restart-recovery test (start daemon, enqueue a local file, kill the process during transcode, restart, assert completion); auth matrix smoke via HTTP.

### P6.4 End-to-end benchmark runner (T-02, T-04, "see benchmarks")
- Tables `benchmark_targets(id, url, domain, category, friction, expected: pass|expected_fail, note)` seeded from the corpus (replace #61 with a real Guardian article URL; keep the hub URL as `expected_fail` with the note), and `benchmark_runs(id, started_at, finished_at, mode, summary_json)`, `benchmark_results(run_id, target_id, extract_ok, extract_ms, method, candidates, download_ok, download_ms, transcode_ms, verify_ok, total_ms, error_code, stream_url)`.
- Modes: `extract` (URL discovery only, ~10 min for 65) and `full` (download + transcode + verify into `temp/benchmark`, no delivery, no archive; hours). `omni-ingest benchmark run --mode full --targets 1-65 --parallel 2` and Admin → Benchmarks "Run" with live progress via SSE. Never runs while the queue has RUNNING jobs unless `--force` (benchmarks steal CPU from on-air work).
- Report: JSON + Markdown written to `data/benchmarks/{run_id}/` (same shape as today's `benchmark_results.md` plus per-stage timings) and rendered in the admin panel with deltas versus the previous run and a per-domain table. `scripts/benchmark_corpus.ps1` becomes a thin wrapper calling the CLI.
- Acceptance thresholds encoded in the report: extraction ≥ 95 % of non-expected-fail targets; full-mode delivery ≥ 90 %; zero compliance failures; mean extract latency ≤ 12 s.

### P6.5 Metrics
- Optional `GET /metrics` (Prometheus text) behind admin or an allowlist: jobs by status, stage durations histogram, mail poll age, disk, worker busy. Cheap to add with `metrics` + `metrics-exporter-prometheus` crates; keep optional (`metrics.enabled=false` default).

### P6.6 Scheduler
- Replace the hourly `if hour == "03"` loop with a small scheduler (`tokio` interval every minute checking `next_run` times persisted in `scheduled_tasks`): yt-dlp update (03:00 ±20 min), adblock update (03:30), archive/temp retention (04:00), DB purge of COMPLETED jobs older than `retention.jobs_days=90` (keep events), cookie jar validation (05:00), VACUUM monthly. Each task records last run/outcome; Admin → Maintenance shows them with "Run now".

**Phase 6 acceptance:** logs rotate; Event Log entries visible; status endpoint reflects a stopped Ollama and a wrong Graph secret within 60 s; benchmark run from the admin panel produces the report; in-house suite includes the new checks and passes.

---

## 10. Phase 7 — Web panels (est. 4 days)

All pages: embedded assets (P2.5), `app.js` helper with `api(method, path, body)` (adds CSRF header, handles 401 → `/login`, 429 message), `esc()`, SSE client with reconnect and a "Connected/Reconnecting" pill, structured SSE events applied to the DOM in place (no full refetch), Greek/English toggle (small i18n dictionary in `app.js`, default Greek, persisted in localStorage), keyboard focus states, responsive down to 1280 px (MCR wall monitors) and usable on a tablet.

### P7.1 MCR panel (`/mcr`)
Tabs:
1. **Live Queue** — cards for PENDING/RUNNING: id, slug (+ suffix if any), journalist badge, stage chip (Extract/Download/Probe/Transcode/Rewrap/Verify/Deliver), progress bar, speed/ETA, elapsed per stage, extraction method, "silent audio" badge, source duration; actions: Cancel (RUNNING → kills the stage, CANCELLED), Priority bump, Details drawer (events log, stderr tail, candidates, compliance report, source archive path, delivered path with "copy path" button).
2. **Action Required** — REQUIRES_REVIEW / MANUAL_DOWNLOAD / FAILED cards with: reason text from `error_code` (Greek), hint, original email subject/sender, "Open link" (new tab), **candidate picker** when `candidates_json` has alternatives (thumbnail-less list: kind, host, duration, "in article" mark) → "Use this", override URL → "Retry with URL", "Retry", "Discard", "Mark as done manually" (operator dropped the file in Dalet themselves → COMPLETED_MANUAL, reply email still sent), and for `MANUAL_DOWNLOAD` a "Upload file" button that accepts a local file (multipart, ≤ `web.max_upload_mb=4096`) and continues the pipeline from PROBE.
3. **Archive** — server-side paging (`GET /api/jobs?status=…&journalist=…&q=…&page=&per_page=50`), real timestamps, final filename, delivered folder, duration, loudness, method, "Re-deliver" (copies the archived MXF again with a new suffix if the source MXF is still in `archive/`; else re-runs the pipeline from the archived source), CSV export.
4. **Journalists** — table with surname, full name, emails, Greek aliases (editable chips), default priority, Teams/notify preferences; inline edit; validation.
5. **Quick Queue** — URL, journalist (datalist from roster), keyword (auto-suggested from URL slug on blur), index, priority, notes; paste-multiple-URLs mode (one per line) creating `1A/1B…`.
Header status pills read from the real status endpoint: Mail, LLM, Browser, Disk (temp + watchfolder), Workers busy/total; clicking a pill shows the last error. Bell + sound (P5.4). "IT Admin" link only shown to admins.

### P7.2 Admin panel (`/admin`)
Sections (left nav):
1. **Health** — status cards (P6.2), scheduler tasks with last outcome and "Run now", startup self-test results, service uptime, version.
2. **Configuration** — form bound to config v2 (P11) with validation and "Test" buttons: watchfolder (writes a probe file), mail (Graph token + list 1 message), LLM (ping + a 1-line completion), Teams (send test card), browser (launch + navigate to `about:blank`). Save writes config.json atomically and hot-reloads what can be hot-reloaded (allowlist, notify, LLM, poll interval); shows "restart required" for the rest (ports, paths, TLS).
3. **Secrets** — set/replace Graph client secret, Teams webhook; shows only `set / not set / last changed`.
4. **Cookies** — per-domain jar upload/test/delete (P3.5), staleness.
5. **Extraction** — adapters editor (P3.6), domain routing stats (P3.1), "Test URL" dry run with candidate list and timings.
6. **Users** — list, create, reset password, disable/enable, delete (not self), role change, force logout; password policy (≥ 10 chars).
7. **Tools** — versions, yt-dlp channel, "Check for update" (shows remote version + hash), "Apply staged update", "Rollback", Deno presence, adblock stats and "Update lists".
8. **Maintenance** — retention settings (archive days, jobs days, temp), purge/vacuum with confirmation, DB backup download, DB size.
9. **Audit & Logs** — paged audit log with filters (level, category, text, date), download today's log file, Event Log pointer.
10. **Benchmarks** — runs list, start run (mode, range, parallel), live progress, report view with per-domain and per-target tables, deltas vs previous, download JSON/MD.
11. **Parser test bench** — paste raw email text (or pick a message id from the mailbox) → shows parsed sections/jobs/journalist/warnings from the deterministic parser and, side by side, the LLM assist result; "Enqueue these" button. This is the tool that makes prompt/roster tuning a no-code operation.

### P7.3 User panel (`/user`)
- Submit form (URL, keyword with auto-suggest, notes, priority, "multiple URLs" mode), my jobs with real state/stage/timestamps and the delivered filename, cancel own PENDING job, change own password, notification preference (email me on completion).

### P7.4 Login and first-run
- Login with lockout messaging; "MCR desk" link shown only when the client IP is on the allowlist (server injects a flag). `/setup` first-run page (loopback only, until an admin exists): create admin, set watchfolder, optional Graph settings; then redirects to `/admin`.

### P7.5 API changes summary (all JSON; errors `{ "error": { "code": "...", "message": "..." } }`)
- `GET /api/jobs?status&journalist&q&page&per_page` → `{ items, page, per_page, total }`; `GET /api/jobs/:id` → full job incl. events, candidates, compliance; `POST /api/jobs` (single or `urls[]`); `POST /api/jobs/:id/{retry,override,discard,cancel,priority,choose-candidate,mark-manual-done}`; `POST /api/jobs/:id/upload` (multipart); `GET /api/jobs/:id/events`.
- `GET/PUT /api/config`, `GET/PUT /api/secrets/:key`, `GET/PUT/DELETE /api/cookies/:domain`, `POST /api/cookies/:domain/test`, `GET/PUT /api/adapters`, `POST /api/extract/test`, `GET /api/domain-stats`.
- `GET/POST/PATCH/DELETE /api/admin/users[/ :id]`, `POST /api/admin/users/:id/{password,disable,enable,logout-all}`.
- `GET /api/tools`, `POST /api/tools/ytdl/{check,stage,apply,rollback}`, `POST /api/tools/adblock/update`.
- `GET /api/scheduler`, `POST /api/scheduler/:task/run`.
- `GET /api/audit?level&category&q&page`, `GET /api/logs/today`.
- `GET /api/benchmarks[/ :run]`, `POST /api/benchmarks/run`, `GET /api/benchmarks/:run/report.{json,md}`.
- `POST /api/parser/test`.
- SSE `GET /api/events` emits `job`, `status`, `benchmark`, `notice` typed events.

**Phase 7 acceptance:** every panel works offline (no external requests); MCR operator can resolve a review item in ≤ 3 clicks; admin can change the watchfolder and mail settings without editing files; parser bench reproduces golden fixtures.

---

## 11. Phase 8 — File-locker resolver (est. 2.5 days, feature-flagged, last)

Owner's concern: maintenance. Design accordingly.

- Feature flag `lockers.auto_resolve=false` by default; enabling it per domain (`lockers.domains=["wetransfer.com","we.tl","transfernow.net","myairbridge.com","filemail.com","1drv.ms","sharepoint.com","drive.google.com","dropbox.com"]`).
- **Deterministic first, where the vendor gives a stable path:** Dropbox (`?dl=1`), Google Drive (`https://drive.google.com/uc?export=download&id=…` + confirm token handling), OneDrive/SharePoint share links (`?download=1`), Filemail direct links. These are plain HTTP downloads through reqwest with size/type checks. yt-dlp is **not** used here.
- **Generic browser capture for the rest (WeTransfer, TransferNow, MyAirBridge):** open the link in the browser pool, dismiss consent, click the first visible control whose text/aria-label matches `Download|Λήψη|Κατέβασμα|Get your files|Download all|I agree` (up to 3 rounds, 4 s apart), enable `Browser.setDownloadBehavior {behavior: "allowAndName", downloadPath: temp/jobs/{id}/locker, eventsEnabled: true}`, wait on `Browser.downloadProgress` until `completed` (timeout `lockers.timeout_secs=1800`, progress reported to the job). Archives (`.zip`) are extracted with the `zip` crate; if the archive contains multiple videos, each becomes a sub-job (`{index}A/B/…`); non-video content → REQUIRES_REVIEW with the file list.
- **Graceful degradation:** any failure (no button found, download did not start in 60 s, unexpected file type, virus-scan interstitial) → the job stays `MANUAL_DOWNLOAD` exactly as today, with the screenshot of the last page state attached to the job (`temp/jobs/{id}/locker/last.png`, shown in the MCR drawer) so the operator sees why. No retries beyond one.
- No vision model, no Computer-Use agent: delete `ComputerUseAgentPlaceholder` and the `agent.rs` trait; keep a minimal `LockerResolver` trait with the two implementations above.
- Maintenance expectation to state in the README: vendor UI changes break the generic capture perhaps once or twice a year; when that happens the job simply falls back to manual, and the fix is usually a one-line button-text pattern in config (`lockers.button_patterns`), editable in the admin panel without a release.
- Tests: local axum page with a fake "Download" button serving a file → resolver captures it; page without a button → falls back with a screenshot.

**Phase 8 acceptance:** a real WeTransfer link resolves end to end to an MXF with the flag on; with the flag off, behaviour is unchanged; failure path leaves a screenshot and a MANUAL_DOWNLOAD job.

---

## 12. Phase 9 — Release, deployment, docs (est. 1.5 days)

- `cargo build --release` with `[profile.release] lto="fat" codegen-units=1 strip=true panic="abort"` (measure size and startup).
- Build script `scripts/package.ps1` → `dist/OmniDownloader-{version}/` with `omni-ingest.exe`, `bin/` (ffmpeg, ffprobe, bmxtranswrap, yt-dlp, deno if applicable, mxf2raw optional), `assets/prompts`, `data/adapters.json`, `README.md`, `docs/`. Version from `Cargo.toml` + git sha embedded via `build.rs` (`vergen` or env var).
- Docs (`docs/`): `install.md` (service account, share permissions, firewall port), `office365-setup.md` (app registration, permissions, access policy, secret rotation), `operations.md` (runbook: common review reasons and fixes, cookie jars, updating tools, rollback, where logs are), `architecture.md` (this plan's section 2 + state machine diagram), `benchmarks.md`.
- Update `AGENTS.md` and `SKILL.md` to reflect the new invariants (state machine, compliance gate, allowlist, secrets store, path anchoring) and the expanded verification checklist.
- **Staged rollout:** (1) deploy on a staging PC with `delivery.watchfolder` pointed at a test folder and the real mailbox in **read-only shadow mode** (`mail.shadow=true`: process and log, do not mark read, do not reply) for 5 working days; compare parsed jobs against what MCR did manually; (2) switch the mailbox to live; (3) point the watchfolder at Dalet; keep the previous binary and DB backup for rollback (`omni-ingest service stop`, restore `omni.db.bak`, previous exe, start).

---

## 13. Config v2 (config.json)

```jsonc
{
  "config_version": 2,
  "paths": { "database": "data/omni.db", "temp": "temp", "bin": "bin", "logs": "logs", "archive": "archive" },
  "web": { "host": "0.0.0.0", "port": 8080, "tls": { "cert_path": null, "key_path": null }, "max_upload_mb": 4096 },
  "security": { "mcr_open_networks": ["127.0.0.1/32"], "trust_proxy_header": false, "trusted_proxies": [],
                 "session_hours_user": 12, "session_hours_admin": 8, "session_days_mcr": 30, "login_rate_limit_per_5min": 5 },
  "pipeline": { "max_concurrent_jobs": 2, "max_concurrent_sniffs": 1, "extract_timeout_secs": 90, "download_timeout_secs": 1800,
                "transcode_timeout_factor": 6.0, "rewrap_timeout_secs": 600, "deliver_timeout_secs": 900,
                "max_source_duration_secs": 5400, "max_attempts": 3, "dedup_window_hours": 24, "min_free_gb": 20 },
  "download": { "insecure_tls": false, "max_height": 1080, "concurrent_fragments": 4 },
  "audio": { "loudnorm_enabled": true, "target_lufs": -23.0, "true_peak": -1.0, "lra": 7.0 },
  "verify": { "bmx_check": false },
  "delivery": { "watchfolder": "\\\\dalet\\ingest", "layout": "per_journalist", "start_timecode": null },
  "archive": { "enabled": true, "retention_days": 14 },
  "retention": { "jobs_days": 90, "logs_days": 30, "temp_orphans_hours": 24 },
  "mail": { "provider": "graph", "poll_interval_secs": 30, "shadow": false, "max_attachment_mb": 2048,
            "graph": { "tenant_id": "", "client_id": "", "mailbox": "ingest@example.gr", "processed_folder": "Omni/Processed", "failed_folder": "Omni/Failed" },
            "imap": { "server": "", "port": 993, "username": "" } },
  "parser": { "tier2_domains": ["neakriti.gr", "lifo.gr", "..."], "auto_attempt_unknown_domains": true, "urgent_keywords": ["ΕΚΤΑΚΤΟ", "BREAKING", "URGENT"] },
  "llm": { "mode": "assist", "endpoint": "http://localhost:11434/v1", "model": "gemma3:4b", "timeout_secs": 30, "keyword_polish": false, "prompt_id": "extract_v2" },
  "notify": { "reply_on_review": true, "reply_on_summary": true, "teams_enabled": false, "language": "el" },
  "browser": { "executable": null, "max_parallel_pages": 2, "sniff_timeout_secs": 35, "max_pages_per_instance": 40 },
  "adblock": { "enabled": true, "hagezi": true, "greek": true },
  "lockers": { "auto_resolve": false, "domains": [], "timeout_secs": 1800, "button_patterns": ["Download", "Λήψη", "Κατέβασμα", "Get your files", "Download all"] },
  "tools": { "ytdl_channel": "stable", "auto_update": true, "ffmpeg_path": null, "ffprobe_path": null, "bmxtranswrap_path": null, "ytdl_path": null, "deno_path": null },
  "log": { "filter": "info,omni_browser=info", "json": false },
  "metrics": { "enabled": false }
}
```

Migration from v1: `AppConfig::load` detects a missing `config_version`, maps old fields, moves `email_password` into the SecretStore, writes `config.v1.bak.json`, and saves v2.

---

## 14. Testing strategy summary

| Layer | What | Where |
|---|---|---|
| Unit | url normalizer, translit, parser sections/tiers/keywords, transcoder plan matrix, verifier checks, error classification, adblock, scoring, auth extractors, CSRF | each crate |
| Golden | 15+ email fixtures → expected jobs | `omni-email/tests/fixtures` |
| Integration | repository state machine (lease/heartbeat/reap/recover), enqueue dedup, migrations from v1 DB, API auth matrix, XSS regression, Graph client against a mock server, notification outbox retries | `*/tests` |
| Browser | sniffer against local HTML fixtures (skipped with warning if no Chrome/Edge) | `omni-browser/tests` |
| System | `in_house_test.ps1`: tools, synthetic sources ×7 → compliance + frame/loudness checks, restart-recovery, daemon HTTP smoke | `scripts/` |
| Benchmark | `omni-ingest benchmark run` extract + full modes, thresholds in report | admin panel / CLI |
| Soak | 200 synthetic jobs from a local HTTP server, 2 workers, 4 h: no leaks (process count, handles, RSS), all delivered | `scripts/soak_test.ps1` (new) |

---

## 15. Performance targets (reference: the current MCR machine)

| Metric | Target |
|---|---|
| Time from email arrival to job enqueued | ≤ 60 s (poll 30 s + parse) |
| 2-minute 1080p YouTube clip, email → watchfolder | ≤ 4 min |
| Transcode speed (MPEG-2 422 50 Mbps, 1080i) | ≥ 1.5× realtime on 8 cores; measure and record in benchmarks |
| Sniff latency (news portals) | mean ≤ 12 s, p95 ≤ 30 s |
| Daemon RSS (idle, browser closed) | ≤ 150 MB; browser ≤ 1.5 GB while sniffing |
| Panel refresh under 500 jobs in the archive | ≤ 200 ms per page request |

If transcode throughput is the bottleneck, the only safe knobs are `-threads`, `-trellis 0` (already), and running 2 transcodes in parallel; never change GOP, bitrate, or scan type.

---

## 16. Suggested order and effort

| Phase | Days | Depends on |
|---|---|---|
| P0 Safety net | 1.5 | — |
| P1 Queue & pipeline | 5 | P0 |
| P2 Security | 3 | P0 (P2.5 assets can run in parallel with P1) |
| P3 Extraction | 4 | P1 |
| P4 Email + parser + LLM | 5 | P1, P2.6 |
| P5 Notifications | 2 | P4 |
| P6 Observability + benchmarks | 3 | P1, P3 |
| P7 Panels | 4 | P2, P6 (API) |
| P8 Lockers | 2.5 | P3 |
| P9 Release | 1.5 | all |
| **Total** | **≈ 31.5 engineering days** | |

---

## 17. Assumptions and open items (state these in the first PR)

1. IT can register the Azure app and grant `Mail.ReadWrite` + `Mail.Send` application permissions with an application access policy scoped to the ingest mailbox. Until then, Graph code is tested against a mock.
2. Video attachments in emails should be ingested automatically (P4.6). Assumed yes.
3. Per-journalist folder name = the roster surname (e.g. `PAPADAKI`); `MCR` for unresolved. Dalet watchfolder rules must be updated by MCR to watch subfolders (or `delivery.layout=flat` if Dalet cannot).
4. The station has (or will create) an X account whose cookies can be exported for the cookie jar.
5. Loudness target −23 LUFS / −1 dBTP per EBU R128 for broadcast programme audio. If the station's playout applies its own normalization, set `audio.loudnorm_enabled=false`.
6. Start timecode is left at bmxtranswrap's default unless MCR requests `10:00:00:00`.
7. Corpus #61 replaced by a real Guardian article; the hub URL kept as `expected_fail`.
8. Reference hardware for performance targets is the current MCR workstation; numbers are recorded in the first benchmark run and used as the baseline.
