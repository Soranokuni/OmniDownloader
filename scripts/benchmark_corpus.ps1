<#
.SYNOPSIS
    Runs the stored 65-link benchmark corpus through `omni-ingest benchmark run`.

.DESCRIPTION
    The corpus, the stages, the thresholds and the report live in the binary
    (plan P6.4); this script only starts it. The run never touches the queue, the
    watchfolder or the archive. The report is written to data\benchmarks\{run}\
    as report.json and report.md, with the change from the previous run of the
    same mode. Exit code 0: every threshold passed. 2: the run finished but a
    threshold failed. Anything else: it could not run (a job is running, another
    run is going, bad arguments).

.PARAMETER StartIndex
    First target, 1-65 (default 1).

.PARAMETER EndIndex
    Last target, 1-65 (default 65).

.PARAMETER Mode
    extract (find the video only, default) or full (download, transcode, rewrap, verify).

.PARAMETER Parallel
    Targets at once, 1-4 (default 2).

.PARAMETER Force
    Run although a job is running.

.PARAMETER Binary
    The omni-ingest.exe to run (default target\gate\release\omni-ingest.exe; never
    target\release, which is the live daemon's).
#>

param(
    [int]$StartIndex = 1,
    [int]$EndIndex = 65,
    [ValidateSet("extract", "full")]
    [string]$Mode = "extract",
    [int]$Parallel = 2,
    [switch]$Force,
    [string]$Binary = ""
)

$ErrorActionPreference = "Stop"

$WorkspaceRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if (-not $Binary) {
    $Binary = Join-Path $WorkspaceRoot "target\gate\release\omni-ingest.exe"
}
if (-not (Test-Path $Binary)) {
    Write-Error "omni-ingest.exe not found at $Binary. Build it first: cargo build --release --target-dir target/gate"
    exit 1
}

$ArgList = @("--root", $WorkspaceRoot, "benchmark", "run", "--mode", $Mode, "--targets", "$StartIndex-$EndIndex", "--parallel", "$Parallel")
if ($Force) { $ArgList += "--force" }

& $Binary @ArgList
exit $LASTEXITCODE
