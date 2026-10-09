<#
.SYNOPSIS
  Build a self-contained OmniDownloader release: a folder and a zip that
  install on any 64-bit Windows 10/11 PC with Install.cmd (plan P9).

.DESCRIPTION
  * omni-ingest.exe is built with the C runtime linked in (+crt-static), in
    its own target directory: no Visual C++ redistributable needed on the
    target PC, and the build never touches target\release, which a running
    daemon may hold open.
  * bin\ gets ffmpeg, ffprobe, bmxtranswrap, the C++ runtime DLLs that
    bmxtranswrap needs (app-local), yt-dlp (folder build when present) and
    Deno. yt-dlp and Deno update themselves nightly once installed.
  * The roster and taxonomy (real names) are included only with
    -IncludeRoster. Never commit or publish a release built with it.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File .\scripts\package.ps1
  powershell -ExecutionPolicy Bypass -File .\scripts\package.ps1 -IncludeRoster
#>
param(
    [switch]$IncludeRoster,
    [string]$OutDir = "dist"
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $targetDir = Join-Path $root 'target\package'
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    Write-Host "Building the release (static C runtime) in $targetDir ..."
    cargo build --release --target-dir $targetDir
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    Remove-Item Env:RUSTFLAGS

    $exe = Join-Path $targetDir 'release\omni-ingest.exe'
    $version = ((& $exe --version) -split ' ')[-1].Trim()
    $name = "OmniIngest-$version-$(Get-Date -Format yyyyMMdd)"
    $stage = Join-Path (Join-Path $root $OutDir) $name
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force (Join-Path $stage 'bin') | Out-Null

    Copy-Item $exe $stage
    foreach ($f in 'Install.cmd', 'Uninstall.cmd', 'README.txt') {
        Copy-Item (Join-Path $PSScriptRoot "package\$f") $stage
    }

    # Broadcast tools: required.
    foreach ($t in 'ffmpeg.exe', 'ffprobe.exe', 'bmxtranswrap.exe') {
        $p = Join-Path $root "bin\$t"
        if (-not (Test-Path $p)) { throw "bin\$t is missing; put the tool in bin\ first" }
        Copy-Item $p (Join-Path $stage 'bin')
    }
    Get-ChildItem (Join-Path $root 'bin') -Filter *.dll -ErrorAction SilentlyContinue |
        Copy-Item -Destination (Join-Path $stage 'bin')

    # bmxtranswrap is built against the Visual C++ runtime: ship it next to it.
    foreach ($dll in 'msvcp140.dll', 'vcruntime140.dll', 'vcruntime140_1.dll') {
        $src = Join-Path $env:SystemRoot "System32\$dll"
        if (-not (Test-Path $src)) { throw "$dll not found in System32; install the VC++ 2015-2022 x64 redistributable on this build PC" }
        Copy-Item $src (Join-Path $stage 'bin')
    }

    # Self-updating tools: a starting copy, so a PC without internet at
    # install time still works.
    $ytFolder = Join-Path $root 'bin\yt-dlp'
    if (Test-Path (Join-Path $ytFolder 'yt-dlp.exe')) {
        Copy-Item -Recurse $ytFolder (Join-Path $stage 'bin\yt-dlp')
    } elseif (Test-Path (Join-Path $root 'bin\yt-dlp.exe')) {
        Copy-Item (Join-Path $root 'bin\yt-dlp.exe') (Join-Path $stage 'bin')
    } else {
        Write-Warning "No yt-dlp in bin\: the service will need one before it can download"
    }
    $deno = Join-Path $root 'bin\deno.exe'
    if (Test-Path $deno) { Copy-Item $deno (Join-Path $stage 'bin') }

    if ($IncludeRoster) {
        New-Item -ItemType Directory -Force (Join-Path $stage 'data') | Out-Null
        foreach ($f in 'journalists.seed.json', 'taxonomy.json') {
            $p = Join-Path $root "data\$f"
            if (Test-Path $p) { Copy-Item $p (Join-Path $stage 'data') }
        }
        Write-Warning "This release contains the real roster: keep it inside the station."
    }

    $sums = Get-ChildItem -Recurse -File $stage | Sort-Object FullName | ForEach-Object {
        $rel = $_.FullName.Substring($stage.Length + 1)
        "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $rel
    }
    Set-Content -Path (Join-Path $stage 'SHA256SUMS.txt') -Value $sums -Encoding ascii

    $zip = "$stage.zip"
    if (Test-Path $zip) { Remove-Item $zip }
    Write-Host "Compressing $zip ..."
    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
    $mb = [math]::Round((Get-Item $zip).Length / 1MB)
    Write-Host ""
    Write-Host "Release ready:"
    Write-Host "  folder: $stage"
    Write-Host "  zip:    $zip ($mb MB)"
    Write-Host "Copy either to the target PC and run Install.cmd."
}
finally {
    Pop-Location
}
