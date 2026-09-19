---
name: omni-downloader
description: Operational guardrails for the OmniDownloader broadcast ingest engine — Dalet/XDCAM constants, delivery contract, queue/email/browser rules, and the mandatory verification pipeline. Consult before touching transcoding, delivery, extraction, email parsing, the web panels, or the Windows service.
---

# OmniDownloader Skill & Operational Rules

Use this skill when:
- Adding or modifying download, transcode, rewrap, verify, or delivery logic.
- Changing email ingestion (Graph/IMAP), the deterministic parser, LLM assist, or notifications.
- Modifying the headless browser sniffer, adapters, adblock, or cookie jars.
- Touching Axum routes, auth/allowlist, CSRF, or the embedded UI.
- Changing the Windows service lifecycle, scheduler, or updater.

`AGENTS.md` is the full rulebook; `plan.md` is the roadmap. This skill is the checklist.

---

## 1. Non-negotiable broadcast constants (Dalet Galaxy)

| Parameter | Required value | Flags |
|---|---|---|
| Container | SMPTE RDD9 OP1a MXF | `bmxtranswrap.exe -t rdd9 --tc-rate 25 --clip "{slug}"` |
| Video codec | MPEG-2 4:2:2@ML (`mpeg2video`) | `-profile:v 0 -level:v 2` |
| Bitrate | 50 Mbps CBR | `-b:v 50M -minrate 50M -maxrate 50M -bufsize 17825792` |
| Geometry | 1920×1080, 16:9, pillar/letterbox | `scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black -aspect 16:9` |
| Frame rate / scan | 25 fps interlaced, top field first | `-r 25 -flags +ildct+ilme -top 1`; **progressive sources need `fps=50` before `tinterlace=interleave_top`**, 25i TFF sources pass fields through |
| Chroma / colour | 4:2:2, Rec.709 | `-pix_fmt yuv422p -color_primaries bt709 -color_trc bt709 -colorspace bt709` |
| GOP | 12, 2 B-frames | `-g 12 -bf 2` |
| Audio matrix | EBU R48, 8 discrete mono streams: Ch1 L, Ch2 R, Ch3–8 silence | `anullsrc=r=48000:cl=mono`, `-shortest` |
| Audio format | PCM 24-bit 48 kHz | `-c:a pcm_s24le -ar 48000` |
| Loudness | EBU R128 −23 LUFS, −1 dBTP (two-pass `loudnorm`, configurable) | plan P1.5 |
| Delivery | `.{slug}.mxf.tmp` in destination → fsync → size check → atomic rename; never overwrite, suffix `_2` on collision | plan P1.7 |
| Gate | ffprobe compliance report must pass before delivery | plan P1.6 |

If ffprobe cannot read the source, the job goes to review. Never deliver a clip whose audio was assumed.

---

## 2. Directory layout

```
D:\OmniDownloader\
├── bin/                      ffmpeg, ffprobe, bmxtranswrap, yt-dlp (+ deno for yt-dlp, mxf2raw optional)
├── config.json               runtime config (v2 after plan P0; secrets NOT here)
├── data/omni.db              SQLite WAL
├── data/adblock/             cached HaGeZi Light + Greek (kargig) lists
├── data/adapters.json        data-driven site extraction rules (plan P3.6)
├── data/secrets.bin          DPAPI-encrypted secrets (plan P2.6)
├── temp/jobs/{id}/           per-job workspace (plan P1.4)
├── archive/YYYY/MM/DD/       retained sources + sidecar JSON (plan P1.7)
├── logs/                     rolling daily logs (plan P6.1)
├── crates/                   omni-core, -broadcast, -browser, -email, -notify, -web, -service, -cli
├── scripts/                  in_house_test.ps1, benchmark_corpus.ps1, package.ps1, soak_test.ps1
├── plan.md / AGENTS.md / CLAUDE.md
└── target/release/omni-ingest.exe
```

---

## 3. Queue rules

- Permit first, then lease; lease has heartbeat and expiry; orphans requeued at startup.
- Stage timeouts: extract 90 s, download 30 min, transcode 6× duration (min 10 min), rewrap 10 min, deliver 15 min; kill the process tree on timeout.
- Retries only for `NETWORK`, `DELIVERY_FAILED`, `LOW_DISK`, `*_TIMEOUT` on download/extract; everything else → `REQUIRES_REVIEW` with an `error_code`.
- Dedup by normalised URL (active jobs, and completed within 24 h for the same journalist).
- Slug `{index}_{JOURNALIST}_{KEYWORD}`, `[A-Z0-9_]`, ≤ 60 chars.

---

## 4. Email rules

- Transport: Microsoft Graph OAuth2 (client credentials, `Mail.ReadWrite` + `Mail.Send`, mailbox-scoped access policy). IMAP basic auth only for on-prem servers.
- Mark processed only after jobs are persisted; failures stay unread in `Omni/Failed`; idempotent by `internetMessageId`.
- Deterministic parser first (sections, `ΓΙΑ ΠΛΑΝΑ:` marker, Tier1 > Tier2 > other, journalist via body override → subject → sender mapping → `MCR`, ELOT 743 keywords).
- LLM: `assist` mode by default; validated surname/keyword only; never URLs; must work with the LLM down.
- File lockers (WeTransfer, we.tl, TransferNow, MyAirBridge, Filemail, AMNA, OneDrive, Drive, Dropbox) → `MANUAL_DOWNLOAD`, optional flagged resolver with graceful fallback.
- Video attachments ingested directly.
- Replies to journalists and Teams cards go through the notification outbox with retries; never reply to auto-responders.

---

## 5. Browser sniffing rules

- Router: direct platforms → yt-dlp first; news portals → sniffer first; per-domain success stats reorder automatically.
- One pooled browser, incognito context per job, hard kill on shutdown; `--disable-features=IsolateOrigins,site-per-process`; no `--no-sandbox`.
- `Network.setBlockedURLs` with adblock patterns before navigation; consent dismissed in all frames; DOM polled every 1.5 s; AMP and canonical fallbacks.
- Candidates ranked by in-article position, score, then order; short (< 20 s) candidates demoted; all stored on the job for MCR choice.
- Forward Referer, User-Agent, cookies (and per-domain cookie jar) to yt-dlp. Never log cookie values.

---

## 6. Web & security rules

- Roles `admin` / MCR / `user`; MCR panel open only from `security.mcr_open_networks`; admin actions always authenticated.
- CSRF header `X-Omni-Request: 1` + same-host Origin on every state change; `SameSite=Strict` cookie; login rate limit.
- Escape everything rendered; no CDN assets; no default admin; secrets in `SecretStore` only.
- Real health in `/api/system/status`; `/api/health` for monitors.

---

## 7. Mandatory verification pipeline

```powershell
cargo test --workspace
cargo build --release                                   # 0 warnings
powershell -ExecutionPolicy Bypass -File .\scripts\in_house_test.ps1
powershell -ExecutionPolicy Bypass -File .\scripts\benchmark_corpus.ps1 -StartIndex 1 -EndIndex 65   # when extraction code changed
```

Pass criteria: all tests green; compliance gate passes for synthetic 25p/30p/50p/25i sources with correct frame counts, TFF, and −23 LUFS ±1; ≥ 95 % extraction on valid corpus targets with 0 ad streams; no orphaned jobs after a forced kill/restart; no leftover browser or tool processes.
