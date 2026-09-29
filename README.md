# rdlink

Rust 编写的 Windows 局域网远程桌面。对标向日葵/UU 远程的低延迟体验，全链路自研：WGC 捕获 → 硬件编码 → QUIC 传输 → 解码 → wgpu 渲染，键鼠反向通道齐全。

- 两机实测（4060 主控 ↔ MX250 被控，Wi-Fi）：**端到端 p50 18~22ms**、33~40fps、局域网免服务器
- 设计文档：`docs/M1-任务拆解.md`（需求/架构/任务卡）、`docs/会话交接.md`（进度/坑位/决策）
- 当前阶段：**M1 完成**（tag `m1`）

## 快速上手（两台 Windows 机）

### 被控端（一次性）

1. 环境搭建见下文「环境」一节
2. 启动（**必须在桌面会话**，双击或 `start-host.bat`；WGC 捕获不能在 SSH/服务会话里跑）：

```
cd 仓库根目录
start-host.bat          # 后台启动，日志 host-run.log
```

3. 启动输出里有**证书指纹**（64 位十六进制），填给主控端（指纹不变，只需查一次：`type host-run.log`）

### 主控端

```
cd 仓库根目录
PATH=third_party/ffmpeg/bin:$PATH client.exe --host <被控端IP>:9527 <证书指纹>
```

窗口模式打开（可最小化/关闭），**F11 全屏、Esc 退出**。标题栏实时显示：

```
rdlink | 33fps | e2e 21ms(p95 27) | enc 15 dec 1 | rtt 2
```

### 辅助命令

| 命令 | 用途 |
|------|------|
| `host --check` | 编码器注册检查（注意：注册≠能开会话，以 `--encode-demo` 实测为准） |
| `host --capture-demo` / `--encode-demo` | 捕获/编码自测（fps 统计 / out.h264 落盘） |
| `host --input-demo` | 输入注入自测（6 项程序化验证） |
| `host --list-monitors` | 列显示器（多屏选择 M4 用） |
| `client --render-demo [vsync]` | 本地渲染管线自测（不走网络） |
| `client --decode-demo <file>` | 解码自测 |

## 配置（rdlink.toml，可选）

放仓库根目录，不存在用默认值：

```toml
[host]
port = 9527            # 监听端口
cert_dir = "certs"     # 证书目录（首启自动生成）

[client]
vsync = false          # true = 渲染垂直同步（延迟+8~16ms，更省电）
```

编码参数走环境变量（冒烟/扫描用）：`RDLINK_BITRATE_MBPS`（默认 50）、`RDLINK_GOP`（默认 90）。

## 环境搭建（新机器）

1. **Rust** stable-msvc（rustup 默认安装）
2. **VS Build Tools 2022**（C++ 桌面开发工作负载）
3. **LLVM**（bindgen 需要）：`winget install LLVM.LLVM`
4. **FFmpeg vendored**（git 忽略，手动放置）：BtbN `ffmpeg-n8.1-latest-win64-gpl-shared-8.1` 解压为 `third_party/ffmpeg/`
5. 验证：`cargo build --workspace` 全绿 + `host --check` 显示编码器已注册

运行时 DLL：调试期 `PATH=third_party/ffmpeg/bin:$PATH`，或把 DLL 拷到 exe 旁。

## 运维通道（可选，主控端 SSH 直达被控端）

被控端开 OpenSSH Server + 主控端公钥免密后：

```
ssh <user>@<被控端IP> "cd /d D:\AI\ConnectInternet\ConnectInternet && git pull && cargo build --release -p rdlink-host"
ssh <user>@<被控端IP> "taskkill /IM host.exe /F & schtasks /Run /TN rdlink-host"   # 重启 host（桌面会话）
```

注意：SSH 会话**不能**直接运行 host（WGC 需桌面会话），用计划任务（`schtasks /Create /TN rdlink-host /TR "...start-host.bat" /SC ONCE /ST 00:00 /IT /F`）。

## 架构速览

```
被控端 host.exe                          主控端 client.exe
┌─────────────────────────┐             ┌──────────────────────────┐
│ WGC 捕获(脏区驱动)        │             │ QUIC 收流                 │
│ → 编码(NVENC→QSV→x264    │  QUIC v2    │ → 软解 h264(in-band SPS)  │
│    三级兜底, CBR, 无B帧)  │ ──────────→ │ → BGRA                    │
│ 背压: 有界通道整帧丢弃     │  视频流      │ → wgpu 直通渲染(230+fps)   │
│ SendInput 注入 ←──────────┼──────────── │ winit 键鼠捕获→VK 映射     │
│  [stats] 编码p50/p95/队列 │  输入流      │  标题栏: e2e/enc/dec/rtt  │
└──────────┬──────────────┘             └──────────────────────────┘
           └─ 控制流: Ping/Pong 四时间戳对时, Bye 优雅退出, 版本校验
```

证书：rcgen 自签 + SHA-256 指纹 pin（无 CA，指纹即身份）。

## 性能基线（M1，docs/M1-基线数据.md）

| 场景 | e2e p50 | 瓶颈 |
|------|---------|------|
| 本机闭环（4060） | 4~6ms | — |
| 两机 Wi-Fi（被控端 QSV/x264） | 18~22ms | 编码 ~15ms |

编码瓶颈的出路（M2）：被控端 D3D11 零拷贝直喂编码器（省 CPU 拷贝 8~10ms），或回滚 576.xx 驱动恢复 NVENC（2~5ms 级，需管理员）。

## 已知限制

- UAC 提权窗口无法注入（UIPI）；中文输入法注入未做（M4）
- Alt+Tab 被主控端系统拦截（M4 键盘钩子）
- 多屏选择/DPI 映射固定主屏主分辨率（M4）
- 断线重连未做（断开需重连命令；host 自动回收会话等下一个连接）
