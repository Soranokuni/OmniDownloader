<#
.SYNOPSIS
    One pass of the 14-day soak feed: replays mail into an isolated omni-ingest
    instance and checks the files it delivers (plan P14.1).

.DESCRIPTION
    Meant to run every 15 minutes from a Task Scheduler task, one pass per
    invocation. It keeps no state except files under -Root, so a reboot or a
    missed run only delays work. It never starts or stops the daemon.

    A pass does, in order:
      1. loads or creates <Root>\soak_state.json (start date, entries sent);
      2. skips sending when the soak window is over or the drive has under
         20 GB free (step 4 still runs);
      3. builds today's plan from the day index alone (the same plan on every
         pass of a day): every benchmark corpus link, cut into mails of 1 to 8
         links, the saved sample mails, and a daily canary mail when a source
         for canary links exists. Every mail of a day is sent from one invented
         journalist (day index mod 14). Due mails are replayed with
         `omni-ingest mail-ingest`, at most 3 per pass, oldest first;
      4. checks every new *.mxf in the test watchfolder with ffprobe against
         the broadcast format, appends the verdict to <Root>\soak_log.csv, and
         deletes checked files older than one hour from that folder;
      5. appends a one-line summary to <Root>\soak_feed.log.

    Exit code 0: the pass ran. 1: it could not finish. 2: refused, nothing was
    written (the root, config, binary or watchfolder is not safe for a soak).

    The corpus is read from crates\omni-core\src\benchmark.rs, so it cannot
    drift from the benchmark's. Write the journalist roster with -WriteSeed
    BEFORE the soak instance first opens its database: the roster is read only
    when the journalists table is empty.

.PARAMETER Root
    The soak root, for example D:\OmniSoak. It must exist, lie outside any git
    repository and hold a config.json with no mailbox settings and a watchfolder
    inside it.

.PARAMETER Binary
    The omni-ingest.exe to run (default <Root>\omni-ingest.exe). Never one under
    target\release, which is the live daemon's.

.PARAMETER Days
    Length of the run in days, counted from the start date stored on the first
    pass (default 14).

.PARAMETER DryRun
    Print today's plan, what this pass would send and what it would check or
    delete. Writes, deletes and starts nothing.

.PARAMETER WriteSeed
    Write <Root>\data\journalists.seed.json (14 invented journalists, one per
    soak day) unless it exists, then exit. An existing file is never overwritten.

.PARAMETER SampleDir
    Saved .eml files re-sent once a day (default temp\dev\mails in the repository
    that holds this script). Missing is fine; they are skipped.

.PARAMETER LinksPerDay
    Corpus links sent per day (default 0 = the whole corpus). With N > 0, day D
    takes positions D*N .. D*N+N-1 of the corpus, modulo its size, then shuffles
    and cuts them into mails. Negative is refused. Larger than the corpus = all.

.PARAMETER CorpusSource
    The source file that holds the benchmark corpus (default
    crates\omni-core\src\benchmark.rs next to this script's repository).
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Root,
    [string]$Binary = '',
    [ValidateRange(1, 365)]
    [int]$Days = 14,
    [switch]$DryRun,
    [switch]$WriteSeed,
    [string]$SampleDir = '',
    [string]$CorpusSource = '',
    [int]$LinksPerDay = 0
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$MinFreeGB = 20
$MaxMailsPerPass = 3
$IngestTimeoutMs = 5 * 60 * 1000
$ProbeTimeoutMs = 2 * 60 * 1000
$MaxAttempts = 3
$SettleSeconds = 60
$PruneAfterHours = 1
$JournalistCount = 14
$FirstMinute = 7 * 60
$SendSpanMinutes = 15 * 60
$CanaryMinute = 12 * 60

# Files are written as UTF-8 without a BOM. Sample mails are handled as Latin-1
# so every byte survives the header rewrite unchanged.
$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$Latin1 = [System.Text.Encoding]::GetEncoding(28591)
$script:LogPath = $null

# --------------------------------------------------------------------------- helpers

function Stop-Refused {
    param([string]$Message)
    # Not Write-Error: with 'Stop' that would throw instead of exiting 2.
    [Console]::Error.WriteLine("soak_feed: refused: $Message")
    exit 2
}

function Write-Log {
    param([string]$Message)
    $line = '{0} {1}' -f (Get-Date).ToString('s'), $Message
    Write-Host $line
    if ((-not $DryRun) -and $script:LogPath) {
        [System.IO.File]::AppendAllText($script:LogPath, $line + "`r`n", $Utf8NoBom)
    }
}

function Get-Prop {
    param($Object, [string]$Name)
    # Strict mode throws on a missing property; configs and ffprobe output vary.
    if ($null -eq $Object) { return $null }
    $p = $Object.PSObject.Properties[$Name]
    if ($null -eq $p) { return $null }
    return $p.Value
}

function Get-DirWithSeparator {
    param([string]$Path)
    # The trailing separator is what keeps D:\OmniSoak2 from being "inside" D:\OmniSoak.
    return $Path.TrimEnd('\', '/') + '\'
}

function Save-TextAtomic {
    param([string]$Path, [string]$Text)
    # Temp file, then rename: a crash cannot leave half a state file.
    $tmp = $Path + '.tmp'
    [System.IO.File]::WriteAllText($tmp, $Text, $Utf8NoBom)
    # Move-Item -Force replaces in one step on the same volume; File.Replace
    # with a $null backup name throws on Windows PowerShell 5.1.
    Move-Item -LiteralPath $tmp -Destination $Path -Force
}

function Invoke-Tool {
    param([string]$FilePath, [string]$Arguments, [int]$TimeoutMs)
    # System.Diagnostics.Process rather than `&`: a timeout must kill this one
    # PID and nothing else, and native stderr must not raise under 'Stop'.
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $FilePath
    $psi.Arguments = $Arguments
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    $proc = [System.Diagnostics.Process]::Start($psi)
    $outTask = $proc.StandardOutput.ReadToEndAsync()
    $errTask = $proc.StandardError.ReadToEndAsync()
    $timedOut = $false
    if (-not $proc.WaitForExit($TimeoutMs)) {
        $timedOut = $true
        try { $proc.Kill() } catch { }
        $proc.WaitForExit()
    }
    $exit = -1
    if (-not $timedOut) { $exit = $proc.ExitCode }
    $result = [pscustomobject]@{
        ExitCode = $exit
        TimedOut = $timedOut
        Out      = $outTask.Result
        Err      = $errTask.Result
    }
    $proc.Dispose()
    return $result
}

function ConvertFrom-UnicodeEscape {
    param([string]$Text)
    # The script stays ASCII-only: Windows PowerShell 5.1 reads a BOM-less .ps1
    # as ANSI and would garble Greek literals.
    return [regex]::Replace($Text, '\\u([0-9A-Fa-f]{4})', {
        param($m)
        [string][char][Convert]::ToInt32($m.Groups[1].Value, 16)
    })
}

function Get-FreeGB {
    param([string]$Path)
    try {
        $drive = New-Object System.IO.DriveInfo ([System.IO.Path]::GetPathRoot($Path))
        return [math]::Round($drive.AvailableFreeSpace / 1GB, 1)
    } catch {
        return -1
    }
}

# --------------------------------------------------------------------------- invented people and words

function Get-SoakJournalist {
    param([int]$Number)
    # Invented; the address is how the parser attributes a mail (sender match).
    $nn = '{0:D2}' -f $Number
    return [pscustomobject]@{
        Surname  = 'SOAK' + $nn
        FullName = 'Soak Tester ' + $nn
        Email    = 'soak.' + $nn + '@example.invalid'
    }
}

# Uppercase Greek, no accents, the way a slug line is written.
$TitleWords = @(
    '\u039B\u0399\u039C\u0391\u039D\u0399',
    '\u0394\u0397\u039C\u0391\u03A1\u03A7\u039F\u03A3',
    '\u0391\u0395\u03A1\u039F\u0394\u03A1\u039F\u039C\u0399\u039F',
    '\u03A0\u03A5\u03A1\u039A\u0391\u0393\u0399\u0391',
    '\u0392\u03A1\u039F\u03A7\u0397',
    '\u03A3\u03A7\u039F\u039B\u0395\u0399\u039F',
    '\u039D\u039F\u03A3\u039F\u039A\u039F\u039C\u0395\u0399\u039F',
    '\u03A4\u039F\u03A5\u03A1\u0399\u03A3\u039C\u039F\u03A3',
    '\u0391\u0393\u039F\u03A1\u0391',
    '\u03A3\u03A5\u039D\u0391\u03A5\u039B\u0399\u0391'
) | ForEach-Object { ConvertFrom-UnicodeEscape $_ }
$Greeting = ConvertFrom-UnicodeEscape '\u039A\u0391\u039B\u0397\u039C\u0395\u03A1\u0391'

# --------------------------------------------------------------------------- deterministic generator

function New-Rng {
    param([long]$Day, [int]$Stream)
    # A 32-bit LCG of our own: Get-Random -SetSeed would change global state.
    $rng = @{ State = ((($Day + 1) * 2654435761 + $Stream * 97531) % 4294967296) }
    for ($i = 0; $i -lt 4; $i++) { [void](Get-RngInt $rng 2) }
    return $rng
}

function Get-RngInt {
    param($Rng, [int]$Max)
    $Rng.State = ($Rng.State * 1664525 + 1013904223) % 4294967296
    # The high bits: the low bits of a power-of-two LCG have short cycles.
    return [int](($Rng.State -shr 8) % $Max)
}

function Get-Shuffled {
    param($Rng, $Items)
    $a = @($Items)
    for ($i = $a.Count - 1; $i -gt 0; $i--) {
        $j = Get-RngInt $Rng ($i + 1)
        $t = $a[$i]; $a[$i] = $a[$j]; $a[$j] = $t
    }
    return , $a
}

function Format-Minute {
    param([int]$Minute)
    return '{0:D2}:{1:D2}' -f ([int][math]::Floor($Minute / 60)), ($Minute % 60)
}

# --------------------------------------------------------------------------- corpus and plan

function Read-Corpus {
    param([string]$Path)
    # The 65 links live in the benchmark source; row(...) and expected_fail(...)
    # lines are the whole format.
    $rows = New-Object System.Collections.Generic.List[object]
    foreach ($line in [System.IO.File]::ReadAllLines($Path)) {
        if ($line -match '^\s*(row|expected_fail)\(\s*(\d+),\s*"[^"]*",\s*"[^"]*",\s*"([^"]+)"') {
            $rows.Add([pscustomobject]@{
                Idx          = [int]$Matches[2]
                Url          = $Matches[3]
                ExpectedFail = ($Matches[1] -eq 'expected_fail')
            })
        }
    }
    return , $rows.ToArray()
}

function Get-CanaryLinks {
    # A later CLI (or the lead, by hand) writes <Root>\soak_canary_links.txt: no
    # omni-ingest subcommand prints the canary table and PowerShell has no SQLite reader.
    # Read at send time, so the file's content then is what goes out.
    param([string]$Path)
    $urls = New-Object System.Collections.Generic.List[string]
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return , @() }
    foreach ($line in [System.IO.File]::ReadAllLines($Path)) {
        $text = $line.Trim()
        if (($text -eq '') -or $text.StartsWith('#')) { continue }
        $uri = $null
        if ([System.Uri]::TryCreate($text, [System.UriKind]::Absolute, [ref]$uri) -and (($uri.Scheme -eq 'http') -or ($uri.Scheme -eq 'https'))) {
            if ($urls.Count -lt 8) { $urls.Add($text) }
        } else {
            Write-Log "canary links file: skipped a line that is not an absolute http/https URL"
        }
    }
    return , $urls.ToArray()
}

function New-LinkRecord {
    param($Rng, [string]$Url, [bool]$ExpectedFail)
    $first = Get-RngInt $Rng $TitleWords.Count
    $a = $TitleWords[$first]
    # A second word that differs from the first.
    $b = $TitleWords[($first + 1 + (Get-RngInt $Rng ($TitleWords.Count - 1))) % $TitleWords.Count]
    return [pscustomobject]@{ Url = $Url; ExpectedFail = $ExpectedFail; Title = ($a + ' ' + $b) }
}

function Get-DayPlan {
    param([int]$Day, $Corpus, $SampleFiles, $CanaryLinks, [int]$LinksPerDay)
    $dd = '{0:D2}' -f $Day
    $plan = New-Object System.Collections.Generic.List[object]

    # Corpus: shuffled, cut into mails of 1 to 8 links, spread over the day.
    $rng = New-Rng $Day 1
    $todays = @($Corpus)
    if (($LinksPerDay -gt 0) -and ($LinksPerDay -lt $todays.Count)) {
        # A rotating window in file order: consecutive days do not repeat a link
        # while 2*N <= corpus size, and every link comes round.
        $size = $todays.Count
        $window = @()
        for ($k = 0; $k -lt $LinksPerDay; $k++) { $window += $Corpus[(([long]$Day * $LinksPerDay) + $k) % $size] }
        $todays = $window
    }
    $order = Get-Shuffled $rng $todays
    $mails = New-Object System.Collections.Generic.List[object]
    $pos = 0
    while ($pos -lt $order.Count) {
        $n = [math]::Min(1 + (Get-RngInt $rng 8), $order.Count - $pos)
        $links = @()
        for ($k = 0; $k -lt $n; $k++) {
            $row = $order[$pos + $k]
            $links += New-LinkRecord $rng $row.Url $row.ExpectedFail
        }
        $mails.Add($links)
        $pos += $n
    }
    $count = $mails.Count
    $width = [math]::Max(1, [int][math]::Floor($SendSpanMinutes / [math]::Max(1, $count)))
    for ($i = 0; $i -lt $count; $i++) {
        $minute = $FirstMinute + [int][math]::Floor($SendSpanMinutes * $i / $count) + (Get-RngInt $rng $width)
        $plan.Add([pscustomobject]@{
            Id = ('d{0}-c{1:D2}' -f $dd, ($i + 1)); Kind = 'corpus'; Minute = $minute
            Links = @($mails[$i]); SamplePath = $null
            Subject = ('Soak day {0} mail {1:D2}' -f $dd, ($i + 1))
        })
    }

    # Sample mails on their own stream, so a missing folder does not move the rest.
    $samples = @($SampleFiles)
    if ($samples.Count -gt 0) {
        $rng2 = New-Rng $Day 2
        $width2 = [math]::Max(1, [int][math]::Floor($SendSpanMinutes / $samples.Count))
        for ($i = 0; $i -lt $samples.Count; $i++) {
            $minute = $FirstMinute + [int][math]::Floor($SendSpanMinutes * $i / $samples.Count) + (Get-RngInt $rng2 $width2)
            $plan.Add([pscustomobject]@{
                Id = ('d{0}-s{1}' -f $dd, ($i + 1)); Kind = 'sample'; Minute = $minute
                Links = @(); SamplePath = $samples[$i]; Subject = ''
            })
        }
    }

    # The daily canary mail, at noon, only when there are links to send.
    $canary = @($CanaryLinks)
    if ($canary.Count -gt 0) {
        $rng3 = New-Rng $Day 3
        $links = @()
        foreach ($u in $canary) { $links += New-LinkRecord $rng3 ([string]$u) $false }
        $plan.Add([pscustomobject]@{
            Id = ('d{0}-canary' -f $dd); Kind = 'canary'; Minute = $CanaryMinute
            Links = $links; SamplePath = $null; Subject = ('Soak day {0} canary' -f $dd)
        })
    }

    return , @($plan.ToArray() | Sort-Object -Property Minute, Id)
}

# --------------------------------------------------------------------------- mail files

function Get-MailDate {
    param([datetime]$Moment)
    $off = [System.TimeZoneInfo]::Local.GetUtcOffset($Moment)
    $sign = '+'
    if ($off -lt [System.TimeSpan]::Zero) { $sign = '-' }
    $off = $off.Duration()
    $text = $Moment.ToString('ddd, dd MMM yyyy HH:mm:ss', [System.Globalization.CultureInfo]::InvariantCulture)
    return $text + ' ' + $sign + ('{0:D2}{1:D2}' -f $off.Hours, $off.Minutes)
}

function New-MailText {
    param($Entry, $Journalist, [datetime]$Moment)
    $id = '<soak-{0}-{1}-{2}@soak.example.invalid>' -f $Entry.Id, $Moment.ToString('yyyyMMddHHmmss'), [guid]::NewGuid().ToString('N').Substring(0, 8)
    $from = '{0} <{1}>' -f $Journalist.FullName, $Journalist.Email
    $date = Get-MailDate $Moment

    if ($Entry.Kind -eq 'sample') {
        # A saved mail goes out as it is, except for who sent it, when, and its
        # Message-ID: the same file is replayed daily and must not be "already handled".
        $raw = $Latin1.GetString([System.IO.File]::ReadAllBytes($Entry.SamplePath))
        $split = [regex]::Match($raw, "\r?\n\r?\n")
        if (-not $split.Success) { throw "sample mail has no header/body separator" }
        $head = $raw.Substring(0, $split.Index) + "`n"
        $body = $raw.Substring($split.Index + $split.Length)
        $head = [regex]::Replace($head, '(?im)^(?:Message-ID|Date|From):[^\n]*\n(?:[ \t][^\n]*\n)*', '')
        $head = $head.Replace("`r`n", "`n").TrimEnd("`n").Replace("`n", "`r`n")
        $body = $body.Replace("`r`n", "`n").Replace("`n", "`r`n")
        return ('From: {0}' -f $from) + "`r`n" + ('Date: {0}' -f $date) + "`r`n" + ('Message-ID: {0}' -f $id) + "`r`n" + $head + "`r`n`r`n" + $body
    }

    # Each link is its own numbered section with a slug line above it: that is
    # how the desk's mail is written, and it gives every job its own keyword.
    $lines = New-Object System.Collections.Generic.List[string]
    $lines.Add('From: ' + $from)
    $lines.Add('To: ingest@example.invalid')
    # No "gia"/"themata" words: the subject must not name a journalist.
    $lines.Add('Subject: ' + $Entry.Subject)
    $lines.Add('Date: ' + $date)
    $lines.Add('Message-ID: ' + $id)
    $lines.Add('MIME-Version: 1.0')
    $lines.Add('Content-Type: text/plain; charset=utf-8')
    $lines.Add('Content-Transfer-Encoding: 8bit')
    $lines.Add('')
    $lines.Add($Greeting + ',')
    $lines.Add('')
    $n = 0
    foreach ($link in $Entry.Links) {
        $n++
        $lines.Add(('{0}. {1}' -f $n, $link.Title))
        $lines.Add($link.Url)
        $lines.Add('')
    }
    return ([string]::Join("`r`n", $lines.ToArray()))
}

# --------------------------------------------------------------------------- state

function Read-State {
    param([string]$Path, [datetime]$Today)
    if (Test-Path -LiteralPath $Path) {
        $s = Get-Content -LiteralPath $Path -Raw -Encoding UTF8 | ConvertFrom-Json
        # The start date is kept as yyyyMMdd text: PowerShell 7 would turn an
        # ISO date string into a DateTime on load.
        return [pscustomobject]@{ start = [string](Get-Prop $s 'start'); entries = @(Get-Prop $s 'entries') }
    }
    return [pscustomobject]@{ start = $Today.ToString('yyyyMMdd'); entries = @() }
}

function Save-State {
    param([string]$Path, $State)
    Save-TextAtomic -Path $Path -Text (ConvertTo-Json -InputObject $State -Depth 5)
}

function Test-EntryDone {
    param($Entries, [string]$Id)
    foreach ($e in $Entries) {
        if ($e.id -eq $Id) { return ([bool]$e.ok -or [int]$e.attempts -ge $MaxAttempts) }
    }
    return $false
}

# --------------------------------------------------------------------------- delivered files

function ConvertTo-Fps {
    param([string]$Rate)
    if ($Rate -match '^(\d+)/(\d+)$') {
        $den = [double]$Matches[2]
        if ($den -eq 0) { return 0.0 }
        return ([double]$Matches[1]) / $den
    }
    return 0.0
}

function Test-DeliveredFile {
    param([string]$Ffprobe, [string]$Path)
    # The expectations are verify.rs's: one mpeg2video stream, 1920x1080, 25 fps,
    # yuv422p, top field first (tt or tb), and eight mono pcm_s24le 48 kHz tracks.
    $r = Invoke-Tool -FilePath $Ffprobe -Arguments ('-v error -print_format json -show_format -show_streams "{0}"' -f $Path) -TimeoutMs $ProbeTimeoutMs
    if ($r.TimedOut) { return @{ Pass = $false; Reason = 'ffprobe timed out' } }
    if ($r.ExitCode -ne 0) {
        $first = ($r.Err -split "`n")[0].Trim()
        if ($first.Length -gt 120) { $first = $first.Substring(0, 120) }
        return @{ Pass = $false; Reason = ('ffprobe exit {0}: {1}' -f $r.ExitCode, $first) }
    }
    try { $json = $r.Out | ConvertFrom-Json } catch { return @{ Pass = $false; Reason = 'ffprobe output is not JSON' } }

    $fail = New-Object System.Collections.Generic.List[string]
    $streams = @(Get-Prop $json 'streams')
    $format = [string](Get-Prop (Get-Prop $json 'format') 'format_name')
    if ($format -notlike '*mxf*') { $fail.Add("container '$format' is not mxf") }

    $videos = @($streams | Where-Object { (Get-Prop $_ 'codec_type') -eq 'video' })
    if ($videos.Count -ne 1) { $fail.Add("video streams $($videos.Count), expected 1") }
    if ($videos.Count -ge 1) {
        $v = $videos[0]
        $codec = [string](Get-Prop $v 'codec_name')
        if ($codec -ne 'mpeg2video') { $fail.Add("video codec '$codec', expected mpeg2video") }
        $w = [string](Get-Prop $v 'width'); $h = [string](Get-Prop $v 'height')
        if (($w -ne '1920') -or ($h -ne '1080')) { $fail.Add("size ${w}x${h}, expected 1920x1080") }
        $rate = [string](Get-Prop $v 'r_frame_rate')
        if ([math]::Abs((ConvertTo-Fps $rate) - 25.0) -ge 0.001) { $fail.Add("frame rate '$rate', expected 25/1") }
        $pix = [string](Get-Prop $v 'pix_fmt')
        if ($pix -ne 'yuv422p') { $fail.Add("pix_fmt '$pix', expected yuv422p") }
        $order = [string](Get-Prop $v 'field_order')
        if (($order -ne 'tt') -and ($order -ne 'tb')) {
            if (-not $order) { $order = 'unset' }
            $fail.Add("field_order '$order', expected tt")
        }
    }

    $audios = @($streams | Where-Object { (Get-Prop $_ 'codec_type') -eq 'audio' })
    if ($audios.Count -ne 8) { $fail.Add("audio streams $($audios.Count), expected 8") }
    $i = 0
    foreach ($a in $audios) {
        $i++
        $ac = [string](Get-Prop $a 'codec_name')
        $ch = [string](Get-Prop $a 'channels')
        $sr = [string](Get-Prop $a 'sample_rate')
        if ($ac -ne 'pcm_s24le') { $fail.Add("audio$i codec '$ac', expected pcm_s24le") }
        if ($ch -ne '1') { $fail.Add("audio$i channels '$ch', expected 1") }
        if ($sr -ne '48000') { $fail.Add("audio$i sample rate '$sr', expected 48000") }
    }

    if ($fail.Count -gt 0) { return @{ Pass = $false; Reason = ([string]::Join('; ', $fail.ToArray())) } }
    return @{ Pass = $true; Reason = '' }
}

function ConvertTo-CsvField {
    param([string]$Value)
    if ($Value -match '[",\r\n]') { return '"' + $Value.Replace('"', '""') + '"' }
    return $Value
}

# =========================================================================== refusals

if (-not (Test-Path -LiteralPath $Root -PathType Container)) { Stop-Refused "-Root '$Root' does not exist or is not a folder" }
if ($LinksPerDay -lt 0) { Stop-Refused "-LinksPerDay $LinksPerDay is negative" }
$RootFull = (Resolve-Path -LiteralPath $Root).ProviderPath.TrimEnd('\')
if ($RootFull.StartsWith('\\')) { Stop-Refused "-Root '$RootFull' is a network path; the soak root must be on a local drive" }
$RootSep = Get-DirWithSeparator $RootFull

$dir = $RootFull
while ($dir) {
    # A repository above the root would put the soak's database and mail next to real work.
    if (Test-Path -LiteralPath (Join-Path $dir '.git')) { Stop-Refused "'$dir' holds .git; the soak root must be outside any repository" }
    $parent = Split-Path -Parent $dir
    if ((-not $parent) -or ($parent -eq $dir)) { break }
    $dir = $parent
}

$ConfigPath = Join-Path $RootFull 'config.json'
if (-not (Test-Path -LiteralPath $ConfigPath -PathType Leaf)) { Stop-Refused "'$ConfigPath' is missing" }
$config = $null
try { $config = Get-Content -LiteralPath $ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json } catch { Stop-Refused "'$ConfigPath' is not valid JSON" }
if ($config -isnot [System.Management.Automation.PSCustomObject]) { Stop-Refused "'$ConfigPath' is not a JSON object" }

# The soak must never read a real mailbox: Graph is the only mailbox source, and
# the old IMAP keys are refused too in case a copied config still carries them.
$graph = Get-Prop $config 'graph'
foreach ($key in @('tenant_id', 'client_id', 'mailbox')) {
    $value = Get-Prop $graph $key
    if (($null -ne $value) -and (([string]$value).Trim() -ne '')) { Stop-Refused "config.json has graph.$key set; blank every mailbox setting for a soak" }
}
foreach ($p in $config.PSObject.Properties) {
    if (($p.Name -match '^(imap_|email_(?!poll_interval_secs$))') -and ($null -ne $p.Value) -and (([string]$p.Value).Trim() -ne '')) {
        Stop-Refused "config.json has '$($p.Name)' set; blank every mailbox setting for a soak"
    }
}

# The watchfolder is the only place the prune step deletes from: it must be ours.
$watch = Get-Prop $config 'watchfolder_path'
if (($watch -isnot [string]) -or ($watch.Trim() -eq '')) { Stop-Refused 'config.json has no watchfolder_path' }
if ($watch.StartsWith('\\') -or $watch.StartsWith('//')) { Stop-Refused "watchfolder '$watch' is a network path" }
if ($watch -notmatch '^[A-Za-z]:[\\/]') { Stop-Refused "watchfolder '$watch' is not an absolute path" }
$WatchFull = [System.IO.Path]::GetFullPath($watch).TrimEnd('\')
$WatchSep = Get-DirWithSeparator $WatchFull
if ((-not $WatchSep.StartsWith($RootSep, [System.StringComparison]::OrdinalIgnoreCase)) -or ($WatchSep.Length -eq $RootSep.Length)) {
    Stop-Refused "watchfolder '$WatchFull' is not inside the soak root '$RootFull'"
}
if (Test-Path -LiteralPath $WatchFull) {
    if ((Get-Item -LiteralPath $WatchFull -Force).Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
        Stop-Refused "watchfolder '$WatchFull' is a link; it could point outside the soak root"
    }
}

if (-not $Binary) { $Binary = Join-Path $RootFull 'omni-ingest.exe' }
if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) { Stop-Refused "binary '$Binary' is missing" }
$BinaryFull = (Resolve-Path -LiteralPath $Binary).ProviderPath
if ($BinaryFull -like '*\target\release\*') { Stop-Refused "binary '$BinaryFull' is under target\release, the live daemon's" }

$RepoRoot = Split-Path -Parent $PSScriptRoot
if (-not $SampleDir) { $SampleDir = Join-Path $RepoRoot 'temp\dev\mails' }
if (-not $CorpusSource) { $CorpusSource = Join-Path $RepoRoot 'crates\omni-core\src\benchmark.rs' }

# =========================================================================== seed

$SeedPath = Join-Path (Join-Path $RootFull 'data') 'journalists.seed.json'
if ($WriteSeed) {
    if (Test-Path -LiteralPath $SeedPath) {
        Write-Host "soak_feed: $SeedPath exists; not overwritten"
        exit 0
    }
    $roster = @()
    for ($n = 1; $n -le $JournalistCount; $n++) {
        $j = Get-SoakJournalist $n
        $roster += [pscustomobject]@{ surname = $j.Surname; full_name = $j.FullName; emails = @($j.Email) }
    }
    if ($DryRun) {
        Write-Host "soak_feed: dry run: would write $JournalistCount journalists to $SeedPath"
        exit 0
    }
    [void](New-Item -ItemType Directory -Force -Path (Split-Path -Parent $SeedPath))
    Save-TextAtomic -Path $SeedPath -Text (ConvertTo-Json -InputObject $roster -Depth 4)
    Write-Host "soak_feed: wrote $JournalistCount journalists to $SeedPath (read when the soak database is first created)"
    exit 0
}

# =========================================================================== the pass

$StatePath = Join-Path $RootFull 'soak_state.json'
$CsvPath = Join-Path $RootFull 'soak_log.csv'
$script:LogPath = Join-Path $RootFull 'soak_feed.log'
$LockPath = Join-Path $RootFull 'soak_feed.lock'

$lock = $null
if (-not $DryRun) {
    # Passes can overlap when a mail-ingest is slow; the OS drops the lock if we die.
    try {
        $lock = New-Object System.IO.FileStream($LockPath, [System.IO.FileMode]::OpenOrCreate, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
    } catch {
        Write-Log 'another pass is still running; this one does nothing'
        exit 0
    }
}

try {
    $now = Get-Date
    $state = Read-State -Path $StatePath -Today $now.Date
    $startDate = [datetime]::ParseExact($state.start, 'yyyyMMdd', [System.Globalization.CultureInfo]::InvariantCulture)
    $dayIndex = [int][math]::Round(($now.Date - $startDate.Date).TotalDays)
    $freeGB = Get-FreeGB $RootFull
    $sent = 0; $sendFailed = 0; $due = 0; $later = 0
    $checked = 0; $failedChecks = 0; $pruned = 0

    if (-not (Test-Path -LiteralPath $SeedPath)) {
        Write-Log "warning: $SeedPath is missing; mail is attributed to MCR unless the database already has the soak journalists"
    }

    # ---- steps 1-3: plan and send
    $canSend = $true
    $planned = @()
    if ($dayIndex -lt 0) {
        Write-Log "start date $($state.start) is in the future; nothing is sent"
        $canSend = $false
    } elseif ($dayIndex -ge $Days) {
        Write-Log "soak window over (day $dayIndex of $Days); nothing is sent"
        $canSend = $false
    } elseif (($freeGB -lt 0) -or ($freeGB -lt $MinFreeGB)) {
        Write-Log "free space on the soak drive is $freeGB GB (under $MinFreeGB GB or unknown); nothing is sent this pass"
        $canSend = $false
    }

    if (($dayIndex -ge 0) -and ($dayIndex -lt $Days)) {
        if (-not (Test-Path -LiteralPath $CorpusSource -PathType Leaf)) { throw "corpus source '$CorpusSource' not found" }
        $corpus = Read-Corpus $CorpusSource
        if ($corpus.Count -eq 0) { throw "no corpus links found in '$CorpusSource'" }
        $samples = @()
        if (Test-Path -LiteralPath $SampleDir -PathType Container) {
            $samples = @(Get-ChildItem -LiteralPath $SampleDir -Filter '*.eml' -File | Sort-Object -Property Name | ForEach-Object { $_.FullName })
        } else {
            Write-Log 'no sample mail folder; sample mails skipped'
        }
        $canaryPath = Join-Path $RootFull 'soak_canary_links.txt'
        $canaryLinks = Get-CanaryLinks -Path $canaryPath
        $planned = Get-DayPlan -Day $dayIndex -Corpus $corpus -SampleFiles $samples -CanaryLinks $canaryLinks -LinksPerDay $LinksPerDay
        $journalist = Get-SoakJournalist (($dayIndex % $JournalistCount) + 1)
        $nowMinute = $now.Hour * 60 + $now.Minute

        if ($DryRun) {
            $xf = @($corpus | Where-Object { $_.ExpectedFail }).Count
            $effective = $corpus.Count
            if (($LinksPerDay -gt 0) -and ($LinksPerDay -lt $corpus.Count)) { $effective = $LinksPerDay }
            $xf = @($planned | ForEach-Object { $_.Links } | Where-Object { $_.ExpectedFail }).Count
            $canarySource = 'no canary links file'
            if (@($canaryLinks).Count -gt 0) { $canarySource = "canary links from $canaryPath" }
            Write-Host ("plan day {0}: {1} mails, {2} corpus links per day (LinksPerDay {3}; corpus {4}), {5} expected to fail, journalist {6}; {7}" -f $dayIndex, $planned.Count, $effective, $LinksPerDay, $corpus.Count, $xf, $journalist.Surname, $canarySource)
            foreach ($e in $planned) {
                $xfe = @($e.Links | Where-Object { $_.ExpectedFail }).Count
                Write-Host ("  {0}  {1,-8} {2,-9} links={3} expected_fail={4}" -f (Format-Minute $e.Minute), $e.Kind, $e.Id, @($e.Links).Count, $xfe)
            }
        }

        $dueEntries = @($planned | Where-Object { ($_.Minute -le $nowMinute) -and (-not (Test-EntryDone $state.entries $_.Id)) })
        $later = @($planned | Where-Object { ($_.Minute -gt $nowMinute) -and (-not (Test-EntryDone $state.entries $_.Id)) }).Count
        $due = $dueEntries.Count

        if ($canSend) {
            $batch = @($dueEntries | Select-Object -First $MaxMailsPerPass)
            if ($DryRun) {
                foreach ($e in $batch) { Write-Host ("would send {0} ({1}, planned {2}) as {3}" -f $e.Id, $e.Kind, (Format-Minute $e.Minute), $journalist.Email) }
                if ($batch.Count -eq 0) { Write-Host 'would send nothing this pass' }
            } else {
                $mailDir = Join-Path (Join-Path $RootFull 'soak_mail') ('day{0:D2}' -f $dayIndex)
                [void](New-Item -ItemType Directory -Force -Path $mailDir)
                foreach ($e in $batch) {
                    $emlPath = Join-Path $mailDir ($e.Id + '.eml')
                    if (-not (Test-Path -LiteralPath $emlPath)) {
                        # A retry reuses the file, so its Message-ID is the same and ingest stays idempotent.
                        $text = New-MailText -Entry $e -Journalist $journalist -Moment (Get-Date)
                        [System.IO.File]::WriteAllText($emlPath, $text, $(if ($e.Kind -eq 'sample') { $Latin1 } else { $Utf8NoBom }))
                    }
                    $args1 = '--root "{0}" --config "{1}" mail-ingest --eml "{2}"' -f $RootFull, $ConfigPath, $emlPath
                    $r = Invoke-Tool -FilePath $BinaryFull -Arguments $args1 -TimeoutMs $IngestTimeoutMs
                    $jobs = 0
                    $ok = $false
                    $note = ''
                    if ($r.TimedOut) {
                        $note = 'timed out'
                    } elseif ($r.ExitCode -ne 0) {
                        $note = "exit $($r.ExitCode)"
                    } elseif ($r.Out -match 'not queued') {
                        $note = 'not queued'
                    } elseif ($r.Out -match '(\d+) job\(s\)') {
                        $ok = $true; $jobs = [int]$Matches[1]
                    } elseif ($r.Out -match 'already handled') {
                        $ok = $true
                    } else {
                        $note = 'unrecognised output'
                    }
                    [System.IO.File]::WriteAllText(($emlPath + '.out.txt'), ($r.Out + $r.Err), $Utf8NoBom)

                    $prev = $null
                    foreach ($x in $state.entries) { if ($x.id -eq $e.Id) { $prev = $x } }
                    $attempts = 1
                    if ($null -ne $prev) { $attempts = [int]$prev.attempts + 1 }
                    $record = [pscustomobject]@{
                        id = $e.Id; day = $dayIndex; kind = $e.Kind; planned = (Format-Minute $e.Minute)
                        links = @($e.Links).Count; expected_fail = @($e.Links | Where-Object { $_.ExpectedFail }).Count
                        attempts = $attempts; ok = $ok; exit = $r.ExitCode; jobs = $jobs
                        at = (Get-Date).ToString('s')
                    }
                    $state.entries = @(@($state.entries | Where-Object { $_.id -ne $e.Id }) + $record)
                    Save-State -Path $StatePath -State $state
                    if ($ok) {
                        $sent++
                        Write-Log ("sent {0} ({1} links, {2} jobs) as {3}" -f $e.Id, @($e.Links).Count, $jobs, $journalist.Surname)
                    } else {
                        $sendFailed++
                        Write-Log ("send of {0} failed ({1}); attempt {2} of {3}" -f $e.Id, $note, $attempts, $MaxAttempts)
                    }
                }
                $due = $due - $sent
            }
        }
        if ((-not $DryRun) -and (-not (Test-Path -LiteralPath $StatePath))) { Save-State -Path $StatePath -State $state }
    }

    # ---- step 4: delivered files
    if (Test-Path -LiteralPath $WatchFull -PathType Container) {
        $seen = @{}
        if (Test-Path -LiteralPath $CsvPath) {
            foreach ($row in @(Import-Csv -LiteralPath $CsvPath -Encoding UTF8)) { $seen[('{0}|{1}' -f $row.file, $row.bytes)] = $true }
        }
        $ffprobe = $null
        foreach ($candidate in @((Join-Path $RootFull 'bin\ffprobe.exe'), (Join-Path (Split-Path -Parent $BinaryFull) 'bin\ffprobe.exe'))) {
            if ((-not $ffprobe) -and (Test-Path -LiteralPath $candidate -PathType Leaf)) { $ffprobe = $candidate }
        }
        # Only *.mxf directly in the watchfolder; the extension is re-checked because
        # a legacy filter would also match names like x.mxf1 or x.mxf.tmp.
        $files = @(Get-ChildItem -LiteralPath $WatchFull -File -Force | Where-Object { $_.Extension -ieq '.mxf' })
        $settled = [datetime]::Now.AddSeconds(-$SettleSeconds)
        foreach ($f in $files) {
            $key = '{0}|{1}' -f $f.Name, $f.Length
            if ($seen.ContainsKey($key) -or ($f.LastWriteTime -gt $settled) -or ($f.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) { continue }
            if (-not $ffprobe) { Write-Log 'ffprobe not found in <root>\bin or <binary dir>\bin; delivered files are not checked'; break }
            if ($DryRun) { Write-Host ("would check {0} ({1} bytes)" -f $f.Name, $f.Length); continue }
            $verdict = Test-DeliveredFile -Ffprobe $ffprobe -Path $f.FullName
            $word = 'FAIL'
            if ($verdict.Pass) { $word = 'PASS' }
            $line = ((@((Get-Date).ToString('s'), $f.Name, [string]$f.Length, $word, $verdict.Reason) | ForEach-Object { ConvertTo-CsvField $_ }) -join ',')
            if (-not (Test-Path -LiteralPath $CsvPath)) { [System.IO.File]::WriteAllText($CsvPath, "time,file,bytes,result,reason`r`n", $Utf8NoBom) }
            [System.IO.File]::AppendAllText($CsvPath, $line + "`r`n", $Utf8NoBom)
            $seen[$key] = $true
            $checked++
            if (-not $verdict.Pass) { $failedChecks++; Write-Log ("delivered file {0} FAILED: {1}" -f $f.Name, $verdict.Reason) }
        }

        # Prune: only checked files, only older than an hour, only directly in the watchfolder.
        $cutoff = [datetime]::Now.AddHours(-$PruneAfterHours)
        foreach ($f in $files) {
            $key = '{0}|{1}' -f $f.Name, $f.Length
            if ((-not $seen.ContainsKey($key)) -or ($f.LastWriteTime -gt $cutoff) -or ($f.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) { continue }
            if ((Get-DirWithSeparator $f.DirectoryName) -ne $WatchSep) { continue }
            if ($DryRun) { Write-Host ("would delete {0}" -f $f.Name); continue }
            Remove-Item -LiteralPath $f.FullName -Force
            $pruned++
        }
    } else {
        Write-Log "watchfolder '$WatchFull' does not exist yet; nothing to check"
    }

    # ---- step 5: summary
    $summary = 'pass day={0} sent={1} send_failed={2} pending={3} later={4} checked={5} failed={6} pruned={7} free_gb={8}' -f $dayIndex, $sent, $sendFailed, $due, $later, $checked, $failedChecks, $pruned, $freeGB.ToString([System.Globalization.CultureInfo]::InvariantCulture)
    if ($DryRun) { Write-Host "dry run: $summary (nothing written)" } else { Write-Log $summary }
    exit 0
} catch {
    Write-Log ("ERROR: {0}" -f $_.Exception.Message)
    exit 1
} finally {
    if ($null -ne $lock) { $lock.Dispose() }
}
