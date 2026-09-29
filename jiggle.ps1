# 冒烟测试辅助：持续微动光标产生画面变化（WGC 脏区驱动，静止=无帧）
# 在被控端桌面会话运行（schtasks /IT 调用），默认 30 分钟。
param(
    [int]$Minutes = 30,
    [int]$IntervalMs = 500
)
Add-Type -AssemblyName System.Windows.Forms
$end = (Get-Date).AddMinutes($Minutes)
$i = 0
while ((Get-Date) -lt $end) {
    $p = [System.Windows.Forms.Cursor]::Position
    $d = ($i % 3)
    [System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point(($p.X + $d), $p.Y)
    $i++
    Start-Sleep -Milliseconds $IntervalMs
}
Write-Output "jiggle done: $i moves"
