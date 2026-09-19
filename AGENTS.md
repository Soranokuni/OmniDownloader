# AGENTS.md — OmniDownloader Broadcast Ingest Engine

> [!CRITICAL]
> **READ THIS BEFORE MODIFYING ANY CODE IN THIS REPOSITORY.**
> This daemon feeds media directly into the Dalet Galaxy playout watchfolder of a live television newsroom (Crete TV). A wrong file format, a silent clip, or an overwritten asset goes to air. The rules in section 2 are non-negotiable. Everything else is governed by `plan.md`, which is the authoritative roadmap; this file tells you the invariants, the current state of the code, and how to work in it.

Companion documents:
- `plan.md` — the implementation roadmap (phases P0–P9), defect register with file:line references, schema and config v2, acceptance criteria. **Work in phase order.**
- `CLAUDE.md` — quick orientation for coding agents: commands, code map, conventions.
- `SKILL.md` — the operational skill (broadcast constants, verification pipeline).

---

## 1. Architectural invariants (DO NOT BREAK)

### 1.1 Single-binary Rust daemon (`omni-ingest.exe`)
Seven-crate workspace, one executable, no runtime dependencies other than the tools in `bin/`:

| Crate | Responsibility |
|---|---|
| `crates/omni-core` | `AppConfig`, SQLite repository (WAL, r2d2 pool), Argon2id auth, `DependencyManager`, models. Target additions (plan P0/P1/P2): `paths`, migrations, `process` runner, `urlnorm`, `translit`, `secrets` (DPAPI). |
| `crates/omni-broadcast` | yt-dlp downloader, FFmpeg Sony XDCAM transcode, bmxtranswrap RDD9 rewrap, atomic watchfolder delivery, `BroadcastEngine` pipeline. Target: extraction router, probe, verifier (compliance gate), worker pool. |
| `crates/omni-browser` | chromiumoxide CDP stream sniffer, `UnifiedAdBlocker` (HaGeZi + Greek list), file-locker resolver. |
| `crates/omni-email` | mail source (currently IMAP; target Microsoft Graph OAuth2), Greek MIME decoding, decontamination, volatile-URL interceptor, LLM client. Target: deterministic parser, LLM assist, attachments, replies. |
| `crates/omni-web` | Axum server on port 8080, session auth, routes, SSE, embedded HTML assets via `rust-embed`. |
| `crates/omni-service` | Windows Service Control Manager integration. |
| `crates/omni-cli` | `inquire` setup wizard, service commands. |
| `src/main.rs` | dispatcher: `run`, `run-service`, `setup`, `service`, `browser-test`, `adblock`; contains the daemon supervisor (`run_daemon`). |

- **NEVER** introduce Python scripts, Node.js sidecars, npm packages, or a JavaScript build step. UI assets are static files embedded into the binary. (Deno bundled in `bin/` purely as yt-dlp's JS runtime is the one permitted exception; see plan P1.8.)
- **NEVER** touch legacy folders `IngestGuard/` or `athena_ingest/` if present. Historical reference only.
- **NEVER** read or search inside `target/` (build output, tens of thousands of files).
- New third-party crates must be pure Rust or vendor their C (like `rusqlite` `bundled`), and must be justified in the PR description.

### 1.2 Paths
All paths in config are relative to the **install directory** (directory of the executable), never the process working directory. Under the Windows service the CWD is `System32`. The current code still resolves against CWD (`config.rs:321`, `adblock.rs:127`); plan P0.1 fixes this. Do not add new CWD-relative lookups.

---

## 2. Broadcast compliance rules (DALET GALAXY SPECIFICATION)

Any media placed in the playout watchfolder must match these parameters exactly. Playout rejects or mis-plays files if a single parameter is wrong.

### 2.1 Video: Sony XDCAM HD422 PAL 1080i50
- **Container:** SMPTE RDD9 OP1a MXF, re-wrapped with `bmxtranswrap.exe -t rdd9 --tc-rate 25` (plan adds `--clip "{slug}"`).
- **Codec:** MPEG-2 Video (`mpeg2video`), `-profile:v 0 -level:v 2` (4:2:2 Profile @ Main Level).
- **Bitrate:** constant 50 Mbps: `-b:v 50M -minrate 50M -maxrate 50M -bufsize 17825792`.
- **Frame rate:** exactly 25.0 fps (`-r 25`). **NEVER** 29.97 / 30 / 50p / 60.
- **Scan:** interlaced, top field first: `-flags +ildct+ilme -top 1`. **NEVER** progressive output.
- **Resolution:** 1920×1080 with pillar/letterboxing: `scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black`, `-aspect 16:9`.
- **Chroma:** 4:2:2 (`-pix_fmt yuv422p`, `format=yuv422p`).
- **Colour:** Rec.709 (`-color_primaries bt709 -color_trc bt709 -colorspace bt709`).
- **GOP:** long GOP of 12 with 2 B-frames (`-g 12 -bf 2`).

> [!WARNING]
> **Known defect in the current filter chain (plan D-08 / P1.5).** `tinterlace=mode=interleave_top` halves the frame rate. It is only correct when fed 50 progressive frames per second. The current chain feeds it the source rate, so a 25p source becomes 12.5 fps and is then duplicated by `-r 25`; interlaced sources are re-interlaced. The correct rule is: **progressive source → `fps=50` before `tinterlace`; interlaced 25 TFF source → pass fields through (`scale=…:interl=1`, `setfield=tff`, no tinterlace); other interlaced → `yadif=mode=send_field`, `fps=50`, then `tinterlace`.** Do not "fix" this by changing the output flags above; fix the filter chain and prove it with frame counts and `idet` (plan P6.3).

### 2.2 Audio: EBU R48 8-channel discrete matrix
- **Exactly 8 discrete mono audio streams** in the MXF. Never a 2-channel interleaved stream, never fewer than 8.
- **Format:** `-c:a pcm_s24le -ar 48000` (24-bit, 48 kHz).
- **Mapping:** Ch1 programme left, Ch2 programme right, Ch3–8 silence pads (`anullsrc=r=48000:cl=mono`, `-shortest`).
- **Loudness (plan P1.5, in scope):** programme audio normalised to EBU R128 −23 LUFS, −1 dBTP, two-pass `loudnorm`. Configurable off.
- **Never assume silence on probe failure.** If ffprobe cannot read the source, the job must go to review, not to air (plan D-07).

### 2.3 Atomic watchfolder delivery contract
- Dalet scans the watchfolder every few seconds. **NEVER** write directly to `{slug}.mxf`.
- Always write to the hidden temporary file `.{slug}.mxf.tmp` **in the destination directory**, fsync, verify size, then atomically rename to `{slug}.mxf`.
- **NEVER delete or overwrite a file that already exists in the watchfolder.** On collision, deliver as `{slug}_2.mxf`, `_3`, … (current code overwrites; plan D-05 / P1.7).
- Target layout is `watchfolder/{JOURNALIST}/{slug}.mxf` (plan P1.7); `flat` remains configurable.

### 2.4 Compliance gate (target, plan P1.6)
Before delivery, `verify_mxf` must confirm with ffprobe: 1 video stream mpeg2video 1920×1080 yuv422p 25/1 `field_order=tt` bt709; exactly 8 audio streams each `pcm_s24le` 48000 Hz 1 channel; sane duration and size. A failed report goes to REQUIRES_REVIEW and is never delivered. **Any change to the ffmpeg or bmxtranswrap command line must be validated by this gate and by `scripts/in_house_test.ps1`.**

### 2.5 Slugs
`{index_str}_{JOURNALIST}_{KEYWORD}`, uppercase, `[A-Z0-9_]` only, Greek transliterated (ELOT 743), max 60 chars. Multi-link sections use `1A, 1B, …`. Slugs are filenames in Dalet; treat them as a public contract.

---

## 3. Job queue and pipeline rules

- **Acquire capacity first, then lease.** A job may only move to a running state when a worker permit is already held. (Current code leases before acquiring the semaphore, `src/main.rs:325–337`; plan D-01.)
- **Every running job has a lease with heartbeat**, and every stage has a timeout and a recorded start time. A process that outlives its timeout is killed with its whole process tree (Windows Job Object). Never spawn a tool without `kill_on_drop` and a timeout (plan P0.6, P1.3).
- **Startup must recover orphans**: running jobs owned by this host are requeued (plan P1.1). Never leave a job in a running state without an owner.
- **Per-job temp directory** `temp/jobs/{id}/`; cleanup removes only that directory. Never clean up by filename prefix (plan D-04).
- **Idempotent enqueue** by normalised URL within an active window; no `UNIQUE(url)` constraint on the queue (plan D-10, P1.2).
- **Error codes, not free text**, drive retries and MCR hints (plan P1.9). Do not add new failure paths without an `error_code`.
- **Do not expand the corpus of hard-coded site logic in Rust.** Site-specific extraction rules belong in the data-driven adapters (`data/adapters.json`, plan P3.6); routing is learned per domain (`domain_stats`, plan P3.1).

---

## 4. Email and Greek newsroom processing rules

### 4.1 Transport
- **Target:** Microsoft Graph with OAuth2 client credentials (`Mail.ReadWrite`, `Mail.Send`, application access policy scoped to the ingest mailbox). Plan P4.2.
- **Current:** basic-auth IMAP (`imap` crate, `native-tls`) inside `spawn_blocking`. Basic auth is retired on Exchange Online; this path is kept only as a secondary provider for on-prem servers. Do not build new features on it.
- A message is marked processed **only after** its jobs are persisted. Failures leave it unread in the failed folder (current code marks `\Seen` regardless; plan E-02).
- Idempotency by `internetMessageId` (`processed_mail` table).

### 4.2 Parsing
- **Deterministic first.** Sections, numbering, URL tiers, `ΓΙΑ ΠΛΑΝΑ:` markers, journalist resolution (body override → subject → sender mapping → `MCR`), keyword generation via ELOT 743 transliteration and stopword removal — all in Rust (plan P4.3), covered by golden `.eml` fixtures.
- **LLM is assist only** (`llm.mode=assist` default): it may propose a journalist surname (validated against the roster aliases) and keywords (validated `^[A-Z0-9]{2,20}$`). It never supplies URLs or indices. In `primary` mode every returned URL must exist verbatim in the email. The system must work identically with the LLM unreachable.
- All bodies pass through `decontaminate_email_body()` (iOS/Android signatures, `aka.ms`, `go.microsoft.com` poison links) and quoted-reply stripping before parsing.
- Greek encodings `ISO-8859-7`, `Windows-1253`, `UTF-8` must decode correctly; Graph is asked for `text` bodies.
- Confidence ≥ 0.7 → PENDING, else REQUIRES_REVIEW. Photo-only emails create no jobs.
- Video attachments are ingested directly (`attachment://` sources skip extract/download; plan P4.6).

### 4.3 Volatile file lockers
WeTransfer (`wetransfer.com`, `we.tl`), MyAirBridge, TransferNow, Filemail, AMNA, OneDrive/SharePoint share links, Google Drive, Dropbox: never handed to yt-dlp. They become `MANUAL_DOWNLOAD` jobs with a `MANUAL_` slug prefix. The optional resolver (plan P8) is feature-flagged and must fall back to `MANUAL_DOWNLOAD` with a screenshot on any failure. **The Computer-Use / vision-agent placeholder is deprecated and is to be removed**, not extended.

---

## 5. Headless browser sniffing and adblock

- Used when yt-dlp cannot resolve a page (news portals, embedded players). Router decides order per domain (plan P3.1); direct platforms (YouTube, X, Instagram, Facebook, TikTok, Vimeo, Dailymotion, direct media URLs) go to yt-dlp first.
- **One long-lived browser process (pool), one incognito context per job, hard-killed on shutdown** (plan P3.2). Never launch a browser per job without a timeout and explicit close; never leave `msedge.exe` orphans.
- Launch with `--disable-features=IsolateOrigins,site-per-process` so cross-origin iframe players are visible on the page's CDP session (fixes in.gr class of failures). Do **not** use `--no-sandbox` on Windows.
- Ad and tracker domains are blocked with `Network.setBlockedURLs` **before** navigation, from the `UnifiedAdBlocker` (HaGeZi Light + kargig Greek list, cached in `data/adblock/`, nightly refresh). Zero promotional streams may be captured; the benchmark checks this.
- Consent dialogs (OneTrust, Didomi, Quantcast, Cookiebot, Sourcepoint incl. iframes, Greek `ΣΥΜΦΩΝΩ / ΑΠΟΔΟΧΗ`) are dismissed in all frames; DOM scans are polled every ~1.5 s because redirects recreate execution contexts (`Error -32000`).
- Candidate selection prefers streams inside the article body, then score, then document position; ad-length candidates (< 20 s) are demoted; all candidates are stored on the job so MCR can pick another (plan P3.3).
- **Anti-403:** when delegating a sniffed stream to yt-dlp, always forward Referer, User-Agent and cookies (`BroadcastEngine::process_job_with_context` today; the router's session context after P3).
- Per-domain cookie jars (plan P3.5) are stored encrypted and passed to yt-dlp via a temporary file; never log cookie values.

---

## 6. Web portal and security rules

- Embedded Axum server, default port 8080, session cookie `omni_session` (`HttpOnly; SameSite=Strict`; `Secure` when TLS is on).
- **Roles:** `admin` (everything, always logged in), `open_mcr` / MCR (operate the queue, journalists), `user` (submit and view own jobs).
- **MCR access model (decided):** the MCR panel is reachable **without login only from IPs in `security.mcr_open_networks`**; everyone else logs in. Admin and destructive actions always require a logged-in admin. The old `auth_mode` switch is being replaced by this allowlist (plan P2.1).
- **Every state-changing route requires a principal** (user session or allowlisted MCR IP), the CSRF header `X-Omni-Request: 1`, and a same-host `Origin`/`Referer`. Currently `/api/setup`, journalist edits, job overrides, logs, and the IMAP tester are unauthenticated — treat that as a bug, not a feature (plan W-01/W-02).
- **All user-supplied strings are escaped in the UI** (`textContent`/`esc()`); no `innerHTML` with interpolated data (plan W-03).
- **No external assets** (no CDN Tailwind/Lucide/Google Fonts). Everything is embedded (plan P2.5).
- **No default credentials.** The seeded `admin@newsroom.local / admin123` account is to be removed; first-run setup creates the admin (plan W-04).
- **Secrets never in `config.json`, logs, or git.** They live in the DPAPI-backed `SecretStore` (plan P2.6). Config holds only identifiers.
- Login is rate-limited; Argon2id parameters are explicit (m=64 MiB, t=3, p=1).
- Tool updates (yt-dlp) are verified against the release `SHA2-256SUMS`, staged, applied only when no download is running, and can be rolled back (plan P2.9).

---

## 7. Observability and operations

- Logs: rolling files under `logs/` (30 days) plus Windows Event Log for service start/stop/fatal (plan P6.1). `stdout` alone is not acceptable for a service.
- `GET /api/system/status` must report **real** health (mail, LLM, browser, tools, disk, queue, workers). Hard-coded "Active"/"Ready" strings are a bug (plan W-09).
- Scheduled tasks (updates, retention, cookie validation, vacuum) are persisted with last run/outcome and visible in the admin panel (plan P6.6).
- Benchmarks run **inside the app** (`omni-ingest benchmark run`, Admin → Benchmarks) in `extract` and `full` modes and never while on-air jobs are running unless forced (plan P6.4).

---

## 8. Verification checklist before committing changes

Run all steps; all must pass with 0 errors and 0 warnings.

```powershell
# 1. Workspace unit & integration tests
cargo test --workspace

# 2. Release build (warnings are errors for this project)
cargo build --release

# 3. In-house robustness & broadcast compliance suite
powershell -ExecutionPolicy Bypass -File .\scripts\in_house_test.ps1

# 4. Extraction benchmark (required when touching omni-browser, omni-broadcast::extract/downloader, or adblock)
powershell -ExecutionPolicy Bypass -File .\scripts\benchmark_corpus.ps1 -StartIndex 1 -EndIndex 65
```

Requirements:
- Step 3 must verify all toolchain binaries, produce synthetic media, and prove Sony XDCAM HD422 + EBU R48 compliance via ffprobe (after plan P6.3 also frame count, `idet` TFF, and loudness).
- Step 4 must reach ≥ 95 % extraction on the non-`expected_fail` corpus targets with 0 promotional ad streams captured (current baseline 61/65 = 93.8 %; #61 is an invalid target).
- Every bug fix ships with a regression test that fails before and passes after.
- Commit messages start with the plan item id (e.g. `P1.4: per-job temp workspace`).

---

## 9. Things that look intentional but are known defects

Do not copy these patterns into new code. Full list with file:line in `plan.md` section 1.

- Leasing before acquiring a worker permit; no orphan recovery; no stage timeouts.
- Prefix-based temp cleanup (`"{id}_"` matches other jobs).
- Overwriting existing watchfolder files.
- `Utc::now()` substituted for stored timestamps on read.
- `UNIQUE(url)` on the queue.
- `tinterlace` without `fps=50` for progressive sources.
- Treating ffprobe failure as "no audio".
- Basic-auth IMAP to Office 365; marking mail seen before processing succeeds.
- Deployed `config.json` overriding the few-shot prompt with a one-liner.
- Unauthenticated setup/journalist/job/log routes; `innerHTML` rendering; CDN assets; default admin; plaintext mail password.
- Double-hashed admin password in the CLI wizard.
- CWD-relative paths.
- Tautological compliance tests.
