# OmniDownloader — Administrator Guide

Installation, configuration, day-to-day maintenance and troubleshooting.
For the MCR desk and the journalist panel, see the [User Guide](USER_GUIDE.md).

---

## 1. Requirements

- Windows 10/11 or Windows Server 2019+, 64-bit.
- Microsoft Edge or Google Chrome (used headless to read news pages).
- Internet access (downloads, nightly yt-dlp / Deno / adblock updates).
- Disk: new jobs stop starting below 5 GB free; plan for tens of GB
  (working files, a 14-day archive of sources, held videos).
- For mail: a Microsoft 365 mailbox and an Entra ID app registration with
  the Microsoft Graph **application** permission `Mail.Read` (or
  `Mail.ReadWrite`, see §5.2).

Nothing else needs installing: a release carries its own tools and the C++
runtime they need.

---

## 2. Installing

### 2.1 From a release package

A release is a folder `OmniIngest-<version>-<date>\` (or its zip). Build one
from a checkout with:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\package.ps1
```

The package contains `omni-ingest.exe`, `bin\` (ffmpeg, ffprobe, bmxtranswrap,
yt-dlp, Deno, the C++ runtime DLLs), `Install.cmd`, `Uninstall.cmd`,
`README.txt` and `SHA256SUMS.txt`.

**New installation**

1. Copy the folder to the PC and double-click **`Install.cmd`** (allow
   administrator rights).
2. Give the Dalet watchfolder when asked: a local folder or `\\server\share`.
3. When it says *OmniDownloader is running*, open
   `http://<this-pc>:8080/admin` and sign in as **`Admin` / `Admin`**.
   This account is created once, on a brand-new installation only.
4. **User accounts → Create user**: create your own administrator, sign in as
   it, and deactivate `Admin`. Until then the `accounts` health line stays
   amber.
5. Configure the mailbox (§5.2), the network for MCR desks (§5.1), and
   optionally the LLM (§5.4).

It installs to `C:\OmniIngest` as the Windows service **OmniIngestService**:
starts with Windows, restarts itself after a failure, panel on port 8080,
firewall opened for domain and private networks.

`Install.cmd` options:

| Option | Meaning |
|---|---|
| `--watchfolder "\\dalet\ingest"` | the Dalet watchfolder |
| `--account "DOMAIN\svc_omni"` | run the service as this account (**required for a network watchfolder**: LocalSystem cannot write to a share). The password is prompted. |
| `--dir D:\OmniIngest` | install somewhere else |
| `--port 8081` | another web port |
| `--yes` | accept the defaults, no questions |
| `--no-firewall` | do not add the firewall rule |
| `--import D:\OmniDownloader` | move an existing setup on this PC (config, database, keys, seeds) |

After installation `data\` and `temp\` are readable only by SYSTEM,
Administrators and the service account, so CLI commands against the
installed root need an elevated prompt.

### 2.2 Upgrading

Run the new release's `Install.cmd`. The service stops, the program and tools
are replaced, the service starts again. Data, configuration, passwords and
keys are kept. Database migrations run on the first start, after a backup is
written to `data\backups\`. If the new version does not answer on
`/api/health` within 4 minutes, the previous release is put back
automatically.

### 2.3 Uninstalling

`Uninstall.cmd` removes the service and the firewall rule. The install folder
(database, configuration, logs) stays; delete it by hand if you want.

---

## 3. The toolchain

| Tool | Version | How it is kept |
|---|---|---|
| ffmpeg / ffprobe, x64 hosts | 9.0.2 (gyan.dev full build) | **pinned** by SHA-256; never upgraded on its own |
| ffmpeg / ffprobe, ARM64 hosts | 9.0.2-39-g7adfbe40dc (BtbN GPL build, native ARM64) | **pinned** by SHA-256; never upgraded on its own |
| bmxtranswrap | 1.6.0 (BBC bmx) | **pinned** by SHA-256; never upgraded on its own |
| yt-dlp | latest stable | verified nightly update at 03:00, previous build kept for rollback |
| Deno | latest | needed by yt-dlp for YouTube; kept current |

**Admin → Toolchain** lists each tool with its state ("pinned build", "not
the pinned build", "missing" or a version) and an **Install** / **Repair**
button. Install downloads the exact pinned build and checks its checksum;
jobs wait while the file is swapped. **Admin → Download engine** shows the
installed and latest yt-dlp, with **Update yt-dlp now** and a rollback to
the previous build.

The service reads the CPU of the machine it runs on. On an ARM64 Windows
host it installs the native BtbN ARM64 ffmpeg and ffprobe, because the x64
build runs under emulation there and has hung. Everywhere else, and whenever
the CPU cannot be determined, it uses the x64 gyan.dev build. bmxtranswrap has
no ARM64 build: the x64 build is used on every host (emulated on ARM64). A
start-up install runs only when a file is missing; it then installs that
tool's whole pinned archive (ffmpeg and ffprobe together). If an x64 ffmpeg is already in
`bin\` on an ARM64 host, the `pinned_tools` health line says so, and
**Admin → Toolchain → Repair** replaces it with the native one.

The encoder is pinned on purpose: the broadcast output was validated with
that exact build. The service never uses an ffmpeg, ffprobe or bmxtranswrap
found on the system PATH. If one is missing from `bin\` at start-up, the
service downloads the pinned build (up to 3 minutes, smallest first). If that
fails, the queue is held, the `pinned_tools` health line says which tool is
missing, and **Admin → Toolchain → Install** followed by a service restart
fixes it.

---

## 4. Where things are

All paths are relative to the install folder, never to the current
directory.

| Path | What |
|---|---|
| `config.json` | configuration (no secrets) |
| `data\omni.db` | the database: queue, mail, users, audit |
| `data\secrets.bin` | encrypted secret store (Windows DPAPI, tied to this PC) |
| `data\backups\` | database backups taken before migrations |
| `data\signin-profile\` | the browser profile used by `cookies sign-in` |
| `bin\` | the tools |
| `logs\omni-ingest.YYYY-MM-DD.log` | daily log, 30 days, secrets removed |
| `temp\jobs\<id>\` | a job's working files (removed when it finishes) |
| `temp\held\<id>\` | repaired videos waiting for approval |
| `archive\YYYY\MM\DD\` | the downloaded source of each delivered job + a `.json` sidecar, kept 14 days |

**Delivered files** land in the watchfolder as `<index>_<JOURNALIST>_<KEYWORD>.mxf`
(for example `1A_PAPADAKI_FOTIA.mxf`). The file is written as a hidden
`.…tmp` and renamed when complete, so Dalet never sees a partial file. An
existing file is never overwritten: a second delivery becomes `…_2.mxf`.

---

## 5. Configuration

Most settings are on the Admin page. The rest are in `config.json`; restart
the service after editing it. `config.example.json` shows every key.

### 5.1 Network and sign-in (`security`)

| Key | Default | Meaning |
|---|---|---|
| `mcr_open_networks` | `["127.0.0.1/32", "::1/128"]` | networks whose PCs open `/mcr` **without** signing in, e.g. `["10.20.0.0/16"]`. Empty: everyone signs in. |
| `trust_proxy_header`, `trusted_proxies` | off | only behind a reverse proxy |
| `session_hours_admin` / `session_hours_user` / `session_days_mcr` | 8 / 12 / 30 | session lengths |
| `session_idle_hours` | 12 | sign out after this long idle |
| `login_rate_limit_per_5min`, `login_rate_limit_per_account_hour` | 5, 10 | login lockout |

For HTTPS set `tls.cert_path` to a `.pfx`; store its passphrase with
`omni-ingest secrets set web.tls_password`.

### 5.2 Mailbox (`graph`)

| Key | Meaning |
|---|---|
| `tenant_id`, `client_id` | the Entra ID app registration |
| `mailbox` | the ingest mailbox address |
| `write_access` | `false` (default): the mailbox is never changed. `true` (needs `Mail.ReadWrite`): processed mail is marked read and moved to `processed_folder`, failed mail to `failed_folder` |

The client secret goes into the secret store, never into `config.json`:
**Admin → Credentials → Microsoft Graph client secret**, or
`omni-ingest secrets set graph.client_secret`. Use **Admin → Mailbox test**
to check access. Mail is polled every `email_poll_interval_secs` (30 s).

The deterministic parser handles every mail on its own. Mail from an address
that matches nobody in the roster still gets processed; the files are named
with a placeholder until you add the person (§6.3).

### 5.3 Processing

| Key | Default | Meaning |
|---|---|---|
| `max_concurrent_downloads` | 2 | jobs downloading at once |
| `max_concurrent_transcodes` | 2 | encodes at once (about CPU cores / 6) |
| `retention_days` | 7 | how long working files of finished jobs are kept |
| `mail_text_retention_days` | 30 | how long mail text is kept for the desk |
| `archive.enabled`, `archive.retention_days` | true, 14 | archive of downloaded sources |
| `parser.urgent_keywords` | ΕΚΤΑΚΤΟ, BREAKING, URGENT | mail that jumps the queue |
| `parser.max_attachment_mb` | 2048 | largest attachment taken |
| `adblock_*` | on | blocklists for the page reader, updated nightly |

### 5.4 LLM assist (optional)

**Admin → LLM assist.** The parser decides every job; the model is only
asked where the parser is unsure (who the mail is for, a keyword). Pick a
provider (LM Studio, Ollama, OpenAI, Azure OpenAI, Gemini, Anthropic,
OpenRouter, Mistral, Groq or any OpenAI-compatible server), **Load models**,
**Test**, then **Save and apply**. Keys are stored encrypted. Online
providers require https and never receive email addresses or phone numbers.
With the LLM off or down the jobs are the same.

### 5.5 Self-healing extraction (`repair`)

When a news page fails the normal route, the repair route can collect page
evidence and try saved recipes, a live page load and (optionally) a model.

| `repair.mode` | Effect |
|---|---|
| `off` (default) | behaves exactly as without repair |
| `shadow` | records what repair *would* do; **Admin → Repair report** shows it. Run this for a week or two first. |
| `hold` | repair runs; a video it finds waits for MCR approval and is deleted if not approved within `hold_hours` (24); the one exception is a trusted recipe (below) |

Optional model rung: `repair.llm_enabled = true` with an explicit
`repair.llm_model`; at most `llm_max_calls_per_day` (15) calls. Make one
call by hand first with **Admin → Test URL** and **Use the model**.

**Trusted recipes deliver on their own.** `repair.auto_deliver_after_approvals`
(default **5**, `0` = never) lets a recipe that MCR approved that many times
on a site, with **no** rejection there, deliver its video straight to the
watchfolder instead of waiting for approval. The recipe must still be active
on that site and unchanged; a recipe the model wrote counts the same way (it
needs 5 real MCR approvals first), and a video the model found itself is always
held. **Any** MCR rejection of that recipe's video on that site ends the trust at once
(«Λάθος βίντεο», «Δεν είναι αξιοποιήσιμο» or «Άλλο»); «Λάθος βίντεο» also
suspends the recipe there, and its videos wait for MCR again. A rejection is
remembered permanently per recipe steps and site: it survives deleting the job,
the 90-day retention purge, and the recipe's promotion to a player recipe. MCR's «Επόμενη
επιλογή» is always held, whatever the trust. The delivery is the
normal atomic one; the job log says «Παραδόθηκε χωρίς έγκριση» and **Admin →
Repair report** counts them per site (*Auto-delivered*). Trust is checked when
the repair starts: a job of the same recipe that is already downloading when
MCR rejects another one still goes to Dalet, so check the next few minutes of
deliveries from that site after a rejection. Set `0` to go back to
"nothing repaired reaches Dalet without MCR".

### 5.6 Secrets

Secrets live only in `data\secrets.bin` (DPAPI). Manage them in
**Admin → Credentials** or with the CLI:

| Key | What |
|---|---|
| `graph.client_secret` | Microsoft Graph app secret |
| `llm.api_key` | LLM provider key |
| `teams.webhook_url` | Teams webhook |
| `web.tls_password` | `.pfx` passphrase |

They are never written to `config.json` or the logs. Keys only work on the
PC they were saved on.

### 5.7 Metrics for a monitoring system (`metrics`)

Optional, **off by default**. Off, `GET /metrics` is a 404 like any unknown
address and nothing is measured. Switch it on in `config.json` and restart:

| Key | Default | Meaning |
|---|---|---|
| `metrics.enabled` | `false` | serve `GET /metrics` in the Prometheus text format |
| `metrics.allow_networks` | `["127.0.0.1/32", "::1/128"]` | clients that may scrape without signing in, e.g. `["10.20.5.10/32"]`; an invalid entry is logged and skipped |

Who may scrape: a client in `metrics.allow_networks`, or an **Admin** session.
Being on `security.mcr_open_networks`, or signed in as MCR or an ordinary user,
does **not** allow it; those get 403. Behind a reverse proxy the same
`trust_proxy_header` rules as everywhere else decide the client address.

The text holds numbers only: no addresses, file names, paths, people or
credentials. Example Prometheus job:

```yaml
scrape_configs:
  - job_name: omni-ingest
    metrics_path: /metrics
    scrape_interval: 30s
    static_configs:
      - targets: ["ingest-pc:8080"]
```

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `omni_jobs` | gauge | `status` | jobs now in each status (every status, 0 when none) |
| `omni_jobs_finished_total` | counter | `status` | jobs that reached a final status since the service started |
| `omni_stage_duration_seconds` | histogram | `stage` | time a job spent in a stage (buckets 1 s to 30 min), since the service started |
| `omni_mail_last_poll_age_seconds` | gauge | none | seconds since the last successful mail poll; absent until one has succeeded |
| `omni_disk_free_bytes` | gauge | `volume` = `temp`, `watchfolder`, `archive` | free space on each volume |
| `omni_workers_busy`, `omni_workers_max` | gauge | none | job runs in progress, and how many may run (downloads + encodes) |
| `omni_health` | gauge | `check` | 0 ok, 1 degraded, 2 down, for each check on the Admin page |

The counter and the histogram start from zero at each service start, which
Prometheus handles as a counter reset.

---

## 6. The Admin page

| Card | Use |
|---|---|
| **Download engine** | yt-dlp version, update now, roll back |
| **Toolchain** | install or repair ffmpeg, ffprobe, bmxtranswrap, yt-dlp, Deno |
| **Credentials** | set or clear the secrets above |
| **Database maintenance** | purge completed jobs older than 7–90 days; vacuum |
| **Self-heal** | the last 30 days of overnight retries on news sites that stopped giving video (nothing is downloaded) |
| **Self-check** | the daily check links ("a link that has a video" per site); add, remove, run now |
| **Sites** | per-site health: recent success, last error, canary checks, **Run now** |
| **Recipes** | learned extraction recipes: export / import `recipes.json`, suspend, retire |
| **Repair report (30 days)** | what the repair route did or would have done |
| **Test URL** | run a page through extraction and the repair ladder without queueing anything |
| **Cookies** | per-site login cookies (§6.1) |
| **Benchmarks** | run the 65-link corpus, extraction only or full pipeline |
| **User accounts** | create users, set role, link to a journalist, reset password, deactivate |
| **Recent mail** | handled mail, reprocess one |
| **LLM assist** | §5.4 |
| **Newsroom taxonomy** | groups and people (roster), import/export, backups (§6.3) |
| **Audit log** | who did what |
| **Scheduled maintenance** | the nightly tasks (§7), run one now |
| **Mailbox test** | check Graph access with given or stored credentials |

### 6.1 Cookies for sites that need a login

Some sites only serve video to a signed-in account (X, Instagram, Facebook,
and YouTube when it shows "confirm you're not a bot"). Use a **dedicated
station account**, never a personal one.

**From the Admin page:**

1. Sign in to the site in a private browser window.
2. Export `cookies.txt` with a cookies.txt browser extension.
3. Close the private window **without signing out** (signing out cancels the
   cookies).
4. **Admin → Cookies**: type the site (`x.com`, `youtube.com`, …),
   **Choose cookies.txt…**, **Upload**, then **Test** with a known link.

**From the command line** (no file to handle): on the server's desktop, in an
Administrator prompt,

```powershell
omni-ingest cookies sign-in youtube.com
```

opens Edge on its own profile; sign in, close the window, and the cookies are
stored. The Cookies card shows this command ready to copy.

`youtu.be` uses the `youtube.com` jar; `twitter.com` is stored as `x.com`.
Every jar is re-tested nightly; the `cookies` health line warns 7 days before
they expire.

YouTube cookies are optional: most YouTube links work without them. Add them
only if LOGIN_REQUIRED keeps coming back.

### 6.2 User roles

| Role | Can |
|---|---|
| Journalist — submits links (`user`) | `/user` only |
| MCR operator — runs the queue (`open_mcr`) | `/mcr` |
| Administrator — full access (`admin`) | everything |

### 6.3 The roster (Newsroom taxonomy)

Each person has a Latin surname (used in file names), full name, email
addresses, aliases (Greek spellings, nicknames, cases) and groups. The parser
matches the sender's address and names in the mail against this list. Export
it as `taxonomy.json` for backup; **the file contains staff names and
addresses: keep it out of shared folders and out of git.** A backup is taken
before every delete or import.

---

## 7. Scheduled maintenance

| Task | When (local time) | What |
|---|---|---|
| `ytdl_update` | daily 03:00 | check, verify and stage a new yt-dlp |
| `adblock_update` | daily 03:30 | refresh blocklists |
| `retention` | daily 04:00 | delete expired archive copies, orphaned temp, old login records, old mail text |
| `selfcheck` | daily 05:15 | browser check and per-site check links |
| `cookie_test` | daily 05:30 | test each cookie jar |
| `vacuum` | every 30 days, 04:30 | compact the database |

---

## 8. Health

`http://<pc>:8080/api/health`, and the health lines on the panels.
Green = ok, amber = degraded, red = down.

| Line | Amber / red means |
|---|---|
| `mail` | the mailbox cannot be read |
| `llm` | the LLM endpoint does not answer (amber; jobs are unaffected) |
| `tools` | a tool is missing (red) |
| `pinned_tools` | a tool is missing or not the pinned build: **Admin → Toolchain → Install / Repair**, then restart the service if it was missing |
| `encoder` | the encoder self-test failed |
| `watchfolder` | the watchfolder is not writable (red) |
| `browser` | no Edge/Chrome usable |
| `disk` | low space (amber); almost full, no new job starts (red) |
| `queue` | jobs are waiting or running unusually long |
| `selfcheck` | a daily check link failed |
| `deno` | Deno missing |
| `accounts` | `Admin` / `Admin` still works: change it |
| `sites` | a site's recent success dropped |
| `cookies` | a jar failed its test or expires within 7 days |
| `lockdown` | the `data` and `temp` folders are not restricted to SYSTEM, Administrators and the service account |
| `archive` | the archive disk is low |

---

## 9. Command line

Run from the install folder (elevated on an installed service).

| Command | What |
|---|---|
| `omni-ingest run [--config <file>]` | run in a console window instead of as a service |
| `omni-ingest install …` / `uninstall` | what `Install.cmd` / `Uninstall.cmd` call (§2) |
| `omni-ingest service install\|start\|stop\|status\|uninstall` | the Windows service |
| `omni-ingest setup` | interactive setup wizard |
| `omni-ingest admin list-users` | list accounts |
| `omni-ingest admin reset-password <email>` | new password (prompted), signs the user out |
| `omni-ingest admin activate\|deactivate <email>` | enable / disable an account |
| `omni-ingest secrets list\|set <key>\|clear <key>` | the secret store (values prompted, never shown) |
| `omni-ingest cookies sign-in <domain> [--browser edge\|chrome]` | sign in once and store the site's cookies |
| `omni-ingest cookies list` | the cookie jars (no contents) |
| `omni-ingest mail-preview [--last 10] [--hours 72] [--eml <file>] [--llm]` | what the parser would do; changes nothing |
| `omni-ingest mail-ingest --eml <file>…` | queue saved `.eml` files as if they had arrived |
| `omni-ingest mail-history [--last 20]` | handled mail, newest first |
| `omni-ingest mail-reprocess --message-id <id> \| --subject <part>` | read a mail again on the next poll |
| `omni-ingest browser-test <url>` | show what the page reader finds on a URL |
| `omni-ingest selfcheck` | run the check links now (downloads nothing) |
| `omni-ingest benchmark run [--mode extract\|full] [--targets 1-65] [--parallel 2]` | the benchmark corpus; `benchmark list`, `benchmark report <id>` |
| `omni-ingest adblock status\|update` | blocklists |

---

## 10. Error codes

A stopped job carries one of these codes; the desk shows a Greek hint.
"Auto" means the job retries by itself (after 30 s, 2 min, then 5 min;
10 min for low disk).

| Code | Auto | Meaning / action |
|---|---|---|
| `NETWORK` | yes | network problem |
| `HTTP_403` | yes | the source refused; try later or another link |
| `EXTRACT_TIMEOUT`, `DOWNLOAD_TIMEOUT` | yes | download timed out |
| `DELIVERY_FAILED`, `DELIVER_TIMEOUT` | yes | could not write to the watchfolder: check the share and the service account's rights |
| `LOW_DISK` | yes | free disk space |
| `LOGIN_REQUIRED` | no | the site wants a login: add cookies (§6.1) |
| `GEO_BLOCKED` | no | not available from Greece |
| `PRIVATE_OR_REMOVED` | no | ask for another link |
| `PAGE_NOT_FOUND` | no | 404; check the link |
| `LIVE_STREAM` | no | live; wait for the recording |
| `UNSUPPORTED_URL` | no | not recognised; try a direct video link |
| `NO_STREAM_FOUND`, `NO_VIDEO_ON_PAGE` | no | no video found on the page |
| `VIDEO_UNRESOLVED` | no | a video is there but was not fetched; download by hand |
| `DRM_PROTECTED` | no | copy-protected; get the file from the source |
| `SOURCE_TOO_LONG` | no | over the duration limit |
| `MANUAL_DOWNLOAD` | no | file-transfer link (WeTransfer etc.): download by hand, mark done |
| `PROBE_FAILED` | no | the downloaded file is unreadable |
| `TRANSCODE_FAILED`, `TRANSCODE_TIMEOUT` | no | encoding failed; report if it repeats |
| `REWRAP_FAILED`, `REWRAP_TIMEOUT` | no | MXF wrapping failed; report if it repeats |
| `COMPLIANCE_FAILED` | no | the result is not to broadcast spec: **not delivered** |
| `LEASE_EXPIRED` | no | interrupted repeatedly (crash or restart) |
| `REJECTED_BY_MCR` | no | MCR rejected a repaired video |
| `APPROVAL_EXPIRED` | no | not approved within 24 h; the held file was deleted |
| `HELD_FILE_CHANGED` | no | the held file changed: not delivered |
| `REPAIR_EXHAUSTED` | no | repair ran out of time or attempts |
| `PIPELINE_FAILED` | no | unexpected error; see the job's history |

---

## 11. Troubleshooting

| Symptom | Check |
|---|---|
| Panel does not open | service running (`omni-ingest service status`), firewall, port |
| Nothing arrives from mail | `mail` health line; **Mailbox test**; the client secret has not expired in Entra ID |
| YouTube LOGIN_REQUIRED | update yt-dlp (**Download engine**); if it persists, add `youtube.com` cookies |
| X / Instagram posts fail | upload that site's cookies and **Test** |
| Wrong video from a news site | **Test URL** on the article; report the article link |
| Files not appearing in Dalet | `watchfolder` health line; for a share, the service must run as an account with write rights (`--account`) |
| Everything waits | `disk` health line (no new jobs below 5 GB) |

**Logs:** `logs\omni-ingest.YYYY-MM-DD.log`. Each job's step-by-step history
is also on its card under «Τεχνικές λεπτομέρειες».

**Rollback:** the installer keeps the previous release, and the
pre-migration database backup is in `data\backups\`. A restored backup needs
the matching older program (the database schema only moves forward).
