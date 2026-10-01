//! M3-1：剪贴板同步（host 侧）。
//!
//! 结构：双 OS 线程 + 单一同步指纹（LAST_SYNCED 原子量）防回环。
//! - poll 线程：每 500ms 读取本机剪贴板，变化经 watch 通道给 serve 控制循环；
//! - write 线程：std mpsc 阻塞收（serve 转发来的对端剪贴板）→ SetClipboardData；
//! - 防回环：无论"本端轮询到变化"还是"收到对端写入"，都与 LAST_SYNCED 比对，
//!   一致即忽略——本端写入引发的回波因此被天然抑制。
//!
//! 为什么轮询而不是 AddClipboardFormatListener：两种窗口形态（message-only /
//! 隐形顶层）在被控端 Win10 19045 上都收不到 WM_CLIPBOARDUPDATE 广播（本机
//! Win11 正常，系统差异；排障记录见 docs/M3-任务拆解.md），而轮询与 client
//! 侧完全同构、跨系统行为一致。500ms 一次 OpenClipboard 微秒级，成本可忽略；
//! 剪贴板被占用时读取失败按本轮跳过处理。

use std::sync::atomic::{AtomicU64, Ordering};

/// 全进程同步指纹：最近一次"已知的剪贴板内容"（本端发出或对端写入）
static LAST_SYNCED: AtomicU64 = AtomicU64::new(0);

/// 对端文本 1 MiB 上限（防链路冲击；超限发送方直接丢弃）
pub const MAX_CLIP_TEXT: usize = 1024 * 1024;

pub struct ClipboardHub {
    /// 本端剪贴板变化（poll 线程 → serve 控制循环），初始 (0, "")
    pub changes: tokio::sync::watch::Receiver<(u64, String)>,
    /// 对端写入请求（serve 控制循环 → write 线程）
    pub write_tx: std::sync::mpsc::Sender<(u64, String)>,
}

/// 启动剪贴板双线程。剪贴板不可用时降级：打印警告，方向静默失效（不 panic）。
pub fn spawn() -> ClipboardHub {
    let (write_tx, write_rx) = std::sync::mpsc::channel::<(u64, String)>();
    let (changes_tx, changes_rx) = tokio::sync::watch::channel((0u64, String::new()));

    // 写线程：对端剪贴板 → 本机（带持久重试，见内注释）
    std::thread::Builder::new()
        .name("rdlink-clip-write".into())
        .spawn(move || {
            while let Ok((hash, text)) = write_rx.recv() {
                LAST_SYNCED.store(hash, Ordering::SeqCst);
                println!("[clip] 对端剪贴板写入 {} 字节", text.len());
                // 剪贴板被其他程序（远控/输入法/微信类常驻）频繁占是常态，
                // 8×300ms 持久重试抢窗口期；全部失败则放弃（日志留痕）
                let mut ok = false;
                for attempt in 1..=8 {
                    match write_clipboard_text(&text) {
                        Ok(()) => {
                            ok = true;
                            break;
                        }
                        Err(e) => {
                            if attempt == 1 || attempt == 8 {
                                eprintln!("[clip] 写入第 {attempt} 次失败: {e}");
                            }
                            std::thread::sleep(std::time::Duration::from_millis(300));
                        }
                    }
                }
                if !ok {
                    eprintln!("[clip] 放弃本次写入（环境争抢过烈）");
                }
            }
        })
        .expect("剪贴板写线程创建失败");

    // 轮询线程：本机剪贴板 → 对端
    std::thread::Builder::new()
        .name("rdlink-clip-poll".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let Some(text) = read_clipboard_text() else { continue };
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

    ClipboardHub { changes: changes_rx, write_tx }
}

// ---------- 剪贴板读写原语 ----------

/// 读取本机剪贴板文本（轮询与 `--get-clip` 调试子命令复用）。
/// 剪贴板被占用/为空/非文本 → None。
pub fn read_clipboard_text() -> Option<String> {
    unsafe {
        use windows::Win32::Foundation::HGLOBAL;
        use windows::Win32::System::DataExchange::*;
        use windows::Win32::System::Memory::*;
        use windows::Win32::System::Ole::CF_UNICODETEXT;
        OpenClipboard(None).ok()?;
        let text = (|| {
            let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            let hg = HGLOBAL(h.0);
            let ptr = GlobalLock(hg) as *const u16;
            if ptr.is_null() {
                return None;
            }
            // 以 nul 截断
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let wide = std::slice::from_raw_parts(ptr, len);
            let s = String::from_utf16_lossy(wide);
            let _ = GlobalUnlock(hg);
            Some(s)
        })();
        let _ = CloseClipboard();
        text
    }
}

/// 写本机剪贴板文本（write 线程与 `--set-clip` 调试子命令复用）。
/// 内部对 OpenClipboard 做了 20×50ms 重试（剪贴板是易争抢的共享资源）。
pub fn write_clipboard_text(text: &str) -> windows::core::Result<()> {
    unsafe {
        use windows::Win32::Foundation::{GlobalFree, HANDLE};
        use windows::Win32::System::DataExchange::*;
        use windows::Win32::System::Memory::*;
        use windows::Win32::System::Ole::CF_UNICODETEXT;
        // 剪贴板是易争抢的共享资源（剪贴板管理器/输入法/其他远控都常驻打开它），
        // OpenClipboard 失败重试是标准做法（Raymond Chen 背书）
        let mut opened = false;
        for _ in 0..20 {
            if OpenClipboard(None).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if !opened {
            let msg = format!("OpenClipboard 重试 20×50ms 后仍失败: {}", windows::core::Error::from_thread());
            return Err(windows::core::Error::new(windows::core::HRESULT(0x8007_0005u32 as i32), msg));
        }
        let r = (|| -> Result<(), String> {
            EmptyClipboard().map_err(|e| format!("EmptyClipboard: {e}"))?;
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            wide.push(0);
            let bytes = wide.len() * 2;
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|e| format!("GlobalAlloc: {e}"))?;
            let ptr = GlobalLock(h) as *mut u16;
            if ptr.is_null() {
                let _ = GlobalFree(Some(h));
                return Err("GlobalLock: null".into());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(h);
            // 成功后所有权归系统；失败要自己释放
            match SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(h.0))) {
                Ok(_) => Ok(()),
                Err(e) => {
                    let _ = GlobalFree(Some(h));
                    Err(format!("SetClipboardData: {e}"))
                }
            }
        })();
        let _ = CloseClipboard();
        r.map_err(|s| windows::core::Error::new(windows::core::HRESULT(0x8007_0005u32 as i32), s))
    }
}
