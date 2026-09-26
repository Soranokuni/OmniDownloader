# ==============================================================================
# OmniDownloader In-House Broadcast Robustness & Integrity Verification Suite
# ==============================================================================
param (
    [string]$BinaryPath = "target\release\omni-ingest.exe",
    [string]$BinDir = "bin",
    [int]$TestPort = 8089
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$RootDir = Split-Path -Parent $ScriptDir
Set-Location $RootDir

Write-Host "======================================================================" -ForegroundColor Cyan
Write-Host "   OMNIDOWNLOADER IN-HOUSE ROBUSTNESS & INTEGRITY TEST SUITE         " -ForegroundColor Cyan
Write-Host "======================================================================" -ForegroundColor Cyan
Write-Host "Time: $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')" -ForegroundColor Gray
Write-Host "Root Directory: $RootDir" -ForegroundColor Gray
Write-Host ""

$testsPassed = 0
$testsFailed = 0

function Assert-Step ($name, [scriptblock]$action) {
    Write-Host "[TEST] $name... " -NoNewline
    try {
        & $action
        Write-Host "PASSED" -ForegroundColor Green
        $script:testsPassed++
    } catch {
        Write-Host "FAILED" -ForegroundColor Red
        Write-Host "  Error: $_" -ForegroundColor DarkRed
        $script:testsFailed++
    }
}

# ------------------------------------------------------------------------------
# 1. Verify Compiled Release Binary
# ------------------------------------------------------------------------------
Assert-Step "Verify Production Release Binary" {
    if (-not (Test-Path $BinaryPath)) {
        throw "Binary not found at $BinaryPath. Run 'cargo build --release' first."
    }
    $verOutput = & $BinaryPath --version
    if (-not ($verOutput -match "omni-ingest")) {
        throw "Binary output did not contain 'omni-ingest': $verOutput"
    }
}

# ------------------------------------------------------------------------------
# 2. Verify Toolchain Binaries
# ------------------------------------------------------------------------------
$requiredTools = @("ffmpeg.exe", "ffprobe.exe", "bmxtranswrap.exe", "yt-dlp.exe")
foreach ($tool in $requiredTools) {
    Assert-Step "Verify Toolchain Binary: $tool" {
        $path = Join-Path $BinDir $tool
        if (-not (Test-Path $path)) {
            throw "Missing tool: $path"
        }
        $flag = if ($tool -match "ffmpeg|ffprobe") { "-version" } elseif ($tool -eq "yt-dlp.exe") { "--version" } else { "" }
        if ($flag) {
            $out = & $path $flag 2>&1
            if ($LASTEXITCODE -ne 0) {
                throw "Tool $tool exited with code $LASTEXITCODE"
            }
        }
    }
}

# ------------------------------------------------------------------------------
# 3. Test Broadcast Transcoding & Dalet RDD9 OP1a Pipeline
# ------------------------------------------------------------------------------
Assert-Step "End-to-End Sony XDCAM HD422 PAL 1080i50 + EBU R48 Transcode & Rewrap" {
    $tempDir = Join-Path $RootDir "temp\inhouse_test"
    $watchDir = Join-Path $RootDir "temp\inhouse_watchfolder"
    if (Test-Path $tempDir) { Remove-Item $tempDir -Recurse -Force }
    if (Test-Path $watchDir) { Remove-Item $watchDir -Recurse -Force }
    New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
    New-Item -ItemType Directory -Path $watchDir -Force | Out-Null

    $ffmpeg = Join-Path $BinDir "ffmpeg.exe"
    $ffprobe = Join-Path $BinDir "ffprobe.exe"
    $bmxtranswrap = Join-Path $BinDir "bmxtranswrap.exe"

    # Step A: Generate 2-second synthetic SMPTE color bars + 1kHz stereo tone at 50fps
    $synthInput = Join-Path $tempDir "synth_input.mp4"
    $pA = Start-Process -FilePath $ffmpeg -ArgumentList @(
        "-y", "-f", "lavfi", "-i", "smptebars=size=1280x720:rate=50",
        "-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000",
        "-t", "2", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", $synthInput
    ) -NoNewWindow -Wait -PassThru
    if ($pA.ExitCode -ne 0 -or -not (Test-Path $synthInput)) {
        throw "Failed creating synthetic input file with FFmpeg (ExitCode: $($pA.ExitCode))"
    }

    # Step B: Transcode to Sony XDCAM HD422 PAL 1080i50 with EBU R48 8-channel discrete audio
    $intermediateMxf = Join-Path $tempDir "intermediate.mxf"
    $filterComplex = "[0:a:0]pan=mono|c0=c0[l];[0:a:0]pan=mono|c0=c1[r]"
    
    $pB = Start-Process -FilePath $ffmpeg -ArgumentList @(
        "-y", "-threads", "0", "-i", $synthInput, "-f", "lavfi", "-i", "anullsrc=r=48000:cl=mono",
        "-filter_complex", $filterComplex,
        "-map", "0:v", "-map", "[l]", "-map", "[r]",
        "-map", "1:a", "-map", "1:a", "-map", "1:a", "-map", "1:a", "-map", "1:a", "-map", "1:a",
        "-vf", "scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black,tinterlace=mode=interleave_top:flags=vlpf,format=yuv422p",
        "-sws_flags", "bilinear",
        "-c:v", "mpeg2video", "-b:v", "50M", "-minrate", "50M", "-maxrate", "50M", "-bufsize", "17825792",
        "-profile:v", "0", "-level:v", "2", "-pix_fmt", "yuv422p",
        "-g", "12", "-bf", "2", "-flags", "+ildct+ilme", "-top", "1",
        "-r", "25", "-aspect", "16:9",
        "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709",
        "-c:a", "pcm_s24le", "-ar", "48000", "-shortest",
        $intermediateMxf
    ) -NoNewWindow -Wait -PassThru


    if ($pB.ExitCode -ne 0 -or -not (Test-Path $intermediateMxf)) {
        throw "FFmpeg transcode to intermediate MXF failed (ExitCode: $($pB.ExitCode))"
    }

    # Step C: Rewrap with bmxtranswrap to SMPTE RDD9 OP1a
    $rdd9Output = Join-Path $tempDir "final_rdd9.mxf"
    $pC = Start-Process -FilePath $bmxtranswrap -ArgumentList @(
        "-t", "rdd9", "--tc-rate", "25", "-o", $rdd9Output, $intermediateMxf
    ) -NoNewWindow -Wait -PassThru

    if ($pC.ExitCode -ne 0 -or -not (Test-Path $rdd9Output)) {
        throw "bmxtranswrap rewrap to RDD9 failed (ExitCode: $($pC.ExitCode))"
    }


    # Step D: Inspect output with ffprobe and assert strict broadcast compliance
    $probeJson = & $ffprobe -v quiet -print_format json -show_streams -show_format $rdd9Output | ConvertFrom-Json
    
    # Check Video Stream
    $videoStream = $probeJson.streams | Where-Object { $_.codec_type -eq "video" }
    if (-not $videoStream) { throw "No video stream found in MXF output" }
    if ($videoStream.codec_name -ne "mpeg2video") { throw "Video codec is not mpeg2video: $($videoStream.codec_name)" }
    if ($videoStream.width -ne 1920 -or $videoStream.height -ne 1080) { throw "Resolution is not 1920x1080: $($videoStream.width)x$($videoStream.height)" }
    if ($videoStream.pix_fmt -ne "yuv422p") { throw "Pixel format is not yuv422p: $($videoStream.pix_fmt)" }

    # Check Audio Streams (EBU R48: 8 discrete channels)
    $audioStreams = $probeJson.streams | Where-Object { $_.codec_type -eq "audio" }
    $totalAudioChannels = ($audioStreams | Measure-Object -Property channels -Sum).Sum
    if ($totalAudioChannels -ne 8) {
        throw "Expected 8 total discrete audio channels, got: $totalAudioChannels"
    }

    # Step E: Test Atomic Delivery to Watchfolder
    $finalDelivery = Join-Path $watchDir "1_TEST_TOPIC.mxf"
    $tempDelivery = Join-Path $watchDir ".1_TEST_TOPIC.mxf.tmp"
    Copy-Item $rdd9Output $tempDelivery
    Rename-Item $tempDelivery $finalDelivery
    if (-not (Test-Path $finalDelivery)) {
        throw "Atomic delivery failed: $finalDelivery does not exist"
    }

    # Cleanup test media
    Remove-Item $tempDir -Recurse -Force
    Remove-Item $watchDir -Recurse -Force
}

# ------------------------------------------------------------------------------
# 4. Test Live Web Endpoints with Ephemeral Daemon Instance
# ------------------------------------------------------------------------------
Assert-Step "Live Daemon Startup & Web Health Check on Port $TestPort" {
    $tempConfigPath = Join-Path $RootDir "temp\inhouse_config.json"
    $tempDbPath = Join-Path $RootDir "temp\inhouse_test.db"
    
    $cfg = @{
        database_path = "temp/inhouse_test.db"
        watchfolder_path = "temp/inhouse_watchfolder"
        temp_path = "temp/inhouse_work"
        bin_dir = "bin"
        web_port = $TestPort
        web_host = "127.0.0.1"
        auth_mode = "open_mcr"
        max_concurrent_downloads = 2
        max_concurrent_transcodes = 2
        ytdl_auto_update_nightly = $false
    }
    $cfg | ConvertTo-Json | Set-Content -Path $tempConfigPath -Force

    $daemonLog = Join-Path $RootDir "temp\inhouse_daemon.log"
    $daemonErrLog = Join-Path $RootDir "temp\inhouse_daemon_err.log"
    $daemonProcess = Start-Process -FilePath $BinaryPath `
        -ArgumentList "run --config `"$tempConfigPath`"" `
        -RedirectStandardOutput $daemonLog `
        -RedirectStandardError $daemonErrLog `
        -PassThru


    try {
        Start-Sleep -Seconds 3


        # Test /api/system/status
        $statusResp = Invoke-RestMethod -Uri "http://127.0.0.1:$TestPort/api/system/status" -TimeoutSec 5
        if ($statusResp.llm_status -ne "Ready") {
            throw "System status reported unexpected LLM status: $($statusResp.llm_status)"
        }

        # Test /api/journalists (should be seeded)
        $journalistsResp = Invoke-RestMethod -Uri "http://127.0.0.1:$TestPort/api/journalists" -TimeoutSec 5
        if (-not ($journalistsResp.journalists.Count -ge 4)) {
            throw "Expected at least 4 default seeded journalists, got: $($journalistsResp.journalists.Count)"
        }

        # Test Web Views
        $mcrResp = Invoke-WebRequest -Uri "http://127.0.0.1:$TestPort/mcr" -UseBasicParsing -TimeoutSec 5
        $loginResp = Invoke-WebRequest -Uri "http://127.0.0.1:$TestPort/login" -UseBasicParsing -TimeoutSec 5
        if ($mcrResp.StatusCode -ne 200 -or $loginResp.StatusCode -ne 200) {
            throw "Web views returned non-200 status: MCR=$($mcrResp.StatusCode), Login=$($loginResp.StatusCode)"
        }

        # Test POST /api/jobs
        $newJob = @{
            url = "https://www.youtube.com/watch?v=inhouse_test"
            notes = "TEST_INGEST"
            priority = 5
        } | ConvertTo-Json

        $jobResp = Invoke-RestMethod -Uri "http://127.0.0.1:$TestPort/api/jobs" `
            -Method Post -Body $newJob -ContentType "application/json" -TimeoutSec 5

        if ($jobResp.status -ne "ok") {
            throw "Failed creating job via API: $($jobResp | ConvertTo-Json)"
        }
    } finally {
        if ($daemonProcess -and -not $daemonProcess.HasExited) {
            Stop-Process -Id $daemonProcess.Id -Force
            Start-Sleep -Seconds 1
        }
        if (Test-Path $tempConfigPath) { Remove-Item $tempConfigPath -Force -ErrorAction SilentlyContinue }
        if (Test-Path $tempDbPath) { Remove-Item $tempDbPath -Force -ErrorAction SilentlyContinue }
        if (Test-Path "$tempDbPath-wal") { Remove-Item "$tempDbPath-wal" -Force -ErrorAction SilentlyContinue }
        if (Test-Path "$tempDbPath-shm") { Remove-Item "$tempDbPath-shm" -Force -ErrorAction SilentlyContinue }
    }
}


# ------------------------------------------------------------------------------
# Summary Report
# ------------------------------------------------------------------------------
Write-Host ""
Write-Host "======================================================================" -ForegroundColor Cyan
Write-Host "                        VERIFICATION SUMMARY                          " -ForegroundColor Cyan
Write-Host "======================================================================" -ForegroundColor Cyan
Write-Host "Total Passed: $testsPassed" -ForegroundColor Green
Write-Host "Total Failed: $testsFailed" -ForegroundColor $(if ($testsFailed -eq 0) { "Green" } else { "Red" })

if ($testsFailed -eq 0) {
    Write-Host "`n✓ ALL ROBUSTNESS TESTS PASSED. The application is production ready!" -ForegroundColor Green
    exit 0
} else {
    Write-Host "`n✗ SOME TESTS FAILED. Please review the errors above." -ForegroundColor Red
    exit 1
}
