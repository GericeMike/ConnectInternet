//! M3-2：文件传输（client 侧）。
//!
//! 三个角色：
//! - **请求入口**：控制面板（刷新/下载）与 rdlink 窗口拖拽（上传）都调 [`submit`]；
//! - **连接槽**：会话建立时 [`set_connection`] 放入 QUIC 连接，断开时清除——
//!   面板/拖拽在未连接时提交请求会得到"未连接"提示；
//! - **管理任务**：[`manager`] 逐个消费请求（串行，进度互不干扰），经事件通道
//!   把列表/进度/状态推给面板线程刷新 UI。
//!
//! 上传：拖到 rdlink 窗口的文件 → 被控端 Downloads（重名自动 (1)）。
//! 下载：面板列表勾选 → 主控端 Downloads。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

use rdlink_proto::{FileEntry, FileMsg, XferDir};
use rdlink_transport::quinn;
use rdlink_transport::{read_frame_of, write_frame_of};

#[derive(Debug, Clone)]
pub enum XferRequest {
    Upload(PathBuf),
    Download { name: String, size: u64 },
    Refresh,
}

/// 面板事件（面板线程消费并刷新控件）
#[derive(Debug, Clone)]
pub enum PanelEvent {
    /// host Downloads 文件列表
    List(Vec<FileEntry>),
    /// 状态行文本
    Status(String),
    /// 进度（标签/当前/总量，字节）
    Progress { label: String, current: u64, total: u64 },
}

static REQ_TX: RwLock<Option<tokio::sync::mpsc::UnboundedSender<XferRequest>>> =
    RwLock::new(None);
static CONN: RwLock<Option<quinn::Connection>> = RwLock::new(None);
static BUSY: AtomicBool = AtomicBool::new(false);
static PANEL_EV: RwLock<Option<std::sync::mpsc::Sender<PanelEvent>>> = RwLock::new(None);

/// 面板线程注册事件接收端（面板侧持 Receiver，定时排空）
pub fn set_panel_events(tx: std::sync::mpsc::Sender<PanelEvent>) {
    *PANEL_EV.write().unwrap() = Some(tx);
}

/// 提交请求。返回 false = 没有管理任务在跑（client 未启动/未连接场景外）。
pub fn submit(req: XferRequest) -> bool {
    match REQ_TX.read().unwrap().as_ref() {
        Some(tx) => {
            tx.send(req).is_ok()
        }
        None => false,
    }
}

/// 会话建立时注入 QUIC 连接
pub fn set_connection(conn: quinn::Connection) {
    *CONN.write().unwrap() = Some(conn);
}

/// 会话结束时清除
pub fn clear_connection() {
    *CONN.write().unwrap() = None;
}

/// 启动管理任务（stream_loop 进程级调用一次）
pub fn spawn_manager() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<XferRequest>();
    *REQ_TX.write().unwrap() = Some(tx);
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let conn = CONN.read().unwrap().clone();
            let Some(conn) = conn else {
                send_event(PanelEvent::Status("未连接：请先连接被控端再传输".into()));
                continue;
            };
            if BUSY.swap(true, Ordering::SeqCst) {
                send_event(PanelEvent::Status("已有传输在进行，请稍候".into()));
                // 等前一个完成再处理本条（串行化：简单且进度不互踩）
                while BUSY.load(Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
            let r = match req {
                XferRequest::Refresh => do_list(&conn).await,
                XferRequest::Upload(path) => do_upload(&conn, path).await,
                XferRequest::Download { name, size } => do_download(&conn, name, size).await,
            };
            BUSY.store(false, Ordering::SeqCst);
            if let Err(e) = r {
                send_event(PanelEvent::Status(format!("失败: {e}")));
            }
        }
    });
}

fn send_event(ev: PanelEvent) {
    if let PanelEvent::Status(s) = &ev {
        eprintln!("[file] {s}");
    }
    if let Some(tx) = PANEL_EV.read().unwrap().as_ref() {
        let _ = tx.send(ev);
    }
}

/// 传输是否进行中（调试子命令等待用）
pub fn busy() -> bool {
    BUSY.load(Ordering::SeqCst)
}

/// 调试/运维：列出被控端 Downloads
pub async fn debug_list(conn: quinn::Connection) -> Result<Vec<FileEntry>, String> {
    let (mut send, mut recv) = open_stream(&conn).await?;
    write_frame_of(&mut send, &FileMsg::ListReq { path: ".".into() })
        .await
        .map_err(|e| e.to_string())?;
    let reply: Option<FileMsg> = read_frame_of(&mut recv).await.map_err(|e| e.to_string())?;
    match reply {
        Some(FileMsg::ListReply { entries }) => Ok(entries),
        other => Err(format!("列表应答异常: {other:?}")),
    }
}

/// 调试/运维：直接下载被控端 Downloads 中的文件到主控端 Downloads
pub async fn debug_download(conn: quinn::Connection, name: String) -> Result<PathBuf, String> {
    let entries = debug_list(conn.clone()).await?;
    let size = entries
        .iter()
        .find(|e| e.name == name && !e.is_dir)
        .map(|e| e.size)
        .ok_or_else(|| format!("被控端 Downloads 中无 {name}"))?;
    do_download(&conn, name.clone(), size).await?;
    Ok(client_downloads_dir().join(name))
}

/// 调试/运维：上传本地文件到被控端 Downloads
pub async fn debug_upload(conn: quinn::Connection, path: PathBuf) -> Result<(), String> {
    do_upload(&conn, path).await
}

/// 主控端下载落地目录：rdlink.toml [client] download_dir 覆盖，默认 Downloads
pub fn client_downloads_dir() -> PathBuf {
    let conf: Option<String> = std::fs::read_to_string("rdlink.toml")
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
        .and_then(|v| v.get("client").cloned())
        .and_then(|c| c.get("download_dir").cloned())
        .and_then(|d| d.as_str().map(String::from));
    conf.map(PathBuf::from).unwrap_or_else(|| {
        let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Public".into());
        Path::new(&profile).join("Downloads")
    })
}

async fn open_stream(conn: &quinn::Connection) -> Result<(quinn::SendStream, quinn::RecvStream), String> {
    conn.open_bi().await.map_err(|e| format!("开流失败: {e}"))
}

async fn do_list(conn: &quinn::Connection) -> Result<(), String> {
    let (mut send, mut recv) = open_stream(conn).await?;
    write_frame_of(&mut send, &FileMsg::ListReq { path: ".".into() })
        .await
        .map_err(|e| e.to_string())?;
    let reply: Option<FileMsg> = read_frame_of(&mut recv).await.map_err(|e| e.to_string())?;
    match reply {
        Some(FileMsg::ListReply { entries }) => {
            send_event(PanelEvent::List(entries));
            Ok(())
        }
        Some(other) => Err(format!("列表应答异常: {other:?}")),
        None => Err("列表应答为空".into()),
    }
}

fn human(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / 1048576.0)
    } else {
        format!("{:.0} KiB", bytes as f64 / 1024.0)
    }
}

async fn do_upload(conn: &quinn::Connection, path: PathBuf) -> Result<(), String> {
    let meta = std::fs::metadata(&path).map_err(|e| format!("读取文件失败: {e}"))?;
    if meta.is_dir() {
        return Err("暂不支持整个文件夹（请拖单个文件）".into());
    }
    let size = meta.len();
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("路径无文件名")?
        .to_string();
    let mut file = std::fs::File::open(&path).map_err(|e| format!("打开文件失败: {e}"))?;

    let (mut send, mut recv) = open_stream(conn).await?;
    write_frame_of(&mut send, &FileMsg::Request { dir: XferDir::Up, name: name.clone(), size })
        .await
        .map_err(|e| e.to_string())?;
    let ack: Option<FileMsg> = read_frame_of(&mut recv).await.map_err(|e| e.to_string())?;
    match ack {
        Some(FileMsg::Accept) => {}
        Some(FileMsg::Reject { reason }) => return Err(format!("被控端拒绝: {reason}")),
        Some(other) => return Err(format!("应答异常: {other:?}")),
        None => return Err("连接中断".into()),
    }

    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    let mut offset: u64 = 0;
    let mut buf = vec![0u8; rdlink_proto::FILE_CHUNK];
    let mut last_prog = std::time::Instant::now();
    send_event(PanelEvent::Progress {
        label: format!("上传 {name}"),
        current: 0,
        total: size,
    });
    loop {
        use std::io::Read;
        let n = file.read(&mut buf).map_err(|e| format!("读文件失败: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        write_frame_of(
            &mut send,
            &FileMsg::Chunk { offset, data: buf[..n].to_vec() },
        )
        .await
        .map_err(|e| e.to_string())?;
        offset += n as u64;
        if last_prog.elapsed() >= std::time::Duration::from_millis(250) {
            send_event(PanelEvent::Progress {
                label: format!("上传 {name}"),
                current: offset,
                total: size,
            });
            last_prog = std::time::Instant::now();
        }
    }
    let sha = format!("{:x}", hasher.finalize());
    write_frame_of(&mut send, &FileMsg::Done { sha256: sha })
        .await
        .map_err(|e| e.to_string())?;
    let verdict: Option<FileMsg> = read_frame_of(&mut recv).await.map_err(|e| e.to_string())?;
    match verdict {
        Some(FileMsg::Ok) => {
            send_event(PanelEvent::Status(format!("上传完成: {name} → 被控端 Downloads")));
            send_event(PanelEvent::Progress {
                label: format!("上传 {name}"),
                current: size,
                total: size,
            });
            Ok(())
        }
        Some(FileMsg::Fail { reason }) => Err(format!("校验失败: {reason}")),
        other => Err(format!("校验应答异常: {other:?}")),
    }
}

async fn do_download(conn: &quinn::Connection, name: String, size: u64) -> Result<(), String> {
    let (mut send, mut recv) = open_stream(conn).await?;
    write_frame_of(&mut send, &FileMsg::Request { dir: XferDir::Down, name: name.clone(), size })
        .await
        .map_err(|e| e.to_string())?;
    let ack: Option<FileMsg> = read_frame_of(&mut recv).await.map_err(|e| e.to_string())?;
    match ack {
        Some(FileMsg::Accept) => {}
        Some(FileMsg::Reject { reason }) => return Err(format!("被控端拒绝: {reason}")),
        Some(other) => return Err(format!("应答异常: {other:?}")),
        None => return Err("连接中断".into()),
    }

    let dest = dedup_local(&client_downloads_dir(), &name);
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = std::fs::File::create(&dest).map_err(|e| format!("建文件失败: {e}"))?;

    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    let mut got: u64 = 0;
    let mut last_prog = std::time::Instant::now();
    send_event(PanelEvent::Progress {
        label: format!("下载 {name}"),
        current: 0,
        total: size,
    });
    loop {
        let frame: Option<FileMsg> = read_frame_of(&mut recv).await.map_err(|e| e.to_string())?;
        match frame {
            Some(FileMsg::Chunk { offset, data }) => {
                if offset != got {
                    let msg = format!("块偏移不连续: 期望 {got} 收到 {offset}");
                    let _ = write_frame_of(&mut send, &FileMsg::Fail { reason: msg.clone() }).await;
                    drop(file);
                    let _ = std::fs::remove_file(&dest);
                    return Err(msg);
                }
                hasher.update(&data);
                use std::io::Write;
                file.write_all(&data).map_err(|e| format!("写文件失败: {e}"))?;
                got += data.len() as u64;
                if last_prog.elapsed() >= std::time::Duration::from_millis(250) {
                    send_event(PanelEvent::Progress {
                        label: format!("下载 {name}"),
                        current: got,
                        total: size,
                    });
                    last_prog = std::time::Instant::now();
                }
            }
            Some(FileMsg::Done { sha256 }) => {
                let actual = format!("{:x}", hasher.finalize());
                drop(file);
                if actual != sha256 {
                    let _ = std::fs::remove_file(&dest);
                    let msg = format!("SHA-256 不符: 期望 {sha256} 实际 {actual}");
                    let _ = write_frame_of(&mut send, &FileMsg::Fail { reason: msg.clone() }).await;
                    return Err(msg);
                }
                let _ = write_frame_of(&mut send, &FileMsg::Ok).await;
                send_event(PanelEvent::Status(format!(
                    "下载完成: {name} → {}（{}）",
                    dest.display(),
                    human(size)
                )));
                send_event(PanelEvent::Progress {
                    label: format!("下载 {name}"),
                    current: size,
                    total: size,
                });
                return Ok(());
            }
            other => {
                drop(file);
                let _ = std::fs::remove_file(&dest);
                return Err(format!("下载流中断: {other:?}"));
            }
        }
    }
}

fn dedup_local(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let stem = Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = Path::new(name).extension().and_then(|s| s.to_str());
    for i in 1..1000u32 {
        let cand = match ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let p = dir.join(&cand);
        if !p.exists() {
            return p;
        }
    }
    dir.join(format!("{name}.dup"))
}
