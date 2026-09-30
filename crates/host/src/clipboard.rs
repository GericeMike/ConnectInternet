//! M3-1：剪贴板同步（host 侧）。
//!
//! 结构：双 OS 线程 + 单一同步指纹（LAST_SYNCED 原子量）防回环。
//! - listen 线程：message-only 窗口 + AddClipboardFormatListener，事件驱动收变化，
//!   变化经 watch 通道给 serve 控制循环（同步给 client）；
//! - write 线程：std mpsc 阻塞收（serve 转发来的对端剪贴板）→ SetClipboardData；
//! - 防回环：无论"本端监听到变化"还是"收到对端写入"，都与 LAST_SYNCED 比对，
//!   一致即忽略——本端写入引发的监听回波因此被天然抑制。
//!
//! 剪贴板必须在使用线程内有消息泵（listener 回调经窗口消息派发），listen 窗口
//! 建在 listen 线程；写线程无窗口（OpenClipboard(None)），各自独立无锁冲突。

use std::sync::atomic::{AtomicU64, Ordering};

/// 全进程同步指纹：最近一次"已知的剪贴板内容"（本端发出或对端写入）
static LAST_SYNCED: AtomicU64 = AtomicU64::new(0);

/// 对端文本 1 MiB 上限（防链路冲击；超限发送方丢弃）
pub const MAX_CLIP_TEXT: usize = 1024 * 1024;

pub struct ClipboardHub {
    /// 本端剪贴板变化（listen 线程 → serve 控制循环），初始 (0, "")
    pub changes: tokio::sync::watch::Receiver<(u64, String)>,
    /// 对端写入请求（serve 控制循环 → write 线程）
    pub write_tx: std::sync::mpsc::Sender<(u64, String)>,
}

/// 启动剪贴板双线程。监听窗口创建失败时降级：只剩写方向（打印警告，不 panic）。
pub fn spawn() -> ClipboardHub {
    let (write_tx, write_rx) = std::sync::mpsc::channel::<(u64, String)>();
    let (changes_tx, changes_rx) = tokio::sync::watch::channel((0u64, String::new()));

    // 写线程：对端剪贴板 → 本机
    std::thread::Builder::new()
        .name("rdlink-clip-write".into())
        .spawn(move || {
            while let Ok((hash, text)) = write_rx.recv() {
                LAST_SYNCED.store(hash, Ordering::SeqCst);
                println!("[clip] 对端剪贴板写入 {} 字节", text.len());
                if let Err(e) = write_clipboard_text(&text) {
                    eprintln!("[clip] 写入剪贴板失败: {e}");
                }
            }
        })
        .expect("剪贴板写线程创建失败");

    // 监听线程：本机 → 对端
    let tx = changes_tx.clone();
    std::thread::Builder::new()
        .name("rdlink-clip-listen".into())
        .spawn(move || {
            if let Err(e) = listen_loop(tx) {
                eprintln!("[clip] 剪贴板监听不可用（写方向不受影响）: {e}");
            }
        })
        .expect("剪贴板监听线程创建失败");

    ClipboardHub { changes: changes_rx, write_tx }
}

// ---------- 监听线程 ----------

type ChangesTx = tokio::sync::watch::Sender<(u64, String)>;

fn listen_loop(tx: ChangesTx) -> Result<(), String> {
    use windows::Win32::System::DataExchange::AddClipboardFormatListener;
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::*;
        let class_name = windows::core::w!("rdlink_clipboard");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(clip_wndproc),
            lpszClassName: class_name,
            hInstance: windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .map_err(|e| e.to_string())?
                .into(),
            ..Default::default()
        };
        let atom = RegisterClassW(&wc);
        if atom == 0 {
            return Err("RegisterClassW 失败".into());
        }
        // message-only 窗口：不显示、不可聚焦，只收广播消息（含 WM_CLIPBOARDUPDATE）
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            windows::core::w!(""),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
        .map_err(|e| format!("CreateWindowExW: {e}"))?;
        AddClipboardFormatListener(hwnd).map_err(|e| format!("AddClipboardFormatListener: {e}"))?;
        // watch 发送端挂到窗口上，wndproc 经 GWLP_USERDATA 取回（窗口存活期=线程存活期）
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(Box::new(tx)) as _);
        let msg = std::mem::zeroed::<MSG>();
        let mut msg = msg;
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        Ok(())
    }
}

// WM_CLIPBOARDUPDATE 由 windows::Win32::UI::WindowsAndMessaging 导出（wndproc 内 glob 引入）

unsafe extern "system" fn clip_wndproc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    _wparam: windows::Win32::Foundation::WPARAM,
    _lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        if msg == WM_CLIPBOARDUPDATE {
            if let Some(text) = read_clipboard_text() {
                let hash = rdlink_proto::fnv1a64(text.as_bytes());
                let last = LAST_SYNCED.load(Ordering::SeqCst);
                if hash != last && text.len() <= MAX_CLIP_TEXT {
                    LAST_SYNCED.store(hash, Ordering::SeqCst);
                    println!("[clip] 本端剪贴板变化 hash={hash:016x} {} 字节", text.len());
                    // 取窗口 GWLP_USERDATA 里藏的 watch 发送端
                    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ChangesTx;
                    if let Some(tx) = ptr.as_ref() {
                        let _ = tx.send((hash, text));
                    }
                }
            }
            return windows::Win32::Foundation::LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, _wparam, _lparam)
    }
}

// ---------- 剪贴板读写原语 ----------

fn read_clipboard_text() -> Option<String> {
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

fn write_clipboard_text(text: &str) -> windows::core::Result<()> {
    unsafe {
        use windows::Win32::Foundation::{GlobalFree, HANDLE};
        use windows::Win32::System::DataExchange::*;
        use windows::Win32::System::Memory::*;
        use windows::Win32::System::Ole::CF_UNICODETEXT;
        OpenClipboard(None)?;
        let r = (|| -> windows::core::Result<()> {
            EmptyClipboard()?;
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            wide.push(0);
            let bytes = wide.len() * 2;
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
            let ptr = GlobalLock(h) as *mut u16;
            if ptr.is_null() {
                let _ = GlobalFree(Some(h));
                return Err(windows::core::Error::from_thread());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(h);
            // 成功后所有权归系统；失败要自己释放
            match SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(h.0))) {
                Ok(_) => Ok(()),
                Err(e) => {
                    let _ = GlobalFree(Some(h));
                    Err(e)
                }
            }
        })();
        let _ = CloseClipboard();
        r
    }
}
