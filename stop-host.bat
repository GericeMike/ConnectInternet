@echo off
rem rdlink host STOPPER (M2.5): kills the supervisor bat loop + host.exe.
rem Use THIS to stop the service -- `schtasks /End` cannot (the task action
rem wscript.exe exits instantly, leaving an orphaned bat loop), and a bare
rem `taskkill host.exe` just gets restarted after 2 seconds.
rem Match "start-host.bat" in process command lines to find the supervisor cmd.
rem ASCII-only.
powershell -NoProfile -Command "Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -like '*start-host.bat*' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }"
taskkill /F /IM host.exe 2>nul
exit /b 0
