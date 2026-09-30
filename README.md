# OmniDownloader

Broadcast-grade media ingest daemon for a television newsroom. Emails and web
links go in; Sony XDCAM HD422 1080i50 MXF files land atomically in the
broadcast ingest watchfolder that playout monitors.

Pure Rust, one `omni-ingest.exe`, runs as a Windows service.

**Author:** Alex Fountas · Master Control engineer

---

## The problem this solves

A journalist emails a numbered rundown: ten stories, each with a link to a
social post, a news portal article, or a video embedded three iframes deep in a
page that only works with cookies accepted. Someone in Master Control then
spends an hour a day downloading those by hand, renaming them to the playout
convention, running them through a transcode preset, and dropping them in a
watchfolder — while the bulletin gets closer.

The manual version fails in expensive ways. A file with the wrong frame rate
plays back with judder on air. A file with two audio channels instead of eight
comes up silent on the channels the gallery expects. A file dropped into the
watchfolder while it is still being written gets ingested half-complete. None of
those are noticed at the desk; they are noticed on air.

So the goal was never "download videos". It was: **produce a file that is
correct by construction, or produce nothing and say why.**

---

## The approach

### 1. Treat the output format as a contract, not a setting

The on-air specification is fixed and every part of it matters. Playout rejects
or mis-plays a file if a single parameter is wrong, and the failure mode is
usually silent.

| | |
|---|---|
| Container | SMPTE RDD9 OP1a MXF (`bmxtranswrap -t rdd9 --tc-rate 25`) |
| Video | MPEG-2 4:2:2 Profile @ Main Level, 1920×1080 |
| Bitrate | 50 Mbps **constant** — `-b:v`, `-minrate` and `-maxrate` all 50M |
| Frame rate | exactly 25 fps — never 29.97 / 30 / 50p / 60 |
| Scan | interlaced, top field first (`+ildct+ilme`, `-top 1`) |
| Chroma / colour | `yuv422p`, Rec.709 |
| GOP | 12, with 2 B-frames |
| Audio | **exactly 8** discrete mono PCM 24-bit 48 kHz tracks (EBU R48): ch 1–2 programme L/R, ch 3–8 silence |
| Loudness | EBU R128, −23 LUFS / −1 dBTP |

Rather than trusting the encoder command to stay right, the format is enforced
twice:

- **Contract tests over the argument builders.** The ffmpeg and bmxtranswrap
  command lines are produced by pure functions, and the tests assert the exact
  arguments. Changing `-g 12` to `-g 15` fails a test by name.
- **A compliance gate before delivery.** The finished MXF is probed and checked
  against the specification — dimensions, pixel format, field order, stream
  count, bitrate mode — and a file that does not match never reaches the
  watchfolder. It goes to review instead.

The second one exists because the first can only prove that the command we
*intended* was issued. Only probing the actual file proves what came out.

### 2. Test the thing that actually breaks, not the thing that is easy to assert

The most instructive defect in this codebase was a filter chain that halved the
frame rate for progressive sources. The output file still reported 25 fps and
still reported top-field-first, because a later `-r 25` duplicated the halved
frames back up. Every obvious check passed. The symptom on air was judder on
every camera pan.

A frame-count assertion cannot catch that. What catches it is counting *unique*
frames:

| | frames | unique frames (`mpdecimate`) |
|---|---|---|
| old chain | 250 | **125** |
| fixed chain | 250 | **250** |

The lesson generalised into a habit: for each fix, ask what observation would
distinguish the fixed system from the broken one, and assert *that* — then
verify the test by breaking the code on purpose and watching it fail. Several
tests in this repository carry a recorded negative control for exactly that
reason. One concurrency test, inverted, reproduces the original defect and
reports it in words: *"8 jobs were RUNNING at once with only 2 worker permits"*.

### 3. Make partial failure the normal case

Web video extraction fails constantly and for boring reasons: a video is
geo-blocked, a portal changed its embed, a link is a file-locker page, a site
wants consent before it will load a player. The system is built so that none of
those produce a wrong file or a silent stall:

- Every job has a **lease with a heartbeat**, so a worker that dies has its job
  requeued rather than leaving it stuck in `DOWNLOADING` forever.
- Every external tool runs under a **timeout with a process-tree kill**, so a
  hung ffmpeg cannot hold a worker slot indefinitely.
- Failures are **classified into error codes** with a retry policy per code: a
  transient HTTP error is retried with backoff, a deleted video is not retried
  at all, and a failure worth opening a browser for is routed to the sniffer.
- Anything unresolved becomes a **review item in the MCR panel** with the reason
  attached, where an operator can paste a corrected URL and force the job
  through — rather than a wrong file on air or a job that quietly disappeared.

The pipeline never writes a terminal status itself; the worker holding the lease
does, so two workers cannot race a job into an inconsistent state.

### 4. Never damage what is already on air

Delivery is the last step and the one with the least margin for cleverness:

- write to `.{name}.mxf.tmp`, fsync, verify the size, then **atomically rename**;
- **never overwrite** an existing file in the watchfolder — a name collision
  gets a `_N` suffix, because the file already there may be in a rundown;
- the copy path is the *only* path. An earlier version had a same-volume rename
  shortcut, which meant the copy logic — the one that has to be correct on the
  SMB share — only ran when the watchfolder happened to be remote. That is
  backwards: the risky path should be the one that is always exercised.

### 5. Deterministic first, model second

Email parsing is the one place a language model is genuinely useful — rundowns
are written by people, in prose, with inconsistent numbering. But a newsroom
cannot stop working because a model endpoint is down, and a model that
hallucinates a URL is worse than one that finds nothing.

So the parser is deterministic, and the model is an *assist* with strict
validation on its output. The constraint is testable: **the parser must produce
the same job set with the model unreachable.** Language handling is a
configurable concern, not a hardcoded one — the deployed prompt, keyword
transliteration and journalist-name resolution all live in configuration so the
station's own language of preference can be set without touching the code.

### 6. Assume the operator's network is hostile to convenience

The panels are served by the daemon itself and load **nothing from the
internet** — no CDN stylesheet, no icon script, no web font. Two reasons, and
the second is the real one:

- Master Control workstations are often on a restricted or entirely offline
  VLAN, where CDN assets mean the panel renders as unstyled text at exactly the
  moment an operator needs it.
- A CDN `<script>` tag is an unsigned third party with full DOM access, running
  on a machine that can write to the playout share.

This is enforced by a test that reads the shipped assets and fails the build on
any external reference. The same test bans building HTML from strings, because
job URLs and error messages originate in email and were previously injected into
the page with `innerHTML` — a path from "someone mailed the newsroom" to "script
ran in an operator's browser".

### 7. Security sized to the actual threat model

The threat is not a targeted attacker; it is the newsroom LAN, where everyone
can route to everything and a misconfiguration is one keystroke away. So:

- **Every route declares its policy** through a typed extractor, and a test
  walks the route table and fails on any route the access matrix does not
  mention. The defects this replaced were not subtle bugs — they were routes
  nobody had decided a policy for, including one that could repoint the playout
  watchfolder without a session.
- **Convenience is scoped, not global.** The MCR desk skips the login screen,
  but only from the client networks an administrator names in CIDR form — not
  from anywhere that can reach the port.
- **No default credentials.** There is no seeded administrator; first run
  creates one, from the machine itself, and that window closes the moment an
  account exists.
- **Secrets are encrypted at rest** with Windows DPAPI at machine scope, and no
  API ever reads one back — the panel reports only whether a secret is set. An
  admin session that is taken over can overwrite the mailbox password, which is
  loud, but cannot walk away with it.

### 8. Make upgrades and failures recoverable

- **Schema migrations** are ordered and append-only, with a backup taken before
  any pending migration runs. A schema change never means "delete the database",
  which would lose the record of what went to air.
- **Tool updates are staged and verified.** A new yt-dlp is checksum-verified
  against the release manifest, staged, and swapped only once no download is in
  flight — with the previous build kept for one-click rollback, because a
  release that breaks an extractor is an ordinary event.
- **Every path is anchored to the install directory**, never the working
  directory. Under the Windows service the working directory is `System32`,
  which previously meant a second, empty database was created there while the
  operator stared at an empty queue.

---

## Architecture

Seven-crate Cargo workspace, one binary:

| Crate | Responsibility |
|---|---|
| `omni-core` | config, paths, SQLite repository, migrations, auth, secrets, process runner |
| `omni-broadcast` | download → transcode → rewrap → verify → deliver pipeline |
| `omni-browser` | CDP stream sniffer, unified adblocker, browser pool |
| `omni-email` | mail source, MIME decoding, deterministic parser, LLM assist |
| `omni-web` | Axum server, session auth, MCR / admin / user panels, SSE |
| `omni-service` | Windows Service Control Manager integration |
| `omni-cli` | setup wizard, service and secret commands |

Conventions the codebase holds to:

- one queue-insertion function, so no job can be created invisible to
  deduplication;
- one process runner, so no external tool runs without a timeout;
- one API helper in the front-end, so the CSRF header cannot be forgotten per
  call site;
- timestamps are `Option` and render blank when absent, never as "now".

## Building

Requires a stable Rust toolchain (2021 edition) on Windows. No build step
beyond cargo — no Node, no npm, no Python, and no C toolchain requirement.

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

# first run: open http://127.0.0.1:8080/setup from this machine,
# or use the wizard
.\target\release\omni-ingest.exe setup

# locked out? list the accounts, or set a new password (prompted, not an argument)
.\target\release\omni-ingest.exe admin list-users
.\target\release\omni-ingest.exe admin reset-password admin@station.gr

# credentials never go in config.json
.\target\release\omni-ingest.exe secrets set graph.client_secret
.\target\release\omni-ingest.exe secrets list

# what the parser would do with the last 10 mails (read-only: nothing is
# queued, nothing in the mailbox changes); or with saved .eml files
.\target\release\omni-ingest.exe mail-preview --last 10
.\target\release\omni-ingest.exe mail-preview --eml .\sample.eml

# sniff a single URL through the headless browser
.\target\release\omni-ingest.exe browser-test "https://example.com/video/..."

# Windows service (admin terminal)
.\target\release\omni-ingest.exe service install --account "DOMAIN\svc_omni"
.\target\release\omni-ingest.exe service start|stop|status|uninstall
```

Panels: `/mcr` (operators), `/admin`, `/user`, `/login` on port 8080 by default.

If the watchfolder is a network share, install the service under a domain
account. LocalSystem authenticates to SMB as the computer account, which most
file servers refuse — and the failure appears only at delivery, after a correct
file has already been produced.

## Office 365 mailbox (Microsoft Graph)

Exchange Online no longer accepts basic-auth IMAP, so the ingest mailbox is
read through Microsoft Graph with an app registration. Graph is the only mail
source; IMAP support has been removed.

1. **Entra ID → App registrations → New registration.** Single tenant, no
   redirect URI. Note the *Application (client) ID* and *Directory (tenant) ID*.
2. **API permissions → Add → Microsoft Graph → Application permissions:**
   `Mail.Read`. Grant admin consent. That is all the daemon needs: it never
   writes to the mailbox unless you also grant `Mail.ReadWrite` and set
   `graph.write_access` (see below).
3. **Certificates & secrets → New client secret.** Copy the value once.
4. **Limit the app to the ingest mailbox.** Without this, application
   permissions reach every mailbox in the tenant. In Exchange Online
   PowerShell, with a mail-enabled security group that contains only the
   ingest mailbox:

   ```powershell
   New-ApplicationAccessPolicy -AppId <client-id> `
       -PolicyScopeGroupId omni-ingest-scope@example.gr `
       -AccessRight RestrictAccess -Description "OmniDownloader: ingest mailbox only"
   Test-ApplicationAccessPolicy -Identity ingest@example.gr -AppId <client-id>
   ```

5. **Configure the daemon.** In `config.json`, fill `graph.tenant_id`,
   `graph.client_id` and `graph.mailbox`; store the secret in the encrypted
   store, then restart:

   ```powershell
   .\target\release\omni-ingest.exe secrets set graph.client_secret
   ```

   Or use `/setup` in the browser, which stores the secret the same way, then
   **Test mailbox** on `/admin`.

   For a console or development run you can instead set `OMNI_GRAPH_TENANT_ID`,
   `OMNI_GRAPH_CLIENT_ID`, `OMNI_GRAPH_MAILBOX` and `OMNI_GRAPH_CLIENT_SECRET`
   (see `.env.example`); a set variable wins over config.json and the store
   and is never written to either. Do not use them for the service: a
   service's environment is stored in plaintext in the registry.

**How mail is picked up.** Each poll lists the Inbox messages changed since the
last poll (with a 15-minute overlap) and processes those whose Message-ID the
database has not seen. Read state does not matter: someone opening the ingest
mailbox in Outlook hides nothing, and a message is never processed twice. A
mail moved into the Inbox later (rescued from Junk) is picked up, provided it
was received no more than 72 hours before the last poll. On the very first
poll the daemon looks back 24 hours.

With `Mail.Read` alone the mailbox is left exactly as it is. A mail that could
not be processed after three attempts is recorded as failed and appears in the
admin log (`Gave up on email ...`), not in the mailbox.

**Optional: mark and file mail in the mailbox.** Grant `Mail.ReadWrite`
instead and set `"write_access": true` under `graph`: processed mail is then
marked read and moved to `Omni/Processed`, and mail that could not be
processed is moved to `Omni/Failed` and left **unread**. Both folders are
created on first use.

## LLM assist (optional)

The deterministic parser decides every job; the LLM is asked only where it
is unsure (a journalist, a keyword, a group) and its answers are checked
before use. With the LLM off, down or slow, the jobs are the same.

Set it on **Admin → LLM assist**: pick a provider preset (LM Studio,
Ollama, GenieX, llama.cpp/vLLM, OpenAI, Azure OpenAI, Google Gemini,
Anthropic, OpenRouter, Mistral, Groq, or any OpenAI-compatible server),
**Load models**, **Test**, then **Save and apply**; no restart. Keys are
stored encrypted (`llm.api_key`) and a stored key is only ever sent to the
base URL it was saved with.

- **Local** (this machine or the station network): the prompt is sent as is.
- **Online**: https is required, and email addresses and phone numbers in
  the mail are replaced with placeholders; the sender's address is never
  sent. Subject, part of the body, roster surnames and group names are.
- Leave **Disable thinking** on for reasoning models (Gemma 4, Qwen 3.5):
  with thinking, Gemma 4 E4B took 63 s per mail on a Snapdragon X Elite
  instead of 8–29 s, or used its whole token budget and answered nothing.

## Configuration and data that stay out of git

| Path | Why |
|---|---|
| `config.json` | deployment-specific; `config.example.json` ships the shape |
| `data/secrets.bin` | encrypted credential store, tied to the machine |
| `data/journalists.seed.json` | real staff names and work addresses |
| `data/omni.db*` | the live queue and archive |
| `bin/`, `temp/`, `logs/`, `archive/`, `watchfolder/` | runtime state and vendored binaries |

## Licence

Not yet chosen. All rights reserved until one is added.
