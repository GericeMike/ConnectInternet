//! rdlink 主控端：QUIC 收流 → 解码 → wgpu 渲染；捕获键鼠发往被控端。

mod clipboard;
mod decode_demo;
mod decoder;
mod display;
mod filex;
mod input_map;
mod panel;
mod render_demo;
mod stream;
mod upload_cmd;

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
            panel::spawn();
            stream::run(&addr, &pin);
        }
        // M3-2 调试/运维：client --upload <本机文件>（连 rdlink.toml 默认被控端，落其 Downloads）
        Some("--upload") => {
            let path = args.get(2).cloned().ok_or("用法: client --upload <文件>").unwrap();
            upload_cmd::run_upload(path);
        }
        // M3-2 调试/运维：client --download <被控端 Downloads 中的文件名>
        Some("--download") => {
            let name = args.get(2).cloned().ok_or("用法: client --download <文件名>").unwrap();
            upload_cmd::run_download(name);
        }
        _ => {
            // 无参数：从 rdlink.toml [client] 读默认连接（host + fingerprint），
            // 支持双击 连接被控端.bat 直接启动
            #[derive(serde::Deserialize, Default)]
            #[serde(default)]
            struct DefaultConn {
                host: Option<String>,
                fingerprint: Option<String>,
            }
            let conn: DefaultConn = std::fs::read_to_string("rdlink.toml")
                .ok()
                .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
                .and_then(|v| v.get("client").cloned())
                .and_then(|c| c.try_into::<DefaultConn>().ok())
                .unwrap_or_default();
            match (conn.host, conn.fingerprint) {
                (Some(addr), Some(pin)) => stream::run(&addr, &pin),
                _ => {
                    println!("rdlink-client {} (主控端)", env!("CARGO_PKG_VERSION"));
                    println!("运行模式: client --host <ip:port> <指纹>");
                    println!("或在 rdlink.toml [client] 配置 host + fingerprint 后无参数启动");
                    println!("辅助: --render-demo [vsync] | --decode-demo <文件>");
                }
            }
        }
    }
}
