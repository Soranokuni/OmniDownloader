@echo off
rem OmniDownloader installer: double-click, or run with options, e.g.
rem   Install.cmd --watchfolder "\\dalet\ingest" --account "DOMAIN\svc_omni"
rem   Install.cmd --dir D:\OmniIngest --port 8080
rem Default account: NT SERVICE\OmniIngestService; --account LocalSystem opts out.
rem An upgrade keeps the service's current account.
rem Run it again from a newer release folder to upgrade.
rem OmniIngest-Setup-<version>-<date>-<arch>.exe does the same with pages instead.
setlocal
rem This file and its folder, read before the cd: started through a quoted
rem relative path, cmd resolves them against the current folder.
set "OMNI_SELF=%~f0"
set "OMNI_DIR=%~dp0"
cd /d "%OMNI_DIR%"

rem Not elevated: this file runs again elevated, with the same arguments. They
rem travel as one raw string in an environment variable, never inside the
rem PowerShell command text, and the elevated process is cmd.exe itself, started
rem as cmd /d /s /c with the command inside one more pair of quotes: /s removes
rem exactly that outer pair, so quoted arguments (spaces, apostrophes,
rem ampersands and parentheses inside quotes) arrive unchanged. The command
rem first sets OMNI_RELAUNCHED, so a relaunched copy that still has no
rem administrator rights stops instead of starting another one.
rem The .cmd file association (its runas verb is cmd /C with the file and the
rem arguments) is not used: it loses every argument that is quoted. Not
rem carried: a variable name between percent signs is expanded again, and a
rem caret outside quotes is lost. fltmc needs administrator rights and, unlike
rem net session, does not depend on the Server service.
set OMNI_ARGS=%*

fltmc >nul 2>&1
if errorlevel 1 (
    if defined OMNI_RELAUNCHED (
        echo Could not get administrator rights. Right-click the file and choose Run as administrator, or start it from an elevated prompt.
        pause
        exit /b 1
    )
    echo Asking for administrator rights...
    powershell -NoProfile -ExecutionPolicy Bypass -Command "$q = [char]34; $a = '/d /s /c ' + $q + 'set ' + $q + 'OMNI_RELAUNCHED=1' + $q + ' & ' + $q + $env:OMNI_SELF + $q; if ($env:OMNI_ARGS) { $a += ' ' + $env:OMNI_ARGS }; Start-Process -FilePath $env:ComSpec -ArgumentList ($a + $q) -Verb RunAs"
    exit /b
)

"%OMNI_DIR%omni-ingest.exe" install %*
echo.
pause
