@echo off
rem OmniDownloader installer: double-click, or run with options, e.g.
rem   Install.cmd --watchfolder "\\dalet\ingest" --account "DOMAIN\svc_omni"
rem   Install.cmd --dir D:\OmniIngest --port 8080
rem Run it again from a newer release folder to upgrade.
setlocal
cd /d "%~dp0"

net session >nul 2>&1
if errorlevel 1 (
    echo Asking for administrator rights...
    if "%~1"=="" (
        powershell -NoProfile -ExecutionPolicy Bypass -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
    ) else (
        powershell -NoProfile -ExecutionPolicy Bypass -Command "Start-Process -FilePath '%~f0' -ArgumentList '%*' -Verb RunAs"
    )
    exit /b
)

"%~dp0omni-ingest.exe" install %*
echo.
pause
