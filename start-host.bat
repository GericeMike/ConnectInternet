@echo off
rem rdlink host launcher.
rem MUST run in an interactive desktop session (WGC capture requirement, pitfall 32).
rem Usage: run this bat from repo root (double-click or via scheduled task).
rem NOTE: keep this file ASCII-only -- cmd parses bat in ANSI codepage (GBK),
rem       UTF-8 Chinese comments get mojibake'd and executed as commands.
cd /d "%~dp0"
set PATH=%~dp0third_party\ffmpeg\bin;%PATH%
target\release\host.exe >> host-run.log 2>&1
