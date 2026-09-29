//! rdlink 被控端：屏幕捕获 → NVENC 编码 → QUIC 发送；接收输入事件并注入。

mod capture_demo;
mod encode_demo;

use ffmpeg_the_third as ffmpeg;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--check") => check(),
        Some("--capture-demo") => capture_demo::run(),
        Some("--encode-demo") => encode_demo::run(),
        Some("--list-monitors") => capture_demo::list_monitors(),
        _ => {
            println!("rdlink-host {} (被控端)", env!("CARGO_PKG_VERSION"));
            println!("协议版本: {}", rdlink_proto::PROTOCOL_VERSION);
            println!("正常运行模式将在 T7 接通（监听 QUIC + 视频链路）");
        }
    }
}

/// T0 验收项：验证 FFmpeg 链接、版本与编码器可用性（NVENC 预检）
fn check() {
    ffmpeg::init().expect("ffmpeg::init 失败，DLL 是否在 PATH？");

    let v = ffmpeg::util::version();
    println!("FFmpeg avutil: {}.{}.{}", v >> 16 & 0xFF, v >> 8 & 0xFF, v & 0xFF);

    for name in ["h264_nvenc", "libx264"] {
        match ffmpeg::encoder::find_by_name(name) {
            Some(c) => println!("encoder {name:<10} 可用 ({})", c.description()),
            None => println!("encoder {name:<10} 不可用"),
        }
    }
}
