@echo off
rem host 启动脚本：必须在交互式桌面会话运行（WGC 捕获要求，坑位 32）。
rem 用法（本机双击或计划任务调用）：cd 到仓库根目录后执行本脚本。
cd /d "%~dp0"
set PATH=%~dp0third_party\ffmpeg\bin;%PATH%
target\release\host.exe >> host-run.log 2>&1
