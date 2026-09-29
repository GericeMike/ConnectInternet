//! rdlink 主控端：QUIC 收流 → 解码 → wgpu 渲染；捕获键鼠发往被控端。

mod decode_demo;
mod decoder;
mod display;
mod input_map;
mod render_demo;
mod stream;

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
            // vsync 模式用于对照测量（决策 D3 默认 no-vsync）
            let vsync = args.get(2).map(String::as_str) == Some("vsync");
            render_demo::run(vsync);
        }
        Some("--host") => {
            // 正常运行模式（T7）：client --host <ip:port> <证书指纹>
            let (addr, pin) = match (args.get(2), args.get(3)) {
                (Some(a), Some(p)) => (a.clone(), p.clone()),
                _ => {
                    eprintln!("用法: client --host <ip:port> <证书指纹>（指纹看 host 启动输出）");
                    std::process::exit(2);
                }
            };
            stream::run(&addr, &pin);
        }
        _ => {
            println!("rdlink-client {} (主控端)", env!("CARGO_PKG_VERSION"));
            println!("运行模式: client --host <ip:port> <指纹>");
            println!("辅助: --render-demo [vsync] | --decode-demo <文件>");
        }
    }
}
