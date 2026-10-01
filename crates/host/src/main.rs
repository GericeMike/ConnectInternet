//! rdlink 被控端：屏幕捕获 → 编码 → QUIC 发送；接收输入事件并注入。

mod capture;
mod clipboard;
mod encoder;
mod filex;
mod gpu;
mod input;
mod serve;

use ffmpeg_the_third as ffmpeg;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--check") => check(),
        Some("--capture-demo") => capture::capture_demo(),
        Some("--encode-demo") => capture::encode_demo(),
        Some("--list-monitors") => capture::list_monitors(),
        Some("--input-demo") => input::input_demo(),
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
