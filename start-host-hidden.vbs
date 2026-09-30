' rdlink host hidden launcher (M2.5): runs start-host.bat with no visible
' console window, so it cannot be closed/Ctrl+C'd by accident.
' ASCII-only. Used by scheduled task rdlink-host.
CreateObject("Wscript.Shell").Run """D:\AI\ConnectInternet\ConnectInternet\start-host.bat""", 0, False
