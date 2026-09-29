# rdlink

Rust 编写的局域网远程桌面（自研，对标向日葵/UU 远程的低延迟体验）。
Windows ↔ Windows，主控端 RTX 4060 laptop，被控端 MX250。

- 需求规格与任务拆解：见 `docs/M1-任务拆解.md`
- 当前进度：**T0 完成**（工程骨架 + 全链路工具链验证）

## 项目结构

```
crates/
├── proto/       # 线上协议消息（serde + bincode，两端共用）
├── transport/   # QUIC 传输层（quinn + rustls，T2）
├── host/        # 被控端 bin: host.exe（捕获/编码/输入注入）
└── client/      # 主控端 bin: client.exe（解码/渲染/输入捕获）
third_party/
└── ffmpeg/      # vendored FFmpeg（git 忽略，按下述步骤获取）
docs/            # 设计文档
```

## 环境搭建（两台机器相同步骤）

1. **Rust**（stable-msvc）：https://rustup.rs 下载 rustup-init.exe，默认安装
2. **VS Build Tools 2022**：勾选「使用 C++ 的桌面开发」（含 MSVC v143 + Windows 11 SDK）
3. **LLVM**（bindgen 需要 libclang）：`winget install LLVM.LLVM`
   - 默认装到 `C:\Program Files\LLVM\bin`，`.cargo/config.toml` 已指向该路径
4. **FFmpeg vendored**（不入库，手动放置）：

```
# 下载后解压到 third_party/，最终结构：
third_party/ffmpeg/bin/*.dll      # avcodec-62.dll 等
third_party/ffmpeg/lib/*.lib
third_party/ffmpeg/include/*
```

5. 验证：仓库根目录执行

```
cargo build --workspace
PATH=third_party/ffmpeg/bin:$PATH cargo run -p rdlink-host -- --check
# 期望输出：FFmpeg avutil: 60.x + h264_nvenc 可用 + libx264 可用
```

## 版本锁定（两台机必须一致）

| 组件 | 版本 | 说明 |
|------|------|------|
| Rust | 1.98.1 stable-msvc | |
| FFmpeg | BtbN n8.1-latest-win64-gpl-shared-8.1 | avutil 60 / avcodec 62，GPL 含 x264 |
| ffmpeg-the-third | 6.0.0+ffmpeg-9.0 | crates.io，FFMPEG_DIR 自动探测版本 |
| LLVM | 23.1.2 | 仅构建期需要 |

## 已定决策记录

| # | 决策 | 理由 |
|---|------|------|
| D1 | M1 视频走 QUIC 可靠流 | 局域网零丢包无队头阻塞，M2 迁 datagram（trait 已预留） |
| D2 | M1 允许两次显存拷贝 | 省 4~6ms 换实现速度，M2 做零拷贝 |
| D3 | 渲染 vsync 默认关 | 延迟优先 |
| D4 | 起播等待 IDR | 防花屏 |
| D5 | 证书用 rcgen 运行时生成（替代 ps1 脚本） | 跨机一致、免 openssl，host 首启生成并落盘，client 配置 pin 指纹 |
| D6 | FFMPEG_DIR 用 `[env] relative=true` | build script cwd 在 registry，纯相对路径会解析错位 |
| D7 | 运行时 DLL 靠 PATH 或程序目录 | 调试期 `PATH=third_party/ffmpeg/bin`，发布期 build.rs 拷贝（M3） |

## 已知限制（M1 记录，M4 处理）

- Alt+Tab 等系统组合键会被主控端系统吃掉
- 被控端中文输入法注入（M1 用英文输入法测试）
- 主控端多屏选择（M1 固定主屏，`monitor_index` 配置已预留）
