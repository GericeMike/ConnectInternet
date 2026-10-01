//! M3-2 调试/运维子命令：`client --upload <文件>` / `client --download <文件名>`。
//! 连接 rdlink.toml 默认被控端，独立完成一次传输（无 UI）。
//! 带 500ms 心跳（host 侧 3s 失联看门狗需要）。

use rdlink_proto::{ControlMsg, Message};
use rdlink_transport::quinn;
use rdlink_transport::{connect, read_frame, write_frame};

fn toml_conn() -> (std::net::SocketAddr, String, Option<String>) {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct DefaultConn {
        host: Option<String>,
        fingerprint: Option<String>,
        password: Option<String>,
    }
    let conn: DefaultConn = std::fs::read_to_string("rdlink.toml")
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
        .and_then(|v| v.get("client").cloned())
        .and_then(|c| c.try_into::<DefaultConn>().ok())
        .unwrap_or_default();
    let addr: std::net::SocketAddr = conn
        .host
        .expect("rdlink.toml [client] host 未配置")
        .parse()
        .expect("地址格式");
    (
        addr,
        conn.fingerprint.expect("rdlink.toml [client] fingerprint 未配置"),
        conn.password,
    )
}

async fn connected_session() -> quinn::Connection {
    let (addr, pin, password) = toml_conn();
    let session = connect(addr, &pin, "rdlink-cmd", password.as_deref())
        .await
        .expect("连接被控端失败");
    println!("已连接: {}（会话保持中，500ms 心跳）", session.peer_name);
    // host 3s 无 Ping 看门狗需要心跳；顺带消费 Pong/control 消息防流积压
    let rdlink_transport::ClientSession {
        control_send: mut cs,
        control_recv: mut cr,
        connection,
        ..
    } = session;
    tokio::spawn(async move {
        loop {
            if write_frame(&mut cs, &Message::Control(ControlMsg::Ping { t_us: 0 }))
                .await
                .is_err()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });
    tokio::spawn(async move {
        while let Ok(Some(_)) = read_frame(&mut cr).await {}
    });
    connection
}

pub fn run_upload(path: String) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async move {
        let conn = connected_session().await;
        match crate::filex::debug_upload(conn, path.into()).await {
            Ok(()) => println!("上传完成"),
            Err(e) => {
                eprintln!("上传失败: {e}");
                std::process::exit(1);
            }
        }
    });
}

pub fn run_download(name: String) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async move {
        let conn = connected_session().await;
        match crate::filex::debug_download(conn, name).await {
            Ok(dest) => println!("下载完成 → {}", dest.display()),
            Err(e) => {
                eprintln!("下载失败: {e}");
                std::process::exit(1);
            }
        }
    });
}

/// M3-4 调试/运维：列出被控端进程
pub fn run_procs() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async move {
        let conn = connected_session().await;
        match crate::filex::debug_procs(conn).await {
            Ok(entries) => {
                println!("{:<8} {:<28} {:>8} {:>10}", "PID", "名称", "CPU%", "内存MB");
                for e in entries.iter().take(40) {
                    println!("{:<8} {:<28} {:>8.1} {:>10.0}", e.pid, e.name, e.cpu, e.mem_mb);
                }
                println!("（共 {} 项，仅显示前 40）", entries.len());
            }
            Err(e) => {
                eprintln!("进程列表失败: {e}");
                std::process::exit(1);
            }
        }
    });
}

/// M3-4 调试/运维：结束指定进程
pub fn run_kill(pid: u32) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async move {
        let conn = connected_session().await;
        match crate::filex::debug_kill(conn, pid).await {
            Ok(()) => println!("已结束进程 {pid}"),
            Err(e) => {
                eprintln!("结束失败: {e}");
                std::process::exit(1);
            }
        }
    });
}
