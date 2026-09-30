@echo off
rem rdlink host supervisor (M2.5): keep host.exe alive and rotate logs.
rem MUST run in an interactive desktop session (WGC capture requirement).
rem Launched hidden via start-host-hidden.vbs (scheduled task rdlink-host).
rem Stop intentionally with: schtasks /End /TN rdlink-host   (taskkill alone
rem only kills host.exe -- this loop restarts it after 2 seconds).
rem NOTE: keep this file ASCII-only -- cmd parses bat in ANSI codepage (GBK),
rem       UTF-8 Chinese comments get mojibake'd and executed as commands.
cd /d "%~dp0"
set PATH=%~dp0third_party\ffmpeg\bin;%PATH%

:loop
rem rotate log when over ~16MB, keep one old generation
if exist host-run.log (
    for %%A in (host-run.log) do (
        if %%~zA GTR 16000000 move /y host-run.log host-run.old.log >nul
    )
)
target\release\host.exe >> host-run.log 2>&1
rem host exited (crash / killed / error) -- bring it back
timeout /t 2 /nobreak >nul
goto loop
