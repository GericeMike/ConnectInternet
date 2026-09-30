//! M3-1：剪贴板同步（client 侧）。
//!
//! 与 host 侧同一套防回环模型：单一同步指纹 LAST_SYNCED，无论"本端轮询到变化"
//! 还是"收到对端写入"，先比对一致即忽略。
//! - poll 线程：arboard 每 500ms 轮询（Windows 无进程间剪贴板事件可安全跨库监听，
//!   轮询最简且对桌面场景足够）→ 变化经 watch 通道给控制循环；
//! - write 线程：std mpsc 收对端同步 → arboard 写入。

use std::sync::atomic::{AtomicU64, Ordering};

static LAST_SYNCED: AtomicU64 = AtomicU64::new(0);

/// 对端文本 1 MiB 上限（与 host 一致）
pub const MAX_CLIP_TEXT: usize = 1024 * 1024;

pub struct ClipboardBridge {
    /// 本端变化（poll 线程 → 控制循环），初始 (0, "")
    pub changes: tokio::sync::watch::Receiver<(u64, String)>,
    /// 对端写入请求（控制循环 → write 线程）
    pub write_tx: std::sync::mpsc::Sender<(u64, String)>,
}

pub fn spawn() -> ClipboardBridge {
    let (write_tx, write_rx) = std::sync::mpsc::channel::<(u64, String)>();
    let (changes_tx, changes_rx) = tokio::sync::watch::channel((0u64, String::new()));

    // 写线程：对端剪贴板 → 本机
    std::thread::Builder::new()
        .name("rdlink-clip-write".into())
        .spawn(move || {
            let Ok(mut board) = arboard::Clipboard::new() else {
                eprintln!("[clip] 剪贴板不可用（写方向关闭）");
                return;
            };
            while let Ok((hash, text)) = write_rx.recv() {
                LAST_SYNCED.store(hash, Ordering::SeqCst);
                println!("[clip] 对端剪贴板写入 {} 字节", text.len());
                if let Err(e) = board.set_text(text) {
                    eprintln!("[clip] 写入剪贴板失败: {e}");
                }
            }
        })
        .expect("剪贴板写线程创建失败");

    // 轮询线程：本机剪贴板 → 对端
    std::thread::Builder::new()
        .name("rdlink-clip-poll".into())
        .spawn(move || {
            let Ok(mut board) = arboard::Clipboard::new() else {
                eprintln!("[clip] 剪贴板不可用（读方向关闭）");
                return;
            };
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                // 剪贴板被其他程序占用/为空时 get_text 报错——静默跳过本轮
                let Ok(text) = board.get_text() else { continue };
                if text.is_empty() || text.len() > MAX_CLIP_TEXT {
                    continue;
                }
                let hash = rdlink_proto::fnv1a64(text.as_bytes());
                if hash != LAST_SYNCED.load(Ordering::SeqCst) {
                    LAST_SYNCED.store(hash, Ordering::SeqCst);
                    println!("[clip] 本端剪贴板变化 hash={hash:016x} {} 字节", text.len());
                    let _ = changes_tx.send((hash, text));
                }
            }
        })
        .expect("剪贴板轮询线程创建失败");

    ClipboardBridge { changes: changes_rx, write_tx }
}
