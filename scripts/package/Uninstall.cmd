@echo off
rem Removes the OmniDownloader Windows service and its firewall rule.
rem The installation folder (database, configuration, logs) is left in place.
setlocal
cd /d "%~dp0"

net session >nul 2>&1
if errorlevel 1 (
    echo Asking for administrator rights...
    powershell -NoProfile -ExecutionPolicy Bypass -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
    exit /b
)

"%~dp0omni-ingest.exe" uninstall %*
echo.
pause
