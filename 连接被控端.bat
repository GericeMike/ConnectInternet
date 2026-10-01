@echo off
rem rdlink client launcher: double-click to connect the controlled machine.
rem Connection target comes from rdlink.toml [client] (host + fingerprint).
rem ASCII-only (cmd parses bat in GBK).
cd /d "%~dp0"
set PATH=%~dp0third_party\ffmpeg\bin;%PATH%
target\release\client.exe
pause
