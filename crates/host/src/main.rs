//! rdlink 被控端：屏幕捕获 → 编码 → QUIC 发送；接收输入事件并注入。

mod capture;
mod clipboard;
mod encoder;
mod filex;
mod gpu;
mod input;
mod monitors;
mod power;
mod procs;
mod serve;
mod setpass;
mod shellfolder;

use ffmpeg_the_third as ffmpeg;

fn main() {
    // M4-T2.3：Per-Monitor V2 DPI 感知。不做的话多 DPI 环境下 GetSystemMetrics
    // （虚拟桌面/主屏尺寸，输入映射用）会被系统 DPI 虚拟化污染，坐标全错。
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2.0,
            ),
        );
    }
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--check") => check(),
        Some("--capture-demo") => capture::capture_demo(),
        Some("--encode-demo") => capture::encode_demo(),
        Some("--list-monitors") => capture::list_monitors(),
        Some("--input-demo") => input::input_demo(),
        Some("--set-password") => {
            let pw = args.get(2).cloned().unwrap_or_default();
            if pw.is_empty() {
                eprintln!("用法: host --set-password <密码>");
                std::process::exit(2);
            }
            match setpass::set_password(&pw) {
                Ok((salt, key)) => println!(
                    "密码认证已启用（rdlink.local.toml [host]）\n  auth_salt = {salt}\n  auth_key  = {key}…\n主控端 rdlink.toml [client] 需配置相同密码；删除这两行可关闭认证"
                ),
                Err(e) => {
                    eprintln!("设置失败: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("--set-clip") => {
            let text = args.get(2).cloned().unwrap_or_default();
            match clipboard::write_clipboard_text(&text) {
                Ok(()) => println!("已写入剪贴板（{} 字节）", text.len()),
                Err(e) => println!("写入失败: {e}"),
            }
        }
        Some("--get-clip") => match clipboard::read_clipboard_text() {
            Some(t) => println!("{t}"),
            None => println!("(空/读取失败)"),
        },
        Some("--click") => {
            let x: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            let y: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
            input::click(x, y);
        }
        Some("--set-res") => {
            // M4-T2.4 驱动/运维：改主显示器分辨率（0 = 恢复时读不到原值的情况不会出现，
            // 需要恢复就再调一次指定原值）
            let w: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            let h: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
            if w == 0 || h == 0 {
                eprintln!("用法: host --set-res <宽> <高>");
                std::process::exit(2);
            }
            match monitors::set_resolution(w, h) {
                Ok((orig, r)) => {
                    if r == 0 {
                        println!("分辨率已改: {}x{} → {w}x{h}（CDS=0）", orig.0, orig.1);
                    } else {
                        println!("ChangeDisplaySettingsEx 结果码 {r}（0=成功，-2=模式不支持，-5=参数错误）");
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("失败: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => serve::run(), // 正常运行模式（T7 视频 + T8 输入注入待接）
    }
}

/// 环境自检：FFmpeg 链接、版本与编码器注册情况（三级兜底全覆盖）。
/// 注意：find_by_name 只查注册表，不代表能开会话——
/// 被控端曾出现 NVENC 注册正常但 OpenEncodeSessionEx 被驱动拒绝（坑位 10）。
/// 真实可用性以 `--encode-demo` 实测为准（TODO: 改为 open probe）。
fn check() {
    ffmpeg::init().expect("ffmpeg::init 失败，DLL 是否在 PATH？");

    let v = ffmpeg::util::version();
    println!("FFmpeg avutil: {}.{}.{}", v >> 16 & 0xFF, v >> 8 & 0xFF, v & 0xFF);

    for name in ["h264_nvenc", "h264_qsv", "libx264"] {
        match ffmpeg::encoder::find_by_name(name) {
            Some(c) => println!("encoder {name:<10} 已注册 ({})", c.description()),
            None => println!("encoder {name:<10} 不可用"),
        }
    }
}
