# Helpers for in_house_test.ps1 (P14.2): clips, mails, the scratch daemon, the
# job API and the on-air check. Dot-sourced; nothing here runs on its own.

# One test clip: a lavfi picture, a 1 kHz tone in the given layout (or none).
function New-Clip([string]$Ffmpeg, [string]$Out, [string]$Vf, [string]$Audio, [int]$Seconds, [string[]]$Extra) {
    $a = @("-v", "error", "-y", "-f", "lavfi", "-i", $Vf)
    if ($Audio -ne "none") { $a += @("-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000") }
    $a += @("-t", "$Seconds", "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p")
    $a += $Extra
    switch ($Audio) {
        "stereo" { $a += @("-c:a", "aac", "-ac", "2") }
        "mono" { $a += @("-c:a", "aac", "-ac", "1") }
        "5.1" { $a += @("-c:a", "aac", "-ac", "6") }
        default { $a += @("-an") }
    }
    $a += $Out
    & $Ffmpeg @a
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $Out)) { throw "ffmpeg could not make $Out (exit $LASTEXITCODE)" }
}

# A mail from the invented reporter with the clip attached; returns the .eml path.
function New-Eml([string]$Dir, [string]$Clip, [string]$Key, [string]$Sender) {
    $name = Split-Path -Leaf $Clip
    $b64 = [Convert]::ToBase64String([System.IO.File]::ReadAllBytes($Clip), [System.Base64FormattingOptions]::InsertLineBreaks)
    $nl = "`r`n"
    $text = "From: In-house Test Reporter <$Sender>$nl" +
        "To: ingest@inhouse.example$nl" +
        "Subject: Inhouse $Key$nl" +
        "Date: $((Get-Date).ToUniversalTime().ToString('r'))$nl" +
        "Message-ID: <inhouse-$Key-$([guid]::NewGuid().ToString('N'))@inhouse.example>$nl" +
        "MIME-Version: 1.0$nl" +
        "Content-Type: multipart/mixed; boundary=`"INHOUSE`"$nl$nl" +
        "--INHOUSE$nl" +
        "Content-Type: text/plain; charset=utf-8$nl$nl" +
        "In-house test clip $Key.$nl$nl" +
        "--INHOUSE$nl" +
        "Content-Type: video/mp4; name=`"$name`"$nl" +
        "Content-Disposition: attachment; filename=`"$name`"$nl" +
        "Content-Transfer-Encoding: base64$nl$nl" +
        $b64 + $nl +
        "--INHOUSE--$nl"
    $path = Join-Path $Dir ("mail_" + $Key.ToLower() + ".eml")
    [System.IO.File]::WriteAllText($path, $text, [System.Text.Encoding]::ASCII)
    return $path
}

# Start the scratch daemon and wait for its web server; returns the process.
function Start-Daemon([string]$Binary, [string]$Scratch, [string]$Config, [string]$Base, [string]$Tag) {
    $out = Join-Path $Scratch "daemon_${Tag}_out.log"
    $err = Join-Path $Scratch "daemon_${Tag}_err.log"
    $p = Start-Process -FilePath $Binary -ArgumentList "--root `"$Scratch`" --config `"$Config`" run" `
        -RedirectStandardOutput $out -RedirectStandardError $err -NoNewWindow -PassThru
    $until = (Get-Date).AddSeconds(120)
    while ((Get-Date) -lt $until) {
        if ($p.HasExited) {
            $p.WaitForExit()
            throw "the scratch daemon exited at start (code $($p.ExitCode)): $((Get-Content -LiteralPath $err -Tail 20 -ErrorAction SilentlyContinue) -join ' | ')"
        }
        try {
            Invoke-WebRequest -Uri "$Base/api/health" -UseBasicParsing -TimeoutSec 3 | Out-Null
            Write-Host "Scratch daemon $Tag up (PID $($p.Id))."
            return $p
        } catch {
            # A 503 (a check not green yet) still means the server answers.
            if ($_.Exception.Response) {
                Write-Host "Scratch daemon $Tag up (PID $($p.Id)), health not green yet."
                return $p
            }
            Start-Sleep -Milliseconds 500
        }
    }
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    throw "the scratch daemon did not answer on $Base within 120 s"
}

function Invoke-MailIngest([string]$Binary, [string]$Scratch, [string]$Config, [string[]]$Emls) {
    $a = @("--root", $Scratch, "--config", $Config, "mail-ingest")
    foreach ($e in $Emls) { $a += @("--eml", $e) }
    # Its log lines go to stderr; under "Stop" Windows PowerShell would call each one an error.
    $ErrorActionPreference = "Continue"
    $out = & $Binary @a 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { throw "mail-ingest failed (exit $LASTEXITCODE): $out" }
    return $out
}

# One job from the MCR API (open on loopback), or $null when there is none.
function Get-OmniJob([string]$Base, [int]$Id) {
    try {
        return (Invoke-RestMethod -Uri "$Base/api/jobs/$Id" -TimeoutSec 10).job
    } catch {
        if ($_.Exception.Response -and [int]$_.Exception.Response.StatusCode -eq 404) { return $null }
        throw
    }
}

$script:EndStates =@("COMPLETED", "COMPLETED_MANUAL", "REQUIRES_REVIEW", "FAILED", "CANCELLED", "AWAITING_APPROVAL", "MANUAL_DOWNLOAD")

# Jobs 1..Count once each has stopped moving (or at the deadline, as they are).
function Wait-Jobs([string]$Base, [int]$Count, [datetime]$Deadline) {
    while ($true) {
        $jobs = @()
        for ($id = 1; $id -le $Count; $id++) {
            $j = Get-OmniJob -Base $Base -Id $id
            if ($j) { $jobs += $j }
        }
        $open = @($jobs | Where-Object { $script:EndStates -notcontains $_.status })
        if (($jobs.Count -eq $Count -and $open.Count -eq 0) -or (Get-Date) -gt $Deadline) { return $jobs }
        Start-Sleep -Seconds 2
    }
}

# True once the job runs in the given stage; false when it ends first or time runs out.
function Wait-Stage([string]$Base, [int]$Id, [string]$Stage, [datetime]$Deadline) {
    while ((Get-Date) -lt $Deadline) {
        $j = Get-OmniJob -Base $Base -Id $Id
        if ($j) {
            if ($j.status -eq "RUNNING" -and $j.stage -eq $Stage) { return $true }
            if ($script:EndStates -contains $j.status) { return $false }
        }
        Start-Sleep -Milliseconds 200
    }
    return $false
}

# The delivered file against the on-air format; returns the problems found.
function Test-OnAir([string]$Ffprobe, [string]$File, [string]$Watch, [double]$SourceSeconds) {
    $p = @()
    if (-not $File -or -not (Test-Path -LiteralPath $File)) { return @("delivered file missing: $File") }
    if (-not ([System.IO.Path]::GetFullPath($File)).StartsWith([System.IO.Path]::GetFullPath($Watch), [System.StringComparison]::OrdinalIgnoreCase)) {
        $p += "delivered outside the watchfolder: $File"
    }
    $info = & $Ffprobe -v error -print_format json -show_streams -show_format $File | Out-String | ConvertFrom-Json
    if ($info.format.format_name -ne "mxf") { $p += "container $($info.format.format_name), not mxf" }
    $op = "$($info.format.tags.operational_pattern_ul)"
    if ($op -notmatch "0d010201\.01010[0-9a-f]00") { $p += "operational pattern $op is not OP1a" }
    $dur = [double]$info.format.duration
    if ([math]::Abs($dur - $SourceSeconds) -gt 0.5) { $p += "duration $dur s, source $SourceSeconds s" }

    $v = @($info.streams | Where-Object { $_.codec_type -eq "video" })
    if ($v.Count -ne 1) { $p += "$($v.Count) video streams"; return $p }
    $v = $v[0]
    if ($v.codec_name -ne "mpeg2video") { $p += "video codec $($v.codec_name)" }
    if ($v.profile -ne "4:2:2") { $p += "profile $($v.profile), not 4:2:2" }
    if ($v.width -ne 1920 -or $v.height -ne 1080) { $p += "picture $($v.width)x$($v.height)" }
    if ($v.pix_fmt -ne "yuv422p") { $p += "pixel format $($v.pix_fmt)" }
    if ($v.r_frame_rate -ne "25/1") { $p += "frame rate $($v.r_frame_rate)" }
    if ($v.field_order -ne "tt") { $p += "field order $($v.field_order), not tt (interlaced, top field first)" }
    if ([int64]$v.bit_rate -ne 50000000) { $p += "video bit rate $($v.bit_rate), not 50000000" }
    # Rec.709, all three labels (P14.9): in the MXF descriptor (stream) and in
    # the MPEG-2 bitstream (the decoded frames). Unset is a failure too.
    $frame = & $Ffprobe -v error -select_streams v:0 -read_intervals "%+#1" -show_entries frame=color_primaries,color_transfer,color_space -print_format json $File | Out-String | ConvertFrom-Json
    foreach ($k in @("color_primaries", "color_transfer", "color_space")) {
        if ("$($v.$k)" -ne "bt709") { $p += "stream $k '$($v.$k)', not bt709" }
        if ("$($frame.frames[0].$k)" -ne "bt709") { $p += "bitstream $k '$($frame.frames[0].$k)', not bt709" }
    }
    # GOP 12: the distance between successive I pictures in the first 4 s.
    $types = & $Ffprobe -v error -select_streams v:0 -read_intervals "%+4" -show_entries frame=pict_type -of csv=p=0 $File
    $iAt = @()
    $n = 0
    foreach ($t in $types) { if ("$t".Trim().TrimEnd(",") -eq "I") { $iAt += $n }; $n++ }
    if ($iAt.Count -lt 2) { $p += "fewer than two I pictures in the first 4 s" }
    for ($k = 1; $k -lt $iAt.Count; $k++) {
        if ($iAt[$k] - $iAt[$k - 1] -ne 12) { $p += "GOP $($iAt[$k] - $iAt[$k - 1]), not 12"; break }
    }

    $a = @($info.streams | Where-Object { $_.codec_type -eq "audio" })
    if ($a.Count -ne 8) { $p += "$($a.Count) audio tracks, not 8" }
    foreach ($s in $a) {
        if ($s.codec_name -ne "pcm_s24le" -or [int]$s.channels -ne 1 -or $s.sample_rate -ne "48000") {
            $p += "audio track $($s.index): $($s.codec_name) $($s.channels) ch $($s.sample_rate) Hz"
        }
    }
    return $p
}
