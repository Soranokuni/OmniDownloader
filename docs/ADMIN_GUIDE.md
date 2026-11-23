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

A release is built for one architecture, `<arch>` = `x64` or `arm64`, and has
two parts:

- **`OmniIngest-Setup-<version>-<date>-<arch>.exe`**, one file that installs,
  upgrades, changes the service account and uninstalls, with
  `OmniIngest-Setup-<version>-<date>-<arch>.exe.sha256` beside it. This is the
  usual way (§2.1).
- **`OmniIngest-<version>-<date>-<arch>.zip`** (and the same folder unpacked):
  `omni-ingest.exe`, `bin\` (ffmpeg, ffprobe, bmxtranswrap, yt-dlp, Deno, the
  C++ runtime DLLs), `Install.cmd`, `Uninstall.cmd`, `README.txt`, `docs\`
  (these guides) and `SHA256SUMS.txt`, for the command-line route (§2.5).

A 64-bit Intel or AMD server needs the **x64** files; `arm64` is for Windows
on ARM. The Setup file carries the release folder inside it.

**Building the release package.** From a checkout, into `dist\`:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\package.ps1 -Arch x64 -ToolsDir D:\Tools\x64\bin -RuntimeDir D:\Tools\x64\vcrt
```

- `-Arch x64` (the default) or `-Arch arm64` chooses the Rust target of
  `omni-ingest.exe` and the Setup program and is part of every output name.
- `-ToolsDir` (default `bin`) is where `ffmpeg.exe`, `ffprobe.exe`,
  `bmxtranswrap.exe`, their DLLs, yt-dlp and `deno.exe` are taken from;
  `-RuntimeDir` (default `%SystemRoot%\System32`) is where the C++ runtime
  DLLs `msvcp140.dll`, `vcruntime140.dll` and `vcruntime140_1.dll` come from.
- Every `.exe` and `.dll` in the package is checked by its PE header before
  anything is zipped or packed: an x64 package must hold x64 programs only;
  an arm64 package may also hold x64 ones (they run under emulation there, and
  bmxtranswrap exists only as x64). A wrong file stops the script and is named.
  An x64 package therefore needs the x64 tools and the x64 runtime DLLs; on an
  ARM64 PC neither the service's own `bin\` (native ARM64 ffmpeg) nor
  System32's runtime DLLs are x64. No `omni-ingest` command downloads the
  pinned tool set into a folder; take them from an x64 installation's `bin\`.
- The script starts the built programs (`--version`, Setup's `--pack`,
  `--verify`, `/?`): an x64 package can be built on an ARM64 PC, where x64
  programs run under emulation, but an arm64 package has to be built on an
  ARM64 machine.

### 2.1 Installing with Setup

**1. Check the file.** Copy the Setup file and its `.sha256` to the server:
the one ending in `-x64` for a 64-bit Intel or AMD server, `-arm64` only for
Windows on ARM. In PowerShell, in their folder:

```powershell
$f = 'OmniIngest-Setup-1.0.0-20261118-x64.exe'
(Get-FileHash $f -Algorithm SHA256).Hash -eq (Get-Content "$f.sha256").Split(' ')[0]
```

`True` means the file is the one that was built. On `False`, do not run it:
copy it again.

**2. Start it.** Double-click the file. It is not code-signed, so Windows
warns twice:

- On the first run of a downloaded copy, SmartScreen may show **Windows
  protected your PC**. Click **More info**, check that the file name is the
  one you checked, then **Run anyway**.
- Windows then asks for administrator rights and names an unknown publisher.
  Answer **Yes**. If you answer No, Setup says it needs administrator rights
  and changes nothing.

The pages are in Greek when the Windows display language is Greek and in
English otherwise (`/lang el` or `/lang en` chooses). This guide quotes the
English pages.

**3. The pages, in order.**

| Page | What it shows | What to choose |
|---|---|---|
| **Install OmniIngest \<version\>** | the folder, `C:\OmniIngest` unless `/dir` gave another | **Install**. To install elsewhere, press **Change folder…** first (a drive root such as `D:\` becomes `D:\OmniIngest`). A folder directly on a drive, or under Program Files, passes the checks in §2.1c. |
| **Which account should the OmniIngest service run as?** | **The built-in service account (recommended)** and **A Windows account…** | see *The account* below, then **Next >** |
| the Windows credential prompt **OmniIngest service account** | only after **A Windows account…** | the account as `DOMAIN\user` (or `.\user` for an account of this computer) and its password |
| **Ready to install OmniIngest \<version\>** | the folder, the account, the service, the firewall rule, and under **Please note:** any warning (§2.1c) | **Install**. **< Back** returns to the account; **Cancel** changes nothing. |
| **Installing OmniIngest \<version\>…** | the step it is on; **Show details** shows the installer's own lines | wait: the first start of the service can take a few minutes |
| **OmniIngest is installed.** | the admin page's address | **Close** |

**Cancel** on any page before **Install** on the summary changes nothing.

**The account.** The service needs an account that can write to the
watchfolder. There are three real situations:

| The watchfolder is | Choose |
|---|---|
| a folder on this computer | **The built-in service account (recommended)**: the virtual account `NT SERVICE\OmniIngestService`, no password. |
| a share on a Windows file server in the same domain that admits this computer's account (`DOMAIN\THIS-PC$` has Modify on the share and on the folder) | **The built-in service account**: it reaches a share as this computer's account. |
| a NAS, or a share that asks for a user name | **A Windows account…**: a user that has Modify on the share. Setup gives it the right to log on as a service and checks its password with Windows before it touches the service; if Windows refuses, the account page comes back with the reason. |

On a new installation Setup does not ask for the watchfolder: it is the
folder `watchfolder` inside the installation folder until you set the real one
in the setup wizard (§2.1b). If you already know it is on a NAS, choose the
Windows account now; otherwise change the account later (§2.2a).

When the configuration already names a network share as the watchfolder (a
folder that kept its data from an earlier installation, or an answer file)
and the built-in account is chosen, the summary starts with **The watchfolder
is on a network share**, and its default button is **< Back**, which leads to
the account page with **A Windows account…** selected. **Install** on that
summary means: run as the built-in account anyway.

**What Setup does to the computer.**

- Creates the installation folder and copies the program and its tools into
  it (`omni-ingest.exe`, `bin\`), and a copy of Setup itself,
  `OmniIngest-Setup.exe`, which Apps & Features runs later. The guides stay in
  the zip.
- Restricts the folder: inheritance is cut, SYSTEM and Administrators have
  full control, Users can read. The service account gets Modify only on
  `data\`, `temp\`, `logs\`, `archive\`, `bin\`, the file `config.json` and a
  local watchfolder. A watchfolder inside the installation folder also keeps
  Modify for Authenticated Users, which Dalet's ingest account relies on (the
  owner can tighten it). The folder itself and the executable get no write
  right, so no local user can replace the program. Afterwards `data\` and
  `temp\` are readable only by SYSTEM, Administrators and the service account,
  so CLI commands against the installation need an elevated prompt.
- Registers and starts the Windows service **OmniIngestService** (starts with
  Windows, restarts itself after a failure) and waits up to 4 minutes for it to
  answer.
- Adds the Windows Firewall rule **OmniIngest web panel**, which opens the
  panel's port (8080) on domain and private networks. Not with `/no-firewall`,
  and not when the panel listens only on `127.0.0.1`.
- Adds **OmniIngest** to Apps & Features (Settings → Apps), with Modify and
  Uninstall.
- Writes what it did to `setup.log` in the installation folder (§4).

**Where it ends.** **Close** on the last page opens
`http://127.0.0.1:8080/admin` in the browser of this computer. Sign in as
**`Admin` / `Admin`**, the first-run account. It is created once, on a
brand-new installation only, and works only from this computer: from any other
PC the password is refused (it looks like a wrong password, and the attempt is
audited). The setup wizard (§2.1b) then replaces that account with your own
administrator and walks you through the watchfolder and the mailbox. Then
configure the network for MCR desks (§5.1) and optionally the LLM (§5.4).

- If the folder already held a database (an uninstall that kept the data),
  there is no first-run account: the last page says so; sign in with that
  installation's administrator.
- Setup started from an already elevated prompt does not open the browser;
  open the address the last page shows.
- Signing in from this computer needs the panel to listen on all addresses
  (`web_host` `0.0.0.0`, the default) or on `127.0.0.1`. If `web_host` names
  one LAN address, set the password from an elevated prompt instead:
  `omni-ingest admin reset-password admin`.

### 2.1a The watchfolder and its share

Use the share's `\\server\share` path, never a mapped drive letter: a mapping belongs to your Windows session and the service cannot see it. The installer, the setup page and the health page refuse a mapped letter. They show the exact path to use wherever the mapping can be resolved: an elevated installer cannot see your desktop's drives, so it reads the mapping your Windows account remembers; the service itself cannot, and then only says that the letter is not usable and asks for the `\\server\share` path.

Share permissions:

- The account the service runs as needs **Modify** on the share. By default that is the server's computer account `DOMAIN\SERVER$`, and that only works on a domain-joined server.
- For a NAS or a share addressed by IP, run the service as a Windows account the share accepts: **A Windows account…** in Setup (§2.1), or change it later (§2.2a).
- MCR and Dalet need **Modify** to move files out.
- Nobody else needs access.

### 2.1b The first-setup wizard

A new installation is configured from the browser. Open `http://127.0.0.1:8080/admin` **on the server itself** and sign in with the first-run account `admin` / `Admin` (it works only from the server). The Admin page then shows the wizard full-page instead of the panel. It also appears, for any administrator, while `setup_completed` is `false` in `config.json`, which is what a configuration created by this version starts with. A configuration from an older version has no such key and counts as completed, so an upgrade never shows the wizard.

The wizard has three modes, chosen on the first screen. They differ only in how many screens follow:

| Mode | Screens |
|---|---|
| Beginner | the essentials with safe defaults: administrator account, watchfolder, mail, check |
| Intermediate | Beginner plus network access, archive and the language model note (service account, MCR open networks, archive on/off and days, web port) |
| IT / sysadmin | Intermediate plus TLS, reverse proxy, metrics and the answer file |

Back and Next move between screens; Next stays disabled until the screen is valid. Every field shows its default and a one-line explanation.

1. **Welcome.** Pick the mode.
2. **Administrator account** (only while the first-run account still works). Enter a full name, a login (at least 3 characters, no spaces, not `admin`) and a password (at least 12 characters, typed twice; the ticks turn green as the rules are met). On Next the new administrator is created, the first-run `admin` account is switched off and its sessions end, and this browser is signed in as the new administrator. If anything fails, the first-run account stays as it was and you can try again. A link on this screen offers the older route: keep `admin` and only change its password.
3. **Watchfolder.** Enter the share's `\\server\share` path (placeholder `\\192.168.4.42\FileIngest`), never a mapped drive letter. **Test** runs in the daemon itself, as the account the service runs as: it creates the folder if it is missing, writes a probe file, renames it and deletes it, within 15 seconds. On "access denied" it names the account to grant **Modify** on the share and the folder. Next saves the path and is enabled only after a passing test; it starts filled with where the current setting resolves. When the test says access denied, the screen explains the cause (the service is not running as an account the share accepts) and how to run the service as a named account; on an installation made with Setup, do that with Apps & Features → OmniIngest → Modify (§2.2a). **Skip for now** changes nothing: files keep going to the current watchfolder (the local default folder next to the program on a new installation) until you set the share, and the warning says so. See 2.1a.
4. **Mail (Microsoft 365).** Tenant ID, client ID, mailbox and client secret of the app registration (see 5.2). **Test connection** reads the mailbox. The secret goes to the encrypted store and is never shown again; leave it blank later to keep the stored one. **Skip for now** warns that no email is ingested until a mailbox is set.
5. **Network, archive and language model** (Intermediate and IT). Web port, MCR open networks (one CIDR per line; empty means a login for everyone), delivered-source archive on/off and its days. The language model is set on the LLM card (5.4); a button leaves the wizard and opens it. A short note explains that a NAS or IP share needs the service to run as a named account (§2.2a).
6. **TLS, reverse proxy and metrics** (IT only). The `.pfx` certificate path and passphrase, whether to trust the proxy header and from which networks, extra allowed host names, `/metrics` on/off and its networks, and **Download answer file**. A trusted-proxy range wider than /24 (IPv4) or /64 (IPv6) is flagged: every machine in it could speak for any client address. The certificate is opened with its passphrase before it is saved; a file that is not a PKCS#12 bundle, or a wrong passphrase, is refused, because it would stop the web listener at the next start.
7. **Check and finish.** The daemon's current health lines, green, amber or red, each with its hint, and a notice if a restart is needed. **Finish** records `setup_completed` and the mode, and opens the normal panel.

Which settings apply at once and which wait for a restart of the service:

| Applies at once | Needs a restart |
|---|---|
| `setup_completed` and mode, MCR open networks, trust proxy header, trusted proxies, metrics networks | watchfolder, mailbox settings, web port, TLS, allowed hosts, metrics on/off, archive |

The wizard tells you after each save, and again on the last screen, when a restart is needed.

**Leaving early.** On every screen after the account step, **Open the admin panel** leaves the wizard for this browser session without completing setup; the Setup card then says "Setup not finished", and the wizard returns at the next sign-in.

**Running it again.** The Admin page has a **Setup** card with **Run setup wizard again**. The account step is skipped when the first-run account no longer exists. The settings the wizard writes are also available on their own, to an administrator, at `GET /api/admin/settings` and the `POST /api/admin/settings/...` routes the screens use.

**The answer file.** `GET /api/admin/settings/answer-file` (the **Download answer file** button) returns the chosen settings as `omni-answers.json`: watchfolder, mode, ports, networks, archive, TLS path, proxy and metrics settings, the mailbox address and the LLM provider. It never contains a secret (not the mailbox client secret, the LLM key or the certificate passphrase); those are entered again on the new machine. It is the record of an installation, and Setup reads it back for a new installation with `/answer` (§2.4).

### 2.1c What Setup refuses

Setup checks the folder, the configuration and the account before it changes
anything. A refusal ends on the page **Setup cannot continue** with the reason
and **Nothing was changed.** (exit code 5; with `/quiet`, the reason is
printed). On the pages, a refusal about the account (the last five rows)
brings the account page back instead, with **The account was not accepted:**
and the reason; a named account keeps the right to log on as a service that
Setup gave it, nothing else is changed. The messages below are shortened; `<…>` stands for the folder or
account Setup names.

| Setup says | What it means | What to do |
|---|---|---|
| `<folder> exists and belongs to <owner>, not to Administrators.` | The folder was made by another account, which can change its permissions at any time. | Choose another folder, or remove this one. If it is yours, run the `icacls "<folder>" /setowner Administrators /T` line the message gives, then Setup again. |
| `<folder> already exists and <who> can write to it, so files could be planted in it before Setup secures it.` | An existing folder without an installation in it that other users can write to. | Remove the folder and let Setup create it, or choose another one. |
| `<folder>\omni-ingest.exe belongs to <owner>, not to Administrators, so Setup cannot tell whether this installation is the one you made.` | A program in the folder that no administrator put there. | If it is yours, run the `icacls … /setowner Administrators /T` line the message gives, then Setup again. |
| `<folder> is reached through a link (<link>). Give the real folder with /dir.` | The folder, or one above it, is a junction or a symbolic link. | Choose the real folder (**Change folder…** or `/dir`). |
| `<parent> can be renamed or re-permissioned by <who>, so the service's folder could be swapped.` | Someone else may rename a folder above the installation (a folder in a user profile, for example) and swap the whole installation. | Install directly on a drive (`D:\OmniIngest`) or under Program Files, or remove that permission. An existing installation there is upgraded and only warned. |
| `<path> is a link inside the installation.` | A junction or link inside the folder (for example `data` pointing elsewhere). | Remove it, or give the real folder in `config.json`, and run Setup again. |
| `Cannot install: Z: is a mapped network drive (\\nas01\ingest). The service cannot see drive mappings; use \\nas01\ingest\… instead.` | The configuration's watchfolder is a drive letter. A mapping belongs to one user's session (§2.1a). | Put the `\\server\share` path into `config.json` or the answer file. |
| `A network watchfolder needs --account (a domain account), or the computer account granted on the share.` | `/quiet` with a share watchfolder and no `/account`. | Give `/account` (§2.4), or grant this computer's account on the share and give `/account default`. |
| `<account> needs a password, which an unattended install is never given.` | `/quiet` with an account that has a password. | Run Setup with its pages, or use `/account default`. |
| `Windows does not know the account <account>; check the spelling (DOMAIN\user or .\user).` | A misspelt account, or one of a domain this computer does not know. | Correct it on the account page. |
| `Windows did not accept the password given for <account>; …` | A wrong password. | Type it again. |
| `<account> may not log on as a service on this computer (a Group Policy denies it); …` | A domain policy denies the account "Log on as a service". | Ask the domain administrator to allow it for that account. |
| `Windows could not reach a domain controller to check <account>. …` | The computer cannot reach a domain controller now. | Check the network and try again. |
| `The password of <account> has expired…` / `The account <account> is disabled…` / `… is locked out…` | The account cannot log on. | Fix the account in Active Directory (or Computer Management), then try again. |

**Folders reached through a mounted volume or a link.** A path outside the
installation folder that Setup would give the service account rights on (a
local watchfolder, an archive elsewhere) and that is reached through a link or
a mounted volume is not refused: Setup does not change its permissions and
says so under **Please note:** on the summary (with `/quiet`, in `setup.log`),
with the line to run if that is intended:

```text
! E:\Media\ingest is reached through a link or a mounted volume (E:\Media): Setup did not change its permissions. If that is intended, grant them yourself: icacls "E:\Media\ingest" /grant "NT SERVICE\OmniIngestService:(OI)(CI)M"
```

### 2.2 Upgrading

Run the new Setup file on the server (check its `.sha256` first, §2.1). Its
first page is **OmniIngest is installed on this computer**, with the folder,
the version and the account the service runs as.

1. **Upgrade to \<version\>** (*The settings, the database and the service
   account are kept.*).
2. **Ready to upgrade OmniIngest to \<version\>** lists what is kept and ends
   with *If the new version does not start, the previous one is put back.*
   Press **Upgrade**.

Setup stops the service, replaces the program and the broadcast tools (yt-dlp
and Deno only when they are missing: they update themselves), starts the
service again and waits up to 4 minutes for it to answer on `/api/health`.
Kept: `config.json`, the database, the saved passwords and keys, the account
the service runs as, and the previous program as `omni-ingest.exe.prev`.
Database migrations run on the first start, after a backup is written to
`data\backups\`.

How to read the result:

| Last page | Exit code | Meaning |
|---|---|---|
| **OmniIngest was upgraded to \<version\>.** | 0 | Done. |
| **Setup put the previous state back** | 7 | The new version did not come up; the previous one is back and running, for example *The upgrade failed (The service did not answer on 127.0.0.1:8080 within 240 s) and the previous version was put back; it is running.* Send `setup.log` and `logs\` to support. |
| **Setup could not put everything back** | 1 | The message lists what was put back, what was not, and whether the service runs. Act on it; `setup.log` has every step. |
| **Setup could not finish** | 1 (4: a damaged Setup file) | The message, and *The details are in C:\OmniIngest\setup.log.* |

After upgrading to this version everyone signs in once again; session tokens
are now stored hashed.

### 2.2a Changing the service account

Apps & Features → **OmniIngest** → **Modify** opens Setup on the account page
(it runs `C:\OmniIngest\OmniIngest-Setup.exe /change-account`). A Setup file
started by hand offers the same as **Change the account the service runs as**
on its first page.

1. The account page says *The service runs as \<account\> now.* Choose as in
   §2.1, then **Next >**.
2. **Ready to change the service account**: *The service is restarted under
   the new account; if it does not start, Setup puts the previous state
   back.* Press **Change**.
3. **The service now runs as \<account\>.** Files and settings are kept, and the
   new account gets the folder rights of §2.1.

If the service does not start under the new account, the last page is **Setup
put the previous state back** (*The account change failed (…) and was undone;
it is running.*, exit code 7).

Unattended, for an account without a password:
`OmniIngest-Setup.exe /change-account /account default /quiet`.

### 2.3 Uninstalling

Apps & Features → **OmniIngest** → **Uninstall**, or run
`OmniIngest-Setup.exe /uninstall` from the installation folder. The page
**Remove OmniIngest from this computer?** says what happens:

- *Removes:* the Windows service, its firewall rule, the Apps & Features entry
  and the program files (`omni-ingest.exe` and its `.prev`, the tools and DLLs
  in `bin\`, `OmniIngest-Setup.exe`).
- *Keeps:* `config.json`, the database, the secrets, logs, temp and archive.
- *Never removes:* the watchfolder or any video in it.

Press **Remove**. The last page is **Removal finished** (*OmniIngest was
removed.*). When Setup runs from the installation folder (as Apps & Features
starts it), its own file there is removed at the next restart of Windows,
because it is the one running.

**Removing the data too.** Tick *Also remove the database, the saved passwords
and keys, the settings, the logs, the temporary files and the archive of
delivered videos in the installation folder* and press **Remove**. The page
comes back, now listing what goes with the data, and its default button is
**Cancel**: press **Remove** there to go ahead. This also removes, inside the
installation folder only: `config.json` and `config.json.prev`, `setup.log`,
`data\` (the database and its backups, the secret store, the roster seeds,
the adblock lists, the sign-in browser profile), `logs\`, `temp\` (including
videos waiting for MCR approval) and `archive\`. A database, temp or archive
folder configured outside the installation folder is kept, and the page says
so. The installation folder itself goes once it is empty.

The watchfolder is never touched: a folder that is the watchfolder or holds it
is kept wherever it is, and a watchfolder inside the installation folder keeps
that folder too.

If something could not be removed, the last page says how many items and the
Apps & Features entry stays, so the removal can be run again from there.
`Uninstall.cmd` (§2.5) removes only the service and the firewall rule.

### 2.4 Unattended install

For deployment tools that run as administrator: no page, no question.

```text
OmniIngest-Setup-1.0.0-20261118-x64.exe /quiet [/dir <folder>] [/account <name>] [/no-firewall] [/answer <file>]
```

- `/quiet` needs an elevated process (otherwise exit code 3) and never asks
  for elevation. It prints only an error; every line of a run that changed
  something goes to `setup.log`; a refusal, a cancel or a usage error writes
  none.
- `/dir`: the installation folder, an absolute path (default
  `C:\OmniIngest`). For an upgrade, give the folder of the installation.
- `/account`: only an account without a password: `default` (the virtual
  account `NT SERVICE\OmniIngestService`), another `NT SERVICE\…` account,
  `NT AUTHORITY\NetworkService`, `NT AUTHORITY\LocalService` or `LocalSystem`.
  A user account needs its password, and a password is never given on a
  command line: install with the pages, or with the default account and change
  it later with the pages (§2.2a). Without `/account` a new installation runs
  as the virtual account and an upgrade keeps its account.
- `/no-firewall`: no firewall rule.
- `/answer <file>`: the answer file the setup wizard's IT mode exports
  (**Download answer file**, `omni-answers.json`, §2.1b). It is used only for
  a new installation: when the folder already has a `config.json`, Setup keeps
  that and says so. Taken from it: the watchfolder, the web port, the MCR open
  networks, the proxy and allowed-host settings, the archive, the metrics, the
  mailbox's tenant, client and address, the LLM mode (`off`, `assist` or
  `primary`), provider, endpoint and model, and the wizard mode. Never taken: a secret (a key that looks like one refuses
  the whole file), TLS (the certificate path is checked and noted, not applied:
  the certificate and its passphrase are not on the new machine), and "setup
  completed" (a new installation always starts with the wizard). An unknown
  key or a value of the wrong type refuses the whole file before anything is
  installed (exit code 5). With `/quiet`, an answer file whose watchfolder is
  a share needs `/account` too (§2.1c).

Setup is a windowed program: a script must wait for it explicitly.

```bat
start "" /wait OmniIngest-Setup-1.0.0-20261118-x64.exe /quiet /dir D:\OmniIngest /answer C:\Deploy\omni-answers.json
echo %ERRORLEVEL%
```

```powershell
$p = Start-Process .\OmniIngest-Setup-1.0.0-20261118-x64.exe -ArgumentList '/quiet', '/dir', 'D:\OmniIngest' -Wait -PassThru
$p.ExitCode
```

In Windows PowerShell 5.1, put a value with a space in double quotes inside
the single ones (`'"D:\Omni Ingest"'`). `& .\OmniIngest-Setup….exe` does not
reliably wait, and `cmd /c "… > file"` captures nothing; to keep the error
text, add `-NoNewWindow -RedirectStandardError setup-error.txt` to
`Start-Process`.

The same switches remove it (`/uninstall /quiet`, with `/remove-data` to
remove the data too) and change the account (`/change-account /account
<name> /quiet`).

| Exit code | Meaning |
|---|---|
| 0 | Done. |
| 1 | Failed; for an upgrade or an account change, the way back did not restore everything (the message says what). See `setup.log`. |
| 2 | Wrong command line. |
| 3 | Needs administrator rights. |
| 4 | The Setup file is damaged or incomplete: copy it again and check its `.sha256`. |
| 5 | Refused before anything was changed (§2.1c). |
| 6 | Cancelled, or the administrator prompt was declined. |
| 7 | The upgrade or account change failed and the previous state is back and running. |

### 2.5 From the command line (Install.cmd)

The zip installs without Setup, at a console. Unzip it into a local folder on
the server (not on a mapped network drive: the copy of `Install.cmd` that runs
with administrator rights does not see your drive letters) and
double-click **`Install.cmd`** (it asks for administrator rights), give the
Dalet watchfolder when asked, and continue in the browser as in §2.1 (*Where
it ends*). It installs the same way as Setup, except that there is no Apps &
Features entry; Setup can upgrade, change the account of and remove such an
installation later.

| Option | Meaning |
|---|---|
| `--watchfolder "\\nas01\ingest"` | the Dalet watchfolder |
| `--account "NEWSROOM\svc_omni"` | run the service as this account (its password is asked at the console). Default for a new installation: the virtual account `NT SERVICE\OmniIngestService`; `--account LocalSystem` opts out. A network watchfolder needs an account the share accepts (§2.1, *The account*). An upgrade keeps the account; `--account "NT SERVICE\OmniIngestService"` switches an old LocalSystem installation. |
| `--dir D:\OmniIngest` | install somewhere else |
| `--port 8081` | another web port |
| `--yes` | accept the defaults, no questions |
| `--no-firewall` | do not add the firewall rule |
| `--import D:\OmniDownloader` | move an existing setup on this PC (config, database, keys, seeds) |

To upgrade, run the new release's `Install.cmd`: it upgrades as in §2.2. To
uninstall, `Uninstall.cmd` removes the service and the firewall rule; the
installation folder (database, configuration, logs) stays; delete it by hand
if you want.

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
the previous build. If the service stops while the nightly update is still
checking a new yt-dlp or Deno build, the next start puts the previous build back; the next
update run checks the release again.

The verified yt-dlp update also runs early when one platform (YouTube, X,
Facebook, Instagram, TikTok, Vimeo, Dailymotion, Streamable) has 3 extraction
failures in 30 minutes (a rate limit or a login wall does not count), at most
once every 6 hours and only while the nightly update is on
(`ytdl_auto_update_nightly`); it waits at most a minute and a half for running
jobs and tries again later if they are still busy. It is recorded as a run of
`ytdl_update` noted `early: …` (§7); the 03:00 run still happens. When the new
build is kept, the jobs waiting in review for it get their second chance
(§5.3, `retry.second_chance_window_mins`).

yt-dlp is always started with `--ignore-config`, `--no-plugin-dirs` and
`--ffmpeg-location` pointing at the pinned ffmpeg in `bin\` (downloads, probes,
the cookie test, the self-check, the canaries, the benchmark and the repair
route alike). A `yt-dlp.conf` in the service account's profile or a yt-dlp
plugin folder is therefore ignored, and a merge always uses the pinned ffmpeg.

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
| `config.json.prev` | `config.json` as it was before the installer last rewrote it |
| `omni-ingest.exe.prev` | the previous program, kept by an upgrade |
| `OmniIngest-Setup.exe` | the copy of Setup that Apps & Features runs for Modify and Uninstall |
| `setup.log` | what Setup did: each run that changed something, appended under a line with its date (UTC) |
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

If a copy is interrupted, the leftover hidden `.<name>.mxf.tmp` file (and a
leftover `.omni-probe-<number>.tmp`) is removed automatically once it is older
than 10 minutes and nothing has it open, at start-up and when its name is next
needed; finished `.mxf` files and any other file are never touched. The job
records where the file is going just before the final rename, so after a crash
or restart a video that is already in the watchfolder is never delivered a
second time: the job is finished as delivered ("Recovered after a restart").
If the share cannot be read at that moment, the job waits for the watchfolder
(§11.1) with that record; when the share answers, a file that is there finishes
the job without a second copy, a file that is not there sends it to review
(`DELIVERY_UNCONFIRMED`: check Dalet before retrying), and otherwise the job
runs again from its link.

---

## 5. Configuration

The service's write right on `config.json` is an entry on that file itself, so
edit it in place (Notepad is fine); a tool that replaces the file drops the
right, and running the installer again restores it.

Most settings are on the Admin page. The rest are in `config.json`; restart
the service after editing it. `config.example.json` shows every key.

### 5.1 Network and sign-in (`security`)

| Key | Default | Meaning |
|---|---|---|
| `mcr_open_networks` | `["127.0.0.1/32", "::1/128"]` | networks whose PCs open `/mcr` **without** signing in, e.g. `["10.20.0.0/16"]`. Empty: everyone signs in. |
| `trust_proxy_header`, `trusted_proxies` | off | only behind a reverse proxy. The client address is then read from `X-Forwarded-For` **from the right**: entries added by a peer in `trusted_proxies` are skipped and the first other address is the client, so a forged left-hand entry is never believed. An entry that is not an address (or a header line that is not plain ASCII) leaves the client **unknown**: it is then on no allowlist, and is never taken to be the proxy itself. A trusted proxy's `X-Forwarded-Host`, if any, must also name an allowed host (see `allowed_hosts`). |
| `allowed_hosts` | `[]` | extra names the panel is reached by, e.g. a DNS alias or the reverse-proxy name (syntax example only: `["mcr.example.net"]`). The daemon always answers to `localhost`, `127.0.0.1`, `::1`, this PC's NetBIOS name, DNS host name and FQDN, every address of its network interfaces and `web_host`; any other `Host` header, or none, gets **421 Misdirected Request** (DNS-rebinding defence). Needed when users type an alias that is not one of those; restart the service after changing it. Names are read at start-up; interface addresses are read at start-up and again, at most once a minute, when a request names an IP address that is not known (a new DHCP lease or VPN needs no restart). The `Host` the daemon receives must always be allowed. Behind a trusted proxy every entry of `X-Forwarded-Host` must be allowed too, so the proxy must either keep the client's `Host` or pass a name listed here. A request with both an absolute URI and a `Host` header needs them to agree. |
| `egress_allow_networks` | `[]` | networks the job path may request although they are internal (loopback, private, link-local, reserved). Empty: a link to an internal address is never downloaded. Addresses inside pass the internal-address rule; syntax example only: `["10.0.0.0/8"]`. yt-dlp and ffprobe still follow redirects on their own after the check. |
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
| `mail_text_retention_days` | 30 | how long mail text is kept for the desk (in effect at most 90: the handled-mail records themselves are removed after 90 days) |
| `archive.enabled`, `archive.retention_days` | true, 14 | archive of downloaded sources |
| `retry.max_age_mins` | 120 | no automatic retry for a job older than this, counted from when it was queued or from MCR's last «Δοκιμή ξανά» (read as 30 to 360); see §10 |
| `retry.late_after_mins`, `retry.late_after_urgent_mins` | 10, 5 | minutes after which a job that is waiting or running (not yet delivered) counts as late, ordinary and urgent, counted like `retry.max_age_mins` (read as 1 to 120). A late job is listed in the desk's «Καθυστερούν» strip with its reason, and the desk sounds once; a live stream waiting for its next check is listed whatever its age. See the User Guide, §4.9 |
| `retry.second_chance_window_mins` | 120 | a job waiting in review goes back to the queue on its own when what it failed on is fixed: a new yt-dlp or Deno kept after the nightly self-check (jobs that failed to find or read the video), a cookie jar uploaded, signed in or tested good (that site's jobs that needed a login or were rate-limited), a news site that works again (that site's jobs that failed to find the video). Once per cause, at most once for a site and three times in all, never a job a person acted on («Δοκιμή ξανά», cancel, «Το έβαλα στο Dalet», reject, wrong video, an offered video queued…), never a job whose link is already queued or was delivered again meanwhile, and only while the job is younger than this, counted like `retry.max_age_mins` (read as 30 to 360, and never longer than `retry.max_age_mins`). The job's history says «Αυτόματη επανάληψη» and why |
| `pacing.per_host_concurrency` | 1 | yt-dlp runs starting at one video platform (YouTube, X, Facebook, Instagram, TikTok, Vimeo, Dailymotion, Streamable) at a time, so a mail with many links does not trip the platform's bot check (read as 1 to 4). A download gives its turn back as soon as the video starts transferring, so downloads still overlap; a probe keeps it until it ends. A job waiting for its turn keeps its download slot; a wait over 2 s is written to its history |
| `pacing.min_gap_secs` | 8 | seconds between two starts on one platform, plus up to 4 s at random (read as 0 to 120; 0 switches the gap off). After a platform refuses a run as a rate limit (`RATE_LIMITED`), nothing new starts there for 2 minutes |
| `parser.urgent_keywords` | ΕΚΤΑΚΤΟ, BREAKING, URGENT | mail that jumps the queue |
| `parser.max_attachment_mb` | 2048 | largest attachment taken |
| `adblock_*` | on | blocklists for the page reader, updated nightly |
| `alerts.waiting_for_person_mins` | 15 | a video waiting for a person (in review, for a manual download, or held for approval) longer than this is written to the Windows Event Log once (read as 1 to 1440) |

The service writes to the Windows **Application** log (source `OmniIngestService`)
when nobody has the panel open, once when a condition starts and once when it
ends, never repeatedly: a health line turning red (Error, event id 3000; back
to normal: Information, 1000), videos waiting for a person past
`alerts.waiting_for_person_mins` (Warning, 2000; one entry naming them), and
the mailbox not answering for 10 minutes (Error, 3000; answering again:
Information, 1000). A health line's change is written once it has held for 3
minutes, so a line flapping at a threshold writes nothing. E-mail addresses and
known secrets are masked in these entries. After a restart a condition that
still holds is written once more. A console run (`omni-ingest.exe run`) writes these lines to its log
file only.

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
| `off` | behaves exactly as without repair (the value of a config.json written before this version that names no mode) |
| `shadow` (default of a new installation) | records what repair *would* do; **Admin → Repair report** shows it. Run this for a week or two first. Once it would have fixed at least 3 jobs in the last 14 days, the admin Overview offers the switch to `hold` with one click (saved to config.json, used from the next job, no restart; written to the audit log) |
| `hold` | repair runs; a video it finds waits for MCR approval and is deleted if not approved within `hold_hours` (24) of an MCR desk being open (time with no desk open, or the service stopped, does not count; the desk reminds every 30 minutes while a video waits); the one exception is a trusted recipe (below) |

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
| **Setup** | whether the setup wizard was finished; **Run setup wizard again** (§2.1b) |
| **Download engine** | yt-dlp version, update now, roll back |
| **Toolchain** | install or repair ffmpeg, ffprobe, bmxtranswrap, yt-dlp, Deno |
| **Credentials** | set or clear the secrets above |
| **Database maintenance** | purge completed jobs older than 7–90 days; vacuum |
| **Autonomy** | how often a video reached air with nobody touching it, and why a person was needed (below) |
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

**Autonomy report.** The first card on the right. *Delivered untouched* is the
share of the videos that reached air (delivered to the watchfolder, or put into
Dalet by hand) that nobody touched: no retry, rename, approval or rejection,
next choice, cancel, re-download, "wrong video", "run now", discard or manual
completion. A video MCR queued by hand from an offered video or a skipped mail
link starts touched (*Queued by MCR from an offer or a skipped link*); a link
added on the desk or the journalist panel does not. Automatic retries and
waiting for the watchfolder do not count as a touch. A video later marked as
wrong counts as touched. This is the figure the 14-day soak test is judged on
(95 % or more). The smaller *Strict* figure divides by every finished job, cancelled and discarded
ones included. The tables show the first error code of the jobs that needed a
person (*Why a person was needed*), the first thing the operator did (*What the
operator did*) and the sites; *Waiting for a person now* counts the jobs in
review, manual download or waiting for approval. Choose 7, 14 or 30 days. The
same figures on the server, safe while the service runs:

```powershell
.\omni-ingest.exe report autonomy --days 14
```

The report keeps one small row per finished job for 400 days, with the site's
domain only: never a link, file name or journalist.

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
Every jar is re-tested nightly; the `cookies` health line is computed again
every day and warns 7 days before they expire. While jobs in review wait for a
site's login (no jar, an expired one, or one that failed its test), the MCR
desk counts them per site.

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
| `db_backup` | daily 02:30 | back up the database while the service runs into `data\backups\omni-YYYY-MM-DD.db`; the copy is checked before it counts; the 7 newest and one for each of the 4 weeks before them are kept |
| `db_check` | every 7 days, 02:45 | integrity check of the database (`PRAGMA quick_check`) |
| `ytdl_update` | daily 03:00 | check, verify and stage a new yt-dlp; also run early when a platform starts failing, noted `early: …` (§3) |
| `adblock_update` | daily 03:30 | refresh blocklists |
| `retention` | daily 04:00 | delete expired archive copies, orphaned temp, old login records, old mail text, and the old database records below |
| `selfcheck` | daily 05:15 | browser check and per-site check links |
| `cookie_test` | daily 05:30 | test each cookie jar |
| `vacuum` | every 30 days, 04:30 | compact the database |

A task that fails on its last run turns the **maintenance** health line amber
and names it; its next good run clears it. The **database** line shows the
last integrity check and the last backup; it turns red if the check finds the
database damaged. Then stop the service, move `data\omni.db` together with
`omni.db-wal` and `omni.db-shm` (if present) to another folder, copy the newest
`omni-YYYY-MM-DD.db` from `data\backups\` (never a `.tmp`: that is a copy that
was not finished or failed its check) to `data\omni.db`, and start the service
again. The backups hold the whole database, accounts and sessions included:
they stay in `data\`, which only the service and administrators can read.

What the `retention` task keeps in the database, and for how long (fixed, not configurable):

| Records | Kept |
|---|---|
| Finished jobs (completed, failed, needs review, manual download, cancelled) with their timeline | 90 days after they finished; a job waiting, running or awaiting approval is never removed, and a job that ever had a held output is kept, with its timeline |
| Handled-mail records | 90 days after the mail was received |
| Audit log | 365 days |
| Expired sessions | removed daily |

Each run that removes anything adds one `retention` line to the audit log with the counts. The newest audit entry is never removed. To keep a purged mail record from letting an old mail back in, the mail watcher also never ingests a mail received more than 83 days ago, whatever the checkpoint says.

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
| `browser` | Edge/Chrome failed its test page (amber); **red "sandboxed launch failed"**: the browser does not start at all. It always runs with Chromium's sandbox, which is never turned off: check the service account and the browser install |
| `disk` | low space (amber); almost full, no new job starts (red) |
| `queue` | jobs are waiting or running unusually long |
| `selfcheck` | a daily check link failed |
| `deno` | Deno missing |
| `accounts` | `Admin` / `Admin` still works: change it |
| `sites` | a site's recent success dropped |
| `cookies` | a jar failed its test or expires within 7 days |
| `lockdown` | the `data` and `temp` folders are not restricted to SYSTEM, Administrators and the service account |
| `archive` | the archive disk is low |
| `workers` | a job run hit an internal error in the last 24 hours (amber). Before delivery the job goes back to the queue once (after 30 s), then to review; during delivery it is never queued again: it is finished as delivered when its file is in the watchfolder, otherwise it goes to review with `DELIVERY_UNCONFIRMED`. If it repeats, send `logs\` to support |

---

## 9. Command line

Run from the install folder (elevated on an installed service).

| Command | What |
|---|---|
| `omni-ingest run [--config <file>]` | run in a console window instead of as a service |
| `omni-ingest install …` / `uninstall` | what `Install.cmd` / `Uninstall.cmd` call (§2.5) |
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
| `omni-ingest report autonomy [--days 14]` | the Autonomy card's figures as text, for the last N days (1 to 365); reads only, safe while the service runs (§6) |
| `omni-ingest browser-test <url>` | show what the page reader finds on a URL |
| `omni-ingest selfcheck` | run the check links now (downloads nothing) |
| `omni-ingest benchmark run [--mode extract\|full] [--targets 1-65] [--parallel 2]` | the benchmark corpus; `benchmark list`, `benchmark report <id>` |
| `omni-ingest adblock status\|update` | blocklists |

**Setup** (`OmniIngest-Setup.exe`, §2). Switches may be written `/x`, `-x` or
`--x`, in any case. Without `/quiet` Setup shows its pages and asks for
administrator rights itself; `OmniIngest-Setup.exe /?` prints this list.

| Command | What |
|---|---|
| `OmniIngest-Setup.exe [/dir <folder>] [/account <name>] [/no-firewall] [/answer <file>] [/quiet]` | install, or upgrade the installation that is there (§2.1, §2.2, §2.4) |
| `OmniIngest-Setup.exe /change-account [/account <name>] [/quiet]` | run the service under another account (`default` = the virtual account); with `/quiet`, `/account` is required (§2.2a) |
| `OmniIngest-Setup.exe /uninstall [/remove-data] [/dir <folder>] [/quiet]` | remove the service and the program files; `/remove-data` also the configuration and data inside the installation folder; never the watchfolder (§2.3) |
| `OmniIngest-Setup.exe --pack <release folder> --out <file>` | build a Setup that carries a release folder (`scripts\package.ps1` does this); refuses an existing `--out` |
| `/lang el\|en` | the language of the pages (default: the Windows display language) |

There is no password switch: a password is only ever typed in the Windows
credential prompt. Exit codes: §2.4.

---

## 10. Error codes

A stopped job carries one of these codes; the desk shows a Greek hint.
"Auto" means the job retries by itself: after 30 s, 2 min, 5 min and
10 min, then it goes to review (five tries in all, about 17 minutes);
`HTTP_403` only after 30 s and 2 min (three tries), then the page's own
checks and the repair route (§5.5) or review take over. A job
older than `retry.max_age_mins` (2 hours), counted from when it was queued
or from MCR's last «Δοκιμή ξανά», is not retried. `LOW_DISK` retries after
10 minutes three times (four tries), then goes to review: the clip may need
more room than the temp or watchfolder volume has. While the disk check is
red, no job is started at all; they wait in the queue. `RATE_LIMITED`
retries after 2, 5, 10 and 15 minutes (five tries, about half an hour),
within the same age limit. While a job waits out one of these waits, the desk
lists it as late once it is past `retry.late_after_mins`, and MCR's «Τώρα» runs
it at once (that counts as a person acting on the job). A job in review with
`LOGIN_REQUIRED` or `RATE_LIMITED` (a cookie jar for its site saved or tested
good), or with `NO_STREAM_FOUND`, `VIDEO_UNRESOLVED`, `HTTP_403` or
`UNSUPPORTED_URL` (a new yt-dlp or Deno kept, or its news site working again),
may go back to the queue on its own: the second chance of §5.3
(`retry.second_chance_window_mins`).

A job that fails with `PROBE_FAILED`, `TRANSCODE_FAILED`, `REWRAP_FAILED` or
`COMPLIANCE_FAILED` first takes a second route, once per job and without anyone
acting: a link is downloaded again with other formats (another video codec, or
a file the site serves already merged as MP4, either at least 720 lines high),
an attached file is taken as it is, and the file is copied into a clean
container (fresh timestamps) before the usual encode, wrap and compliance
check. The job's history says «second route», the new source's size, and how
it ended. If it does not help, the job goes to review with its first code. A
file is never shortened to fit: one whose own index lists more frames than it
holds (a download cut part way), or whose copy is shorter than the file says
(a clock that jumps), goes to review. A daemon that is stopping does not take
the second route.

| Code | Auto | Meaning / action |
|---|---|---|
| `NETWORK` | yes | network problem |
| `RATE_LIMITED` | yes | the platform is refusing downloads for now (YouTube's bot check, HTTP 429, "try again later"); nothing new starts there for 2 minutes (§5.3 `pacing`). When the retries run out, the card adds the site's cookie advice (§6.1). The same bot check on X, Facebook or Instagram is `LOGIN_REQUIRED` |
| `HTTP_403` | yes | the source refused; try later or another link |
| `EXTRACT_TIMEOUT`, `DOWNLOAD_TIMEOUT` | yes | download timed out |
| `DELIVERY_FAILED`, `DELIVER_TIMEOUT` | yes | could not write to the watchfolder: check the share and the service account's rights |
| `LOW_DISK` | yes | free disk space |
| `LIVE_STREAM` | yes | still live, or a premiere not yet started: checked again 10, 30, 60 and 120 minutes after the first try found it live (the checks stop at the age limit, `retry.max_age_mins`, counted from when it was queued), then review. While it waits it is in the late strip as «Ζωντανή μετάδοση· επόμενος έλεγχος HH:MM» |
| `LOGIN_REQUIRED` | no | the site wants a login: add cookies (§6.1) |
| `GEO_BLOCKED` | no | not available from Greece |
| `PRIVATE_OR_REMOVED` | no | ask for another link |
| `PAGE_NOT_FOUND` | no | 404; check the link |
| `UNSUPPORTED_URL` | no | not recognised; try a direct video link |
| `NO_STREAM_FOUND`, `NO_VIDEO_ON_PAGE` | no | no video found on the page |
| `VIDEO_UNRESOLVED` | no | a video is there but was not fetched; download by hand |
| `DRM_PROTECTED` | no | copy-protected; get the file from the source |
| `SOURCE_TOO_LONG` | no | over the duration limit |
| `MANUAL_DOWNLOAD` | no | file-transfer link a person must fetch (WeTransfer, MyAirBridge, TransferNow, Filemail, MEGA, or a shared folder): download by hand, mark done. One-file shares on Dropbox, Google Drive, OneDrive and SharePoint are downloaded directly, no account needed |
| `PROBE_FAILED` | no | the downloaded file is unreadable, or is not an accepted video container. Accepted: MP4/MOV/3GP, MKV/WebM, MPEG-TS, MPEG-PS, AVI, WMV, FLV, MXF, DV, GXF, Ogg, raw H.264/HEVC/MPEG video and still images. Playlists (HLS, DASH, concat) and scripts (AviSynth, VapourSynth) are always refused |
| `TRANSCODE_FAILED`, `TRANSCODE_TIMEOUT` | no | encoding failed; report if it repeats |
| `REWRAP_FAILED`, `REWRAP_TIMEOUT` | no | MXF wrapping failed; report if it repeats |
| `COMPLIANCE_FAILED` | no | the result is not to broadcast spec: **not delivered** |
| `LEASE_EXPIRED` | no | interrupted repeatedly (crash or restart) |
| `REJECTED_BY_MCR` | no | MCR rejected a repaired video |
| `APPROVAL_EXPIRED` | no | not approved within 24 h of an MCR desk being open; the held file was deleted |
| `HELD_FILE_CHANGED` | no | the held file changed: not delivered |
| `REPAIR_EXHAUSTED` | no | repair ran out of time or attempts |
| `NON_PUBLIC_ADDRESS` | no | the link, or a stream behind it, is an internal address: not downloaded (see `egress_allow_networks`) |
| `DELIVERY_UNCONFIRMED` | no | a delivery was interrupted; check Dalet for the file before retrying |
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
| Files not appearing in Dalet | `watchfolder` health line; for a share, the service must run as an account with write rights on it (§2.2a), or the computer account must be granted on the share |
| Everything waits | `disk` health line (no new jobs below 5 GB) |
| The desk lists videos under «Καθυστερούν» | the reason on each row: «Περιμένει τον φάκελο παράδοσης» is a watchfolder outage (§11.1, `watchfolder` health line); «Νέα προσπάθεια στις …» is an automatic retry, with the code's hint (§10); «Ζωντανή μετάδοση· επόμενος έλεγχος …» is a stream still live (`LIVE_STREAM`). Many YouTube rows at once with «Ο ιστότοπος περιορίζει προσωρινά τις λήψεις»: `RATE_LIMITED`, see `pacing` (§5.3) and cookies (§6.1) |
| Windows says *Windows protected your PC* when Setup starts | the file is not code-signed: check its `.sha256`, then **More info** → **Run anyway** (§2.1) |
| Setup says **Setup cannot continue** | a refusal; nothing was changed. Find the message in §2.1c |
| Setup says **The account was not accepted:** | the reason follows (§2.1c); correct the account or password on the same page |
| Setup says **Setup could not finish** | the page names `setup.log` in the installation folder; the service's own log is in `logs\` |
| Setup says **Setup put the previous state back** | the new version or account did not come up; the previous state runs (exit code 7). `setup.log` and `logs\` say why |
| Setup says **Setup could not put everything back** | the message lists what is in place now; act on it, then run Setup again |
| Setup says the file is damaged (exit code 4) | copy it again and check its `.sha256` |
| Setup ends with *Could not remove Setup's work folder … delete it by hand.* | delete the folder it names under `C:\ProgramData` |

**Logs:** `logs\omni-ingest.YYYY-MM-DD.log`. Setup writes to `setup.log` in the
installation folder (a refusal or a cancel, which change nothing, write
nothing; after an uninstall with the data, there is none). Each job's step-by-step history
is also on its card under «Τεχνικές λεπτομέρειες».

**Rollback:** the installer keeps the previous release, and the
pre-migration database backup is in `data\backups\`. A restored backup needs
the matching older program (the database schema only moves forward).

### 11.1 When the watchfolder share is down

- A finished video that cannot be written to the watchfolder waits on this
  computer and is delivered when the share answers again, however long that
  takes. It is not downloaded or encoded again, with two exceptions: a
  waiting video whose saved file changed or disappeared, and a job recovered
  after a restart that still has to deliver, are downloaded and encoded again
  from their link.
- A waiting video whose saved file cannot be read for 2 hours goes to review
  with `PIPELINE_FAILED`.
- The `watchfolder` health line turns red while the share does not answer.
- A video whose delivery was interrupted (a restart or an error during the
  copy) and whose arrival cannot be confirmed goes to review with
  `DELIVERY_UNCONFIRMED`. Check in Dalet whether it is already there: if it
  is, press «Το έβαλα στο Dalet»; if not, press «Δοκιμή ξανά».
- A waiting video cannot be renamed.
