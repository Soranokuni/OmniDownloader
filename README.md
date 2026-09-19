# OmniDownloader

Broadcast-grade media ingest daemon for a television newsroom. Emails and web
links go in; Sony XDCAM HD422 1080i50 MXF files land atomically in the Dalet
Galaxy playout watchfolder.

Pure Rust, one `omni-ingest.exe`, runs as a Windows service.

> **Status:** actively being hardened against [`plan.md`](plan.md). Phase 0
> (safety net) is complete. See [`handoffs.md`](handoffs.md) for what has
> landed, what was verified and how, and which known defects are still open.

## What it does

A journalist emails a numbered list of stories with links. The daemon:

1. **Reads the mailbox** and parses the Greek email — sections, numbering,
   `ΓΙΑ ΠΛΑΝΑ:` markers, journalist resolution, keyword generation.
2. **Extracts the video** with yt-dlp, or — for news portals that embed a
   player — with a headless-browser CDP stream sniffer that blocks ads and
   trackers, dismisses consent dialogs, and scores candidate streams.
3. **Transcodes to the on-air format** and re-wraps to SMPTE RDD9 OP1a.
4. **Delivers atomically** into the watchfolder as `{index}_{JOURNALIST}_{KEYWORD}.mxf`.

Anything it cannot resolve becomes a review item in the MCR panel rather than a
silent failure or a wrong file on air.

## The on-air format is not negotiable

Playout rejects or mis-plays a file if a single parameter is wrong. These are
enforced by contract tests over the actual ffmpeg argument vector
(`crates/omni-broadcast/tests/broadcast_compliance_tests.rs`):

| | |
|---|---|
| Container | SMPTE RDD9 OP1a MXF (`bmxtranswrap -t rdd9 --tc-rate 25`) |
| Video | MPEG-2 4:2:2 Profile @ Main Level, 1920×1080 |
| Bitrate | 50 Mbps **constant** (`-b:v`/`-minrate`/`-maxrate` all 50M) |
| Frame rate | exactly 25 fps — never 29.97 / 30 / 50p / 60 |
| Scan | interlaced, top field first (`+ildct+ilme`, `-top 1`) |
| Chroma / colour | `yuv422p`, Rec.709 |
| GOP | 12, with 2 B-frames |
| Audio | **exactly 8** discrete mono PCM 24-bit 48 kHz streams (EBU R48): Ch1/Ch2 programme L/R, Ch3–8 silence |
| Delivery | write `.{slug}.mxf.tmp`, fsync, verify size, atomic rename. Never overwrite an existing file in the watchfolder |

See [`AGENTS.md`](AGENTS.md) section 2 for the full specification and the
reasoning behind each constraint.

## Architecture

Seven-crate Cargo workspace, one binary:

| Crate | Responsibility |
|---|---|
| `omni-core` | config, paths, SQLite repository, migrations, auth, process runner |
| `omni-broadcast` | download → transcode → rewrap → verify → deliver pipeline |
| `omni-browser` | CDP stream sniffer, unified adblocker, browser pool |
| `omni-email` | mail source, Greek MIME decoding, parser, LLM assist |
| `omni-web` | Axum server, session auth, MCR / admin / user panels, SSE |
| `omni-service` | Windows Service Control Manager integration |
| `omni-cli` | setup wizard, service commands |

Design principles the code is being moved toward: acquire worker capacity
before leasing a job; every running job holds a lease with a heartbeat and
every stage has a timeout and a process-tree kill; verify before deliver;
deterministic parsing first with the LLM as optional assist; per-domain routing
learned from history rather than hand-tuned; every path anchored to the install
directory, never the working directory.

## Building

Requires a stable Rust toolchain (2021 edition) on Windows.

```powershell
cargo build --release
cargo test --workspace
```

The four external tools are **not** in this repository (214 MB). Place them in
`bin/` before running the pipeline:

| Tool | Source |
|---|---|
| `ffmpeg.exe`, `ffprobe.exe` | any recent FFmpeg build with `mpeg2video` and `mxf` |
| `bmxtranswrap.exe` | [bmx](https://github.com/bbc/bmx) (BBC) |
| `yt-dlp.exe` | [yt-dlp](https://github.com/yt-dlp/yt-dlp) releases |

## Running

```powershell
# copy the example config and edit it
copy config.example.json config.json

# console mode
.\target\release\omni-ingest.exe run

# sniff a single URL
.\target\release\omni-ingest.exe browser-test "https://example.gr/video/..."

# Windows service (admin terminal)
.\target\release\omni-ingest.exe service install|start|stop|status|uninstall
```

Panels: `/mcr` (operators), `/admin`, `/user`, `/login` on port 8080 by default.

## Configuration and data that stay out of git

| Path | Why |
|---|---|
| `config.json` | holds mailbox credentials — `config.example.json` ships the shape |
| `data/journalists.seed.json` | real staff names and work addresses — see `data/journalists.seed.example.json` |
| `data/omni.db*` | the live queue and archive |
| `bin/`, `temp/`, `logs/`, `archive/`, `watchfolder/` | runtime state and vendored binaries |

## Documentation

- [`plan.md`](plan.md) — the hardening roadmap being implemented, phase by phase,
  with a defect register and acceptance criteria per phase.
- [`AGENTS.md`](AGENTS.md) — invariants. Read before changing anything.
- [`CLAUDE.md`](CLAUDE.md) — orientation: commands, code map, conventions.
- [`handoffs.md`](handoffs.md) — milestone log: what landed and how it was verified.

## Known defects still open

Tracked in `plan.md` section 1. The ones that matter most right now:

- **D-08** — the `tinterlace` filter chain halves the frame rate for
  progressive sources; only a 50p input produces correct 25i today. Isolated in
  `transcoder::VIDEO_FILTER_CHAIN`, fixed in plan P1.5.
- **D-07** — an ffprobe failure is read as "no audio", so a probe error can
  deliver a silent clip instead of going to review.
- **D-01 / D-02** — jobs are leased before a worker permit is acquired, and
  there is no crash recovery for jobs left running.
- **W-01 / W-02 / W-03** — several state-changing routes are unauthenticated and
  the panels render user-supplied strings via `innerHTML`.

## Licence

Not yet chosen. All rights reserved until one is added.
