# CLAUDE.md — OmniDownloader

Broadcast-grade media ingest daemon for the Crete TV newsroom. Emails and web links in, Sony XDCAM HD422 1080i50 MXF files out, delivered atomically into the Dalet Galaxy playout watchfolder. Pure Rust, single `omni-ingest.exe`, Windows service.

**Read in this order:** this file → `AGENTS.md` (invariants, current state, verification) → `plan.md` (the roadmap you are implementing, phase by phase).

## Hard rules (summary; AGENTS.md is authoritative)

1. Never change the on-air format: MPEG-2 422 50 Mbps CBR, 1920×1080, 25 fps interlaced TFF, GOP 12, yuv422p, Rec.709, 8 × mono PCM 24-bit 48 kHz, RDD9 OP1a via bmxtranswrap, atomic `.tmp` → rename delivery, never overwrite a file in the watchfolder.
2. Pure Rust. No Python, Node, npm, or JS build steps. No CDN assets in the UI.
3. Work through `plan.md` in phase order. One commit per work item, message prefixed with the item id (`P1.4: …`). Every bug fix gets a regression test.
4. Secrets never in `config.json`, logs, or git. Never log cookies or tokens.
5. Paths resolve against the executable's directory, never the CWD.
6. Do not read or search inside `target/`.
7. If a broadcast rule is ambiguous, stop and ask. For anything else, choose the option with the least operator maintenance and note the assumption in the PR.

## Commands

```powershell
# build (0 warnings expected)
cargo build --release

# tests
cargo test --workspace
cargo test -p omni-email            # one crate
cargo test -p omni-core test_concurrent_queue_leasing_stress -- --nocapture

# run the daemon in console mode (config.json in repo root)
.\target\release\omni-ingest.exe run
.\target\release\omni-ingest.exe run --config temp\dev_config.json

# sniff a single URL through the headless browser
.\target\release\omni-ingest.exe browser-test "https://www.ertnews.gr/video/..."

# adblock lists
.\target\release\omni-ingest.exe adblock status
.\target\release\omni-ingest.exe adblock update

# service lifecycle (admin terminal)
.\target\release\omni-ingest.exe service install|start|stop|status|uninstall

# in-house compliance suite and 65-URL benchmark
powershell -ExecutionPolicy Bypass -File .\scripts\in_house_test.ps1
powershell -ExecutionPolicy Bypass -File .\scripts\benchmark_corpus.ps1 -StartIndex 1 -EndIndex 65
```

Web UI in dev: `http://127.0.0.1:8080/mcr` (MCR desk), `/admin`, `/user`, `/login`. Port and host are in `config.json`.

## Code map

```
src/main.rs                        CLI dispatcher + run_daemon (web, mail watcher, updater, worker pool)
crates/omni-core/src/
  config.rs                        AppConfig (JSON), DEFAULT_SYSTEM_PROMPT, defaults
  repository.rs                    SQLite schema + all queries (queue, users, sessions, journalists, audit)
  models.rs                        Job, JobStatus, User, UserRole, Journalist, AuditLog
  auth.rs                          Argon2id hash/verify, session token
  dependencies.rs                  tool discovery in bin/, yt-dlp update
crates/omni-broadcast/src/
  pipeline.rs                      BroadcastEngine: download → transcode → rewrap → deliver
  downloader.rs                    yt-dlp runner + progress regex
  transcoder.rs                    ffprobe + ffmpeg XDCAM command (see AGENTS.md 2.1 warning)
  rewrapper.rs                     bmxtranswrap -t rdd9
  delivery.rs                      watchfolder atomic drop, temp cleanup
crates/omni-browser/src/
  sniffer.rs                       CDP network + DOM embed extraction, scoring, consent bypass
  browser.rs                       Edge/Chrome discovery + launch flags
  adblock.rs                       UnifiedAdBlocker (HaGeZi + Greek), CDP block patterns
  agent.rs                         file-locker placeholder (to be removed, plan P8)
crates/omni-email/src/
  watcher.rs                       IMAP poll loop, MIME/charset decode, enqueue
  llm.rs                           OpenAI-compatible chat client, JSON fence stripping
  decontaminate.rs, interceptor.rs signature/poison-link stripping, file-locker URL intercept
crates/omni-web/src/
  server.rs                        router + layers
  routes.rs                        all handlers (pages, auth, jobs, journalists, admin, system, SSE)
  auth.rs                          cookie parsing, session lookup
  assets.rs + assets/*.html        rust-embed static pages (mcr, admin, user, login, setup)
crates/omni-service/src/lib.rs     Windows SCM install/run/stop
crates/omni-cli/src/lib.rs         setup wizard, service subcommands
scripts/                           in_house_test.ps1, benchmark_corpus.ps1
bin/                               ffmpeg.exe, ffprobe.exe, bmxtranswrap.exe, yt-dlp.exe
data/                              omni.db (SQLite WAL), adblock/ cached lists
```

## Conventions

- Rust 2021, `anyhow` for application errors, `thiserror` for typed errors, `tracing` for logs (`info!` with `job_id` where relevant; never `println!` in library crates).
- Async: Tokio multi-thread. Blocking work (IMAP, SQLite via r2d2 is fast enough inline) goes through `spawn_blocking`; never `block_on` inside an async context.
- External tools are spawned with `tokio::process::Command`, `CREATE_NO_WINDOW` on Windows, piped output, and (target) the `omni_core::process` runner with timeout and job-object kill.
- DB access only through `Repository` methods; SQL lives in `repository.rs` (and, after plan P0.2, in ordered migrations). Use `params![]`, never string-formatted SQL.
- Status strings are stable API (`PENDING`, `COMPLETED`, `REQUIRES_REVIEW`, …). Keep `as_str()` values backwards compatible.
- UI: plain HTML/CSS/JS embedded via `rust-embed`; every interpolated value is escaped; API calls go through a single `api()` helper that sets the CSRF header.
- Tests: unit tests next to code, integration tests in `crates/*/tests/`, golden email fixtures in `crates/omni-email/tests/fixtures/`. Tests must not need network access except the explicitly network-marked benchmark suite. Browser tests skip with a visible warning when no Chrome/Edge is installed.
- Windows first: paths may be UNC (`\\server\share`), use `PathBuf`, never string-concatenate separators. PowerShell scripts, not bash, for anything in `scripts/`.

## Gotchas

- `config.json` in the repo root is the live dev config and currently overrides the good few-shot prompt with a one-liner (plan E-03). Do not commit credentials into it.
- `benchmark_results.md/.json` are generated reports; do not hand-edit.
- The in-house suite's synthetic source is 50p, which hides the progressive-source interlacing bug (plan D-08/T-03). Add 25p/30p sources before claiming the transcoder is fixed.
- `cargo test` starts real SQLite files in temp; tests that spawn ffmpeg require `bin/` tools present.
- Running the exe as a service uses `System32` as CWD until plan P0.1 lands; test path changes with `--config` pointing at an absolute path.
- The seeded `admin@newsroom.local / admin123` account exists in any fresh DB until plan P2.4 removes it. Never rely on it in tests you intend to keep.
