//! M3-2：文件传输（host 侧服务端）。
//!
//! 每个传输操作一条独立 bi stream（流隔离天然免掉消息 id/交织问题）：
//! - ListReq  → ListReply（列 host Downloads，M3 固定目录）
//! - Request{Up}   → Accept → Chunk* → Done{sha} → Ok/Fail
//! - Request{Down} → Accept → Chunk* → Done{sha}（client 校验后回 Ok/Fail）
//!
//! 安全：文件名仅允许纯文件名（拒绝路径分隔符/..），一律落在 Downloads 内。
//! 上传重名自动追加 (1)/(2)；上传完成即校验 SHA-256，不符删除并报 Fail。

use std::path::{Path, PathBuf};

use rdlink_proto::{FileEntry, FileMsg, XferDir};
use rdlink_transport::quinn;
use rdlink_transport::{read_frame_of, write_frame_of};

/// host 下载目录：rdlink.toml [host] download_dir 覆盖，默认 %USERPROFILE%\Downloads
pub fn downloads_dir() -> PathBuf {
    let conf: Option<String> = std::fs::read_to_string("rdlink.toml")
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
        .and_then(|v| v.get("host").cloned())
        .and_then(|h| h.get("download_dir").cloned())
        .and_then(|d| d.as_str().map(String::from));
    conf.map(PathBuf::from).unwrap_or_else(|| {
        let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Public".into());
        Path::new(&profile).join("Downloads")
    })
}

/// 文件流服务循环：随连接生命周期，连接断开自然退出。
pub async fn serve(connection: quinn::Connection) {
    loop {
        let (send, recv) = match connection.accept_bi().await {
            Ok(x) => x,
            Err(_) => break, // 连接关闭
        };
        tokio::spawn(async move {
            if let Err(e) = handle_stream(send, recv).await {
                println!("[file] 流处理结束: {e}");
            }
        });
    }
}

async fn handle_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> Result<(), String> {
    let first: Option<FileMsg> =
        read_frame_of(&mut recv).await.map_err(|e| e.to_string())?;
    match first {
        Some(FileMsg::ListReq { .. }) => {
            let entries = list_downloads();
            write_frame_of(&mut send, &FileMsg::ListReply { entries })
                .await
                .map_err(|e| e.to_string())?;
            let _ = send.finish();
            Ok(())
        }
        Some(FileMsg::Request { dir: XferDir::Up, name, size }) => {
            upload(&mut recv, &mut send, name, size).await
        }
        Some(FileMsg::Request { dir: XferDir::Down, name, size }) => {
            download(&mut recv, &mut send, name, size).await
        }
        other => Err(format!("文件流首帧非法: {other:?}")),
    }
}

fn list_downloads() -> Vec<FileEntry> {
    let dir = downloads_dir();
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            let Some(name) = e.file_name().to_str().map(String::from) else { continue };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.push(FileEntry {
                name,
                size: meta.len(),
                is_dir: meta.is_dir(),
                mtime,
            });
        }
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    out
}

/// 仅允许纯文件名（拒路径分隔符与 ..），传输一律落在 Downloads 内
fn safe_name(name: &str) -> Option<String> {
    let n = name.trim();
    if n.is_empty()
        || n.contains('/')
        || n.contains('\\')
        || n.contains("..")
        || n.starts_with('.')
    {
        return None;
    }
    Some(n.to_string())
}

/// 重名追加 " (1)"、" (2)"…
fn dedup_path(dir: &Path, name: &str) -> PathBuf {
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

async fn upload(
    recv: &mut quinn::RecvStream,
    send: &mut quinn::SendStream,
    name: String,
    size: u64,
) -> Result<(), String> {
    let Some(name) = safe_name(&name) else {
        write_frame_of(send, &FileMsg::Reject { reason: "非法文件名".into() })
            .await
            .map_err(|e| e.to_string())?;
        return Err("上传被拒: 非法文件名".into());
    };
    let dir = downloads_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dedup_path(&dir, &name);
    println!("[file] 上传开始: {}（{} B）→ {}", name, size, path.display());

    write_frame_of(send, &FileMsg::Accept)
        .await
        .map_err(|e| e.to_string())?;

    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    let mut file = std::fs::File::create(&path).map_err(|e| format!("建文件失败: {e}"))?;
    let mut expected: u64 = 0;
    let mut sha_hex = String::new();
    loop {
        let frame: Option<FileMsg> = read_frame_of(recv).await.map_err(|e| e.to_string())?;
        match frame {
            Some(FileMsg::Chunk { offset, data }) => {
                if offset != expected {
                    let msg = format!("块偏移不连续: 期望 {expected} 收到 {offset}");
                    let _ = write_frame_of(send, &FileMsg::Fail { reason: msg.clone() }).await;
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(msg);
                }
                hasher.update(&data);
                use std::io::Write;
                file.write_all(&data).map_err(|e| format!("写文件失败: {e}"))?;
                expected += data.len() as u64;
            }
            Some(FileMsg::Done { sha256 }) => {
                let actual = format!("{:x}", hasher.finalize());
                drop(file);
                if actual == sha256 {
                    sha_hex = actual;
                    write_frame_of(send, &FileMsg::Ok).await.map_err(|e| e.to_string())?;
                } else {
                    let _ = std::fs::remove_file(&path);
                    let msg = format!("SHA-256 不符: 期望 {sha256} 实际 {actual}");
                    let _ = write_frame_of(send, &FileMsg::Fail { reason: msg.clone() }).await;
                    return Err(msg);
                }
                break;
            }
            other => {
                drop(file);
                let _ = std::fs::remove_file(&path);
                return Err(format!("上传流中断: {other:?}"));
            }
        }
    }
    println!(
        "[file] 上传完成: {}（{} B, sha256 {}…）",
        path.display(),
        expected,
        &sha_hex[..8.min(sha_hex.len())]
    );
    Ok(())
}

async fn download(
    recv: &mut quinn::RecvStream,
    send: &mut quinn::SendStream,
    name: String,
    req_size: u64,
) -> Result<(), String> {
    let Some(name) = safe_name(&name) else {
        write_frame_of(send, &FileMsg::Reject { reason: "非法文件名".into() })
            .await
            .map_err(|e| e.to_string())?;
        return Err("下载被拒: 非法文件名".into());
    };
    let path = downloads_dir().join(&name);
    let meta = std::fs::metadata(&path);
    let Ok(meta) = meta else {
        write_frame_of(send, &FileMsg::Reject { reason: "文件不存在".into() })
            .await
            .map_err(|e| e.to_string())?;
        return Err("下载被拒: 文件不存在".into());
    };
    if meta.is_dir() || meta.len() != req_size {
        write_frame_of(send, &FileMsg::Reject { reason: "文件大小与请求不符".into() })
            .await
            .map_err(|e| e.to_string())?;
        return Err("下载被拒: 大小不符".into());
    }
    write_frame_of(send, &FileMsg::Accept)
        .await
        .map_err(|e| e.to_string())?;
    println!("[file] 下载开始: {}（{} B）", path.display(), req_size);

    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    let data = std::fs::read(&path).map_err(|e| format!("读文件失败: {e}"))?;
    for (offset, chunk) in data.chunks(rdlink_proto::FILE_CHUNK).enumerate() {
        hasher.update(chunk);
        write_frame_of(
            send,
            &FileMsg::Chunk { offset: (offset * rdlink_proto::FILE_CHUNK) as u64, data: chunk.to_vec() },
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    let sha = format!("{:x}", hasher.finalize());
    write_frame_of(send, &FileMsg::Done { sha256: sha.clone() })
        .await
        .map_err(|e| e.to_string())?;
    // 等接收方校验结果（尽力而为，超时不等）
    let result: Result<Option<FileMsg>, String> =
        tokio::time::timeout(std::time::Duration::from_secs(5), read_frame_of(recv))
            .await
            .map_err(|_| "等待校验结果超时".into())
            .and_then(|r| r.map_err(|e| e.to_string()));
    match result {
        Ok(Some(FileMsg::Ok)) => println!("[file] 下载完成: {name}（{req_size} B）"),
        Ok(Some(FileMsg::Fail { reason })) => println!("[file] 下载校验失败: {reason}"),
        other => println!("[file] 下载收尾异常: {other:?}"),
    }
    Ok(())
}
