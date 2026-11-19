<#
.SYNOPSIS
  Build a self-contained OmniDownloader release for one architecture:
  OmniIngest-Setup-<version>-<date>-<arch>.exe (one file that installs,
  upgrades and removes), and a folder and a zip that install on a 64-bit
  Windows 10/11 PC with Install.cmd (plan P9, P13.1d).

.DESCRIPTION
  * -Arch x64 (the default: a 64-bit Intel/AMD server) or -Arch arm64.
    omni-ingest.exe and OmniIngest-Setup.exe are built for that target
    (--target x86_64-pc-windows-msvc / aarch64-pc-windows-msvc) with the C
    runtime linked in (+crt-static), in their own target directory
    (target\package\<triple>\release): no Visual C++ redistributable needed on
    the target PC, and the build never touches target\release, which a running
    daemon may hold open. --remap-path-prefix turns the cargo home into
    `cargo` and the repository into `omni` inside the executables, so they do
    not carry this machine's user profile or checkout path. The flags go in
    CARGO_ENCODED_RUSTFLAGS (a caller's RUSTFLAGS is not used), and the
    caller's value is put back when the script ends, however it ends.
  * -OutDir (default dist, absolute or relative to the repository; never a
    drive root or the repository itself) receives OmniIngest-<v>-<date>-<arch>\,
    its .zip, OmniIngest-Setup-<v>-<date>-<arch>.exe and its .sha256. Outputs
    of the same name are removed when the run starts, and a run that fails
    removes the ones it wrote: nothing half-made is left behind.
  * bin\ gets ffmpeg, ffprobe, bmxtranswrap and the DLLs beside them, yt-dlp
    (folder build when present) and Deno, all taken from -ToolsDir (default
    bin), and the C++ runtime DLLs bmxtranswrap needs (app-local) from
    -RuntimeDir (default %SystemRoot%\System32). yt-dlp and Deno update
    themselves nightly once installed.
  * Every .exe and .dll in the release folder is checked by its PE header
    (scripts\pe-machine.ps1; nothing is started): an x64 package must hold
    x64 programs only; an arm64 package may hold x64 and ARM64 (x64 tools run
    under emulation there, and bmxtranswrap exists only as x64). A wrong one
    stops the script before anything is zipped or packed. An x64 package
    therefore needs a -ToolsDir with the x64 tools and a -RuntimeDir with the
    x64 runtime DLLs; on an ARM64 PC, System32's runtime DLLs are not x64.
    No omni-ingest command fetches the pinned tool set into a folder: the
    service installs its own host's pinned tools (Admin -> Toolchain), so
    fill -ToolsDir from an x64 installation's bin\ or from the pinned
    archives listed in crates\omni-core\src\toolpins.rs.
  * The roster and taxonomy (real names) are included only with
    -IncludeRoster. Never commit or publish a release built with it.
  * The Setup file is the release folder packed with `OmniIngest-Setup.exe
    --pack`, checked with `--verify`, and refused when its version is not the
    packaged omni-ingest.exe's. Its SHA-256 is written beside it (.sha256).
    These runs start the built executables: an x64 build runs on an ARM64 PC
    under emulation, an ARM64 build does not run on an x64 PC, so build an
    arm64 package on an ARM64 machine.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File .\scripts\package.ps1
  powershell -ExecutionPolicy Bypass -File .\scripts\package.ps1 -Arch x64 -ToolsDir D:\Tools\x64\bin -RuntimeDir D:\Tools\x64\vcrt
  powershell -ExecutionPolicy Bypass -File .\scripts\package.ps1 -Arch arm64 -IncludeRoster
#>
param(
    [ValidateSet('x64', 'arm64')]
    [string]$Arch = 'x64',
    [string]$ToolsDir = 'bin',
    [string]$RuntimeDir = (Join-Path $env:SystemRoot 'System32'),
    [switch]$IncludeRoster,
    [string]$OutDir = "dist"
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
. (Join-Path $PSScriptRoot 'pe-machine.ps1')

Write-Host "Architecture: $Arch"

$triple = if ($Arch -eq 'x64') { 'x86_64-pc-windows-msvc' } else { 'aarch64-pc-windows-msvc' }
# What a program file in the package may be built for.
$allowed = if ($Arch -eq 'x64') { @('x64') } else { @('x64', 'arm64') }

# A folder parameter as an absolute path (relative ones are under the repository).
function Resolve-Folder([string]$dir) {
    if (-not [System.IO.Path]::IsPathRooted($dir)) { $dir = Join-Path $root $dir }
    return [System.IO.Path]::GetFullPath($dir)
}

# One argument for Start-Process, quoted when it holds a space (Windows
# PowerShell 5.1 joins -ArgumentList with spaces and quotes nothing).
function Quote-Arg([string]$a) {
    if ($a -notmatch '\s') { return $a }
    # A trailing backslash would escape the closing quote: double it.
    if ($a.EndsWith('\')) { $a = $a + '\' }
    return '"' + $a + '"'
}

# Why a built executable cannot be started here.
function Cannot-Run([string]$exe, $err) {
    $hostArch = $env:PROCESSOR_ARCHITECTURE
    if ($env:PROCESSOR_ARCHITEW6432) { $hostArch = $env:PROCESSOR_ARCHITEW6432 }
    return "Cannot start $exe ($err). The $Arch executables do not run on this $hostArch machine: build the $Arch package on a machine that can run them."
}

# Run the Setup program `$exe` with `$arguments`, waiting for it (it is a
# windowed program: `& Setup.exe` would not wait). Returns the exit code, the
# standard output and the standard error.
function Invoke-Setup([string]$exe, [string[]]$arguments) {
    $out = [System.IO.Path]::GetTempFileName()
    $err = [System.IO.Path]::GetTempFileName()
    try {
        $argLine = ($arguments | ForEach-Object { Quote-Arg $_ }) -join ' '
        try {
            $p = Start-Process -FilePath $exe -ArgumentList $argLine -Wait -PassThru -NoNewWindow `
                -RedirectStandardOutput $out -RedirectStandardError $err
        }
        catch {
            throw (Cannot-Run $exe $_.Exception.Message)
        }
        return @{
            Code   = $p.ExitCode
            Output = [System.IO.File]::ReadAllText($out).Trim()
            Error  = [System.IO.File]::ReadAllText($err).Trim()
        }
    }
    finally {
        Remove-Item -LiteralPath $out, $err -Force -ErrorAction SilentlyContinue
    }
}

function Get-Sha256([string]$path) {
    return (Get-FileHash $path -Algorithm SHA256).Hash.ToLower()
}

# Every .exe and .dll under `$dir` must be built for an architecture in
# `$allowed`; otherwise stop, naming each file that is not.
function Assert-PackageArch([string]$dir) {
    $wrong = @()
    $files = Get-ChildItem -Recurse -File $dir | Where-Object { $_.Extension -in '.exe', '.dll' } | Sort-Object FullName
    foreach ($f in $files) {
        $kind = Get-PeArch $f.FullName
        if ($allowed -notcontains $kind) {
            $wrong += "  {0} is {1}" -f $f.FullName.Substring($dir.Length + 1), $kind
        }
    }
    if ($wrong.Count -gt 0) {
        $need = if ($Arch -eq 'x64') { 'x64 programs only' } else { 'x64 or ARM64 programs' }
        throw ("The $Arch package needs $need; nothing was zipped or packed:`n" + ($wrong -join "`n") +
            "`nGive the matching tools with -ToolsDir and the runtime DLLs with -RuntimeDir.")
    }
    Write-Host ("  {0} program files checked: all {1}." -f $files.Count, ($allowed -join ' or '))
}

# The caller's compiler flags, put back however the run ends.
$savedFlags = $env:CARGO_ENCODED_RUSTFLAGS
try {
    $tools = Resolve-Folder $ToolsDir
    $runtime = Resolve-Folder $RuntimeDir
    # The output folder: absolute or under the repository, never a drive root
    # or the repository itself (the run removes outputs there by name).
    $dist = (Resolve-Folder $OutDir).TrimEnd('\')
    $repo = [System.IO.Path]::GetFullPath($root).TrimEnd('\')
    if ($dist -eq [System.IO.Path]::GetPathRoot($dist).TrimEnd('\') -or $dist -eq $repo) {
        throw "-OutDir $OutDir resolves to $dist, a drive root or the repository itself; give a folder of its own (default dist)."
    }
    New-Item -ItemType Directory -Force $dist | Out-Null

    $targetDir = Join-Path $root 'target\package'
    # Static C runtime; and no path of this machine in the executables: the
    # cargo home (registry sources) becomes `cargo`, the repository `omni`.
    # The encoded form keeps a path with spaces as one argument.
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $flags = @(
        '-C', 'target-feature=+crt-static',
        "--remap-path-prefix=$([System.IO.Path]::GetFullPath($cargoHome).TrimEnd('\'))=cargo",
        "--remap-path-prefix=$repo=omni"
    )
    $env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]0x1f
    Write-Host "Building the release for $triple (static C runtime) in $targetDir ..."
    cargo build --release --target $triple --target-dir $targetDir
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    # The Setup program: the same flags, target and target directory.
    cargo build --release -p omni-setup --target $triple --target-dir $targetDir
    if ($LASTEXITCODE -ne 0) { throw "cargo build of the Setup program failed" }

    $built = Join-Path $targetDir "$triple\release"
    $exe = Join-Path $built 'omni-ingest.exe'
    $setupStub = Join-Path $built 'OmniIngest-Setup.exe'
    foreach ($b in $exe, $setupStub) {
        $kind = Get-PeArch $b
        if ($kind -ne $Arch) { throw "$b is $kind, not $($Arch): the build did not produce the requested architecture" }
    }
    try {
        $versionLine = & $exe --version
    }
    catch {
        throw (Cannot-Run $exe $_.Exception.Message)
    }
    $versionCode = $LASTEXITCODE
    $version = (([string]($versionLine | Select-Object -First 1)) -split ' ')[-1].Trim()
    if ($versionCode -ne 0 -or $version -notmatch '^\d+\.\d+\.\d+') {
        throw "omni-ingest.exe --version exited with $versionCode and printed '$versionLine'; expected a version such as 1.0.0"
    }
    $date = Get-Date -Format yyyyMMdd
    $name = "OmniIngest-$version-$date-$Arch"
    $stage = Join-Path $dist $name
    $zip = "$stage.zip"
    $setupName = "OmniIngest-Setup-$version-$date-$Arch.exe"
    $setup = Join-Path $dist $setupName
    $setupSum = "$setup.sha256"
    $outputs = @($stage, $zip, $setup, $setupSum)
    # An older result of the same name never sits next to a newer half-result.
    foreach ($old in $outputs) {
        if (Test-Path -LiteralPath $old) { Remove-Item -LiteralPath $old -Recurse -Force }
    }

    # From here on, a failure removes everything this run wrote into $dist.
    try {
        New-Item -ItemType Directory -Force (Join-Path $stage 'bin') | Out-Null

        Copy-Item $exe $stage
        foreach ($f in 'Install.cmd', 'Uninstall.cmd', 'README.txt') {
            Copy-Item (Join-Path $PSScriptRoot "package\$f") $stage
        }
        # The operator and administrator guides travel with every release.
        New-Item -ItemType Directory -Force (Join-Path $stage 'docs') | Out-Null
        foreach ($f in 'USER_GUIDE.md', 'ADMIN_GUIDE.md', 'USER_GUIDE.el.md', 'ADMIN_GUIDE.el.md') {
            Copy-Item (Join-Path $root "docs\$f") (Join-Path $stage 'docs')
        }

        # Broadcast tools: required.
        foreach ($t in 'ffmpeg.exe', 'ffprobe.exe', 'bmxtranswrap.exe') {
            $p = Join-Path $tools $t
            if (-not (Test-Path $p)) { throw "$p is missing; put the tool in $tools first" }
            Copy-Item $p (Join-Path $stage 'bin')
        }
        Get-ChildItem $tools -Filter *.dll -ErrorAction SilentlyContinue |
            Copy-Item -Destination (Join-Path $stage 'bin')

        # bmxtranswrap is built against the Visual C++ runtime: ship it next to it.
        foreach ($dll in 'msvcp140.dll', 'vcruntime140.dll', 'vcruntime140_1.dll') {
            $src = Join-Path $runtime $dll
            if (-not (Test-Path $src)) { throw "$dll not found in $runtime; install the VC++ 2015-2022 x64 redistributable on this build PC, or give -RuntimeDir" }
            Copy-Item $src (Join-Path $stage 'bin')
        }

        # Self-updating tools: a starting copy, so a PC without internet at
        # install time still works.
        $ytFolder = Join-Path $tools 'yt-dlp'
        if (Test-Path (Join-Path $ytFolder 'yt-dlp.exe')) {
            Copy-Item -Recurse $ytFolder (Join-Path $stage 'bin\yt-dlp')
        } elseif (Test-Path (Join-Path $tools 'yt-dlp.exe')) {
            Copy-Item (Join-Path $tools 'yt-dlp.exe') (Join-Path $stage 'bin')
        } else {
            Write-Warning "No yt-dlp in $tools`: the service will need one before it can download"
        }
        $deno = Join-Path $tools 'deno.exe'
        if (Test-Path $deno) { Copy-Item $deno (Join-Path $stage 'bin') }
        else { Write-Warning "No deno.exe in $tools`: YouTube downloads fail until the service downloads it" }

        if ($IncludeRoster) {
            New-Item -ItemType Directory -Force (Join-Path $stage 'data') | Out-Null
            foreach ($f in 'journalists.seed.json', 'taxonomy.json') {
                $p = Join-Path $root "data\$f"
                if (Test-Path $p) { Copy-Item $p (Join-Path $stage 'data') }
            }
            Write-Warning "This release contains the real roster: keep it inside the station."
        }

        # Before anything is zipped or packed: no program of the wrong architecture.
        Write-Host "Checking the program files for $Arch ..."
        Assert-PackageArch $stage

        # Names relative to the release folder, from its resolved path.
        $sums = Get-ChildItem -Recurse -File $stage | Sort-Object FullName | ForEach-Object {
            $rel = $_.FullName.Substring($stage.Length + 1)
            "{0}  {1}" -f (Get-Sha256 $_.FullName), $rel
        }
        Set-Content -Path (Join-Path $stage 'SHA256SUMS.txt') -Value $sums -Encoding ascii

        # The zip: the route for a command-line install (Install.cmd).
        Write-Host "Compressing $zip ..."
        Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip

        # The Setup file: the finished release folder packed into the Setup program.
        Write-Host "Packing $setup ..."
        $r = Invoke-Setup $setupStub @('--pack', $stage, '--out', $setup)
        if ($r.Code -ne 0) { throw "OmniIngest-Setup.exe --pack failed (exit $($r.Code)): $($r.Error)" }
        Write-Host "  $($r.Output)"
        $r = Invoke-Setup $setup @('--verify', $setup)
        if ($r.Code -ne 0) { throw "OmniIngest-Setup.exe --verify failed (exit $($r.Code)): $($r.Error)" }
        Write-Host "  $($r.Output)"
        # The version on the first line of /? (no installer mode runs).
        $r = Invoke-Setup $setup @('/?')
        if ($r.Code -ne 0) { throw "OmniIngest-Setup.exe /? failed (exit $($r.Code)): $($r.Error)" }
        $first = ($r.Output -split "`r?`n")[0].Trim()
        if ($first -match '^OmniIngest Setup (\S+)$') { $setupVersion = $Matches[1] }
        else { throw "OmniIngest-Setup.exe /? does not start with its version: '$first'" }
        if ($setupVersion -ne $version) {
            throw "The Setup program is version $setupVersion but the packaged omni-ingest.exe is $version; bump both crates to the same version and build again."
        }
        $setupHash = Get-Sha256 $setup
        Set-Content -Path $setupSum -Value ("{0}  {1}" -f $setupHash, $setupName) -Encoding ascii
    }
    catch {
        # Nothing half-made stays behind: the four outputs of this run go.
        foreach ($o in $outputs) {
            Remove-Item -LiteralPath $o -Recurse -Force -ErrorAction SilentlyContinue
        }
        throw
    }

    $zipItem = Get-Item $zip
    $setupItem = Get-Item $setup
    Write-Host ""
    Write-Host "Release ready ($Arch):"
    Write-Host "  folder: $stage"
    Write-Host ("  zip:    {0} ({1} bytes, {2} MB)" -f $zipItem.Name, $zipItem.Length, [math]::Round($zipItem.Length / 1MB))
    Write-Host "          SHA-256 $(Get-Sha256 $zip)"
    Write-Host ("  setup:  {0} ({1} bytes, {2} MB)" -f $setupItem.Name, $setupItem.Length, [math]::Round($setupItem.Length / 1MB))
    Write-Host "          SHA-256 $setupHash"
    Write-Host "Give the administrator $setupName and $setupName.sha256 (both in $dist);"
    Write-Host "for a command-line install, copy the folder or the zip and run Install.cmd."
}
finally {
    # The caller's compiler flags, as they were.
    if ($null -eq $savedFlags) { Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue }
    else { $env:CARGO_ENCODED_RUSTFLAGS = $savedFlags }
    Pop-Location
}
