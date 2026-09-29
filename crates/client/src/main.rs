//! rdlink 主控端：QUIC 收流 → NVDEC 解码 → wgpu 渲染；捕获键鼠发往被控端。

mod decode_demo;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--decode-demo") => {
            let input = args.get(2).cloned().unwrap_or_else(|| {
                eprintln!("用法: client --decode-demo <输入文件.mp4>");
                std::process::exit(2);
            });
            decode_demo::run(&input);
        }
        Some("--render-demo") => {
            eprintln!("render-demo: 尚未实现（T6 交付：本地测试图渲染）");
            std::process::exit(2);
        }
        _ => {
            println!("rdlink-client {} (主控端)", env!("CARGO_PKG_VERSION"));
            println!("协议版本: {}", rdlink_proto::PROTOCOL_VERSION);
            println!("正常运行模式将在 T7 接通：client.exe --host <ip>:<port>");
        }
    }
}
