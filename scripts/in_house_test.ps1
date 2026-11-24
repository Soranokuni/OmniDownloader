# ==============================================================================
# OmniDownloader in-house suite (P14.2)
#
# Runs a scratch daemon (its own root under $env:TEMP, its own config, database,
# watchfolder and loopback port) and sends it one generated clip per case as a
# mail attachment, the way a journalist would. Every delivered file is checked
# with ffprobe against the on-air format. Then the daemon is killed by its PID
# in the middle of an encode, started again, and the job must be delivered
# exactly once.
#
# Never the live daemon: a binary under target\release is refused, and only
# the PIDs this script started are ever stopped.
#
#   powershell -ExecutionPolicy Bypass -File .\scripts\in_house_test.ps1
#   powershell -ExecutionPolicy Bypass -File .\scripts\in_house_test.ps1 -Binary C:\build\release\omni-ingest.exe
# ==============================================================================
param (
    # The build under test. Relative paths resolve against the repository root.
    [string]$Binary = "target\gate\release\omni-ingest.exe",
    # Where ffmpeg, ffprobe, bmxtranswrap and yt-dlp are copied from (read only).
    [string]$BinDir = "bin",
    # Minutes allowed for all the cases, and again for the kill-and-restart job.
    [int]$TimeoutMinutes = 20,
    # Keep the scratch root even when everything passed.
    [switch]$Keep
)

$ErrorActionPreference = "Stop"
$RootDir = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Definition)

function Resolve-FromRoot([string]$p) {
    if ([System.IO.Path]::IsPathRooted($p)) { return [System.IO.Path]::GetFullPath($p) }
    return [System.IO.Path]::GetFullPath((Join-Path $RootDir $p))
}

$Binary = Resolve-FromRoot $Binary
$BinDir = Resolve-FromRoot $BinDir

# ------------------------------------------------------------------------------
# Refusals: the live daemon runs from target\release.
# ------------------------------------------------------------------------------
if ($Binary -match '\\target\\release\\omni-ingest\.exe$') {
    Write-Host "REFUSED: $Binary is the live daemon's binary (target\release)." -ForegroundColor Red
    Write-Host "Build the one under test elsewhere, e.g. cargo build --release --target-dir target/gate, and pass -Binary." -ForegroundColor Red
    exit 2
}
if (-not (Test-Path -LiteralPath $Binary)) {
    Write-Host "Binary not found: $Binary (cargo build --release --target-dir target/gate)" -ForegroundColor Red
    exit 2
}
$running = Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Path -and ($_.Path -ieq $Binary) }
if ($running) {
    Write-Host "REFUSED: $Binary is running now (PID $(@($running.Id) -join ', ')); test a copy that nothing runs." -ForegroundColor Red
    exit 2
}
foreach ($tool in @("ffmpeg.exe", "ffprobe.exe", "bmxtranswrap.exe", "yt-dlp.exe")) {
    if (-not (Test-Path -LiteralPath (Join-Path $BinDir $tool))) {
        Write-Host "Missing tool: $(Join-Path $BinDir $tool)" -ForegroundColor Red
        exit 2
    }
}

# ------------------------------------------------------------------------------
# Scratch instance
# ------------------------------------------------------------------------------
$Scratch = Join-Path $env:TEMP ("omni-inhouse-" + (Get-Date -Format "yyyyMMdd-HHmmss") + "-" + $PID)
$ScratchBin = Join-Path $Scratch "bin"
$Watch = Join-Path $Scratch "watchfolder"
$Cases = Join-Path $Scratch "cases"
foreach ($d in @($Scratch, $ScratchBin, (Join-Path $Scratch "data"), $Watch, $Cases)) {
    New-Item -ItemType Directory -Path $d -Force | Out-Null
}
# A copy, so nothing the daemon does (a yt-dlp update) can touch the real bin\.
foreach ($tool in @("ffmpeg.exe", "ffprobe.exe", "bmxtranswrap.exe", "yt-dlp.exe")) {
    Copy-Item -LiteralPath (Join-Path $BinDir $tool) -Destination $ScratchBin
}
$Ffmpeg = Join-Path $ScratchBin "ffmpeg.exe"
$Ffprobe = Join-Path $ScratchBin "ffprobe.exe"

$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
$listener.Start()
$Port = $listener.LocalEndpoint.Port
$listener.Stop()
$Base = "http://127.0.0.1:$Port"

$ConfigPath = Join-Path $Scratch "config.json"
$cfg = [ordered]@{
    database_path               = "data/omni.db"
    watchfolder_path            = $Watch
    temp_path                   = "temp"
    bin_dir                     = "bin"
    web_port                    = $Port
    web_host                    = "127.0.0.1"
    max_concurrent_downloads    = 2
    max_concurrent_transcodes   = 2
    ytdl_auto_update_nightly    = $false
    adblock_enabled             = $false
    adblock_auto_update_nightly = $false
    security                    = [ordered]@{ mcr_open_networks = @("127.0.0.1/32", "::1/128") }
    llm                         = [ordered]@{ mode = "off" }
    repair                      = [ordered]@{ mode = "off" }
}
# UTF-8 without a BOM: Windows PowerShell's "UTF8" writes one, and the JSON reader refuses it.
$NoBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText($ConfigPath, ($cfg | ConvertTo-Json -Depth 5), $NoBom)

# An invented reporter, so the mails resolve to a journalist.
$Sender = "reporter@inhouse.example"
[System.IO.File]::WriteAllText((Join-Path $Scratch "data\journalists.seed.json"),
    ('[{"surname":"INHOUSE","full_name":"In-house Test Reporter","emails":["' + $Sender + '"],"aliases":[],"default_priority":0}]'), $NoBom)

Write-Host "======================================================================" -ForegroundColor Cyan
Write-Host " OmniDownloader in-house suite" -ForegroundColor Cyan
Write-Host "======================================================================" -ForegroundColor Cyan
Write-Host "Binary:  $Binary"
Write-Host "Scratch: $Scratch"
Write-Host "Web:     $Base"
Write-Host ""

$results = New-Object System.Collections.ArrayList
function Add-Result([string]$name, [bool]$ok, [string]$detail) {
    [void]$results.Add([pscustomobject]@{ Name = $name; Ok = $ok; Detail = $detail })
    if ($ok) { Write-Host "[PASS] $name" -ForegroundColor Green }
    else { Write-Host "[FAIL] $name -- $detail" -ForegroundColor Red }
}

. (Join-Path $PSScriptRoot "in_house_lib.ps1")

$daemon = $null
try {
    # --------------------------------------------------------------------------
    # The cases
    # --------------------------------------------------------------------------
    # Keys are letters only: the keyword (and so the file name) drops digits,
    # and two cases with one keyword would be delivered as NAME and NAME_2.
    $caseList = @(
        @{ Key = "TWENTYFIVEP";  Name = "25p";      Vf = "testsrc2=size=1920x1080:rate=25";  Audio = "stereo"; Extra = @() },
        @{ Key = "THIRTYP";      Name = "30p";      Vf = "testsrc2=size=1280x720:rate=30";   Audio = "stereo"; Extra = @() },
        @{ Key = "TWENTYFIVEI";  Name = "25i";      Vf = "testsrc2=size=1920x1080:rate=50,tinterlace=mode=interleave_top,setfield=tff"; Audio = "stereo"; Extra = @("-flags", "+ildct+ilme", "-x264-params", "tff=1") },
        @{ Key = "FIFTYP";       Name = "50p";      Vf = "testsrc2=size=1920x1080:rate=50";  Audio = "stereo"; Extra = @() },
        @{ Key = "MONO";         Name = "mono";     Vf = "testsrc2=size=1920x1080:rate=25";  Audio = "mono";   Extra = @() },
        @{ Key = "SURROUND";     Name = "5.1";      Vf = "testsrc2=size=1920x1080:rate=25";  Audio = "5.1";    Extra = @() },
        @{ Key = "NOAUDIO";      Name = "no audio"; Vf = "testsrc2=size=1920x1080:rate=25";  Audio = "none";   Extra = @() }
    )
    $seconds = 6
    $emls = @()
    foreach ($c in $caseList) {
        $clip = Join-Path $Cases ($c.Key.ToLower() + ".mp4")
        New-Clip -Ffmpeg $Ffmpeg -Out $clip -Vf $c.Vf -Audio $c.Audio -Seconds $seconds -Extra $c.Extra
        $emls += New-Eml -Dir $Cases -Clip $clip -Key $c.Key -Sender $Sender
    }
    # The long clip for kill-and-restart: long enough to be caught mid-encode.
    $longClip = Join-Path $Cases "restart.mp4"
    New-Clip -Ffmpeg $Ffmpeg -Out $longClip -Vf "testsrc2=size=1920x1080:rate=50" -Audio "stereo" -Seconds 120 -Extra @()
    $longEml = New-Eml -Dir $Cases -Clip $longClip -Key "RESTART" -Sender $Sender

    $daemon = Start-Daemon -Binary $Binary -Scratch $Scratch -Config $ConfigPath -Base $Base -Tag "1"

    $ingest = Invoke-MailIngest -Binary $Binary -Scratch $Scratch -Config $ConfigPath -Emls $emls
    Write-Host $ingest

    $deadline = (Get-Date).AddMinutes($TimeoutMinutes)
    $jobs = Wait-Jobs -Base $Base -Count $caseList.Count -Deadline $deadline
    for ($i = 0; $i -lt $caseList.Count; $i++) {
        $c = $caseList[$i]
        # The job's address names its mail, and the mail's Message-ID names the case.
        $job = $jobs | Where-Object { $_.url -like ("attachment://inhouse-" + $c.Key + "-*") } | Select-Object -First 1
        if (-not $job) { Add-Result "case $($c.Name)" $false "no job from mail $($c.Key)"; continue }
        if ($job.status -ne "COMPLETED") {
            Add-Result "case $($c.Name)" $false "job #$($job.id) ended $($job.status) $($job.error_code): $($job.error_message)"
            continue
        }
        $problems = Test-OnAir -Ffprobe $Ffprobe -File $job.file_path -Watch $Watch -SourceSeconds $seconds
        Add-Result "case $($c.Name) (job #$($job.id), $(Split-Path -Leaf $job.file_path))" ($problems.Count -eq 0) ($problems -join "; ")
    }

    # --------------------------------------------------------------------------
    # Kill and restart in the middle of an encode
    # --------------------------------------------------------------------------
    $before = @(Get-ChildItem -LiteralPath $Watch -Recurse -File).Count
    $ingest = Invoke-MailIngest -Binary $Binary -Scratch $Scratch -Config $ConfigPath -Emls @($longEml)
    Write-Host $ingest
    $restartId = $caseList.Count + 1
    $caught = Wait-Stage -Base $Base -Id $restartId -Stage "TRANSCODE" -Deadline (Get-Date).AddMinutes(5)
    if (-not $caught) {
        Add-Result "kill and restart" $false "job #$restartId never reached TRANSCODE while the daemon ran"
    } else {
        Start-Sleep -Seconds 3
        Stop-Process -Id $daemon.Id -Force
        $daemon.WaitForExit(30000) | Out-Null
        Write-Host "Killed the scratch daemon (PID $($daemon.Id)) during job #$restartId's encode."
        $daemon = $null
        Start-Sleep -Seconds 2
        # The encoder dies with the daemon (job-object tree kill).
        $orphans = @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($ScratchBin, [System.StringComparison]::OrdinalIgnoreCase) })
        foreach ($o in $orphans) { Stop-Process -Id $o.Id -Force -ErrorAction SilentlyContinue }
        $early = @(Get-ChildItem -LiteralPath $Watch -Recurse -File).Count - $before

        $daemon = Start-Daemon -Binary $Binary -Scratch $Scratch -Config $ConfigPath -Base $Base -Tag "2"
        $done = Wait-Jobs -Base $Base -Count $restartId -Deadline (Get-Date).AddMinutes($TimeoutMinutes)
        $job = $done | Where-Object { $_.id -eq $restartId }
        $files = @(Get-ChildItem -LiteralPath $Watch -Recurse -File)
        $mine = @($files | Where-Object { $_.BaseName -eq $job.slug -or $_.BaseName -like ($job.slug + "_*") })
        $tmps = @($files | Where-Object { $_.Name -like "*.tmp" })
        $extraJobs = Get-OmniJob -Base $Base -Id ($restartId + 1)
        $problems = @()
        if ($orphans.Count -gt 0) { $problems += "$($orphans.Count) encoder process(es) outlived the daemon" }
        if ($early -ne 0) { $problems += "$early file(s) reached the watchfolder before the restart" }
        if ($job.status -ne "COMPLETED") { $problems += "job ended $($job.status) $($job.error_code): $($job.error_message)" }
        if ($mine.Count -ne 1) { $problems += "$($mine.Count) files for $($job.slug) in the watchfolder: $(($mine | ForEach-Object Name) -join ', ')" }
        if ($tmps.Count -ne 0) { $problems += "left in the watchfolder: $(($tmps | ForEach-Object Name) -join ', ')" }
        if ($extraJobs) { $problems += "a job #$($restartId + 1) exists" }
        if ($job.status -eq "COMPLETED") {
            $problems += Test-OnAir -Ffprobe $Ffprobe -File $job.file_path -Watch $Watch -SourceSeconds 120
        }
        Add-Result "kill and restart (job #$restartId, delivered once)" ($problems.Count -eq 0) ($problems -join "; ")
    }
} catch {
    Add-Result "suite" $false "$_"
} finally {
    if ($daemon -and -not $daemon.HasExited) {
        Stop-Process -Id $daemon.Id -Force -ErrorAction SilentlyContinue
        $daemon.WaitForExit(30000) | Out-Null
    }
}

# ------------------------------------------------------------------------------
# Summary
# ------------------------------------------------------------------------------
$failed = @($results | Where-Object { -not $_.Ok }).Count
$passed = @($results | Where-Object { $_.Ok }).Count
Write-Host ""
Write-Host "Passed: $passed  Failed: $failed"
if ($failed -eq 0 -and -not $Keep) {
    Start-Sleep -Seconds 2
    Remove-Item -LiteralPath $Scratch -Recurse -Force -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $Scratch) { Write-Host "Could not remove all of $Scratch; delete it by hand." -ForegroundColor Yellow }
} else {
    Write-Host "Scratch root kept for inspection: $Scratch (daemon log under logs\, stdout\stderr in daemon_*.log)"
}
if ($failed -eq 0) { exit 0 } else { exit 1 }
