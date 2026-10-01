//! M3-1/M3-6：剪贴板同步（host 侧）——文本 + 图片（截图）。
//!
//! 结构：双 OS 线程 + 单一同步指纹（LAST_SYNCED 原子量）防回环。
//! - poll 线程：每 500ms 读取本机剪贴板（文本优先，无文本试图片），变化经
//!   watch 通道给 serve 控制循环；
//! - write 线程：std mpsc 阻塞收（serve 转发来的对端剪贴板）→ 写本机剪贴板；
//! - 防回环：无论"本端轮询到变化"还是"收到对端写入"，都与 LAST_SYNCED 比对，
//!   一致即忽略——本端写入引发的回波因此被天然抑制。
//!
//! 为什么轮询而不是 AddClipboardFormatListener：两种窗口形态（message-only /
//! 隐形顶层）在被控端 Win10 19045 上都收不到 WM_CLIPBOARDUPDATE 广播（本机
//! Win11 正常，系统差异；排障记录见 docs/M3-任务拆解.md），而轮询与 client
//! 侧完全同构、跨系统行为一致。500ms 一次 OpenClipboard 微秒级，成本可忽略；
//! 剪贴板被占用时读取失败按本轮跳过处理。
//!
//! 图片链路：剪贴板 CF_DIB/CF_DIBV5 → RGBA（指纹域）→ PNG 编码（传输）；
//! 接收：PNG → RGBA → 构造 CF_DIB 写回。截图类内容 alpha 常为 0，全零时强制不透明。

use std::sync::atomic::{AtomicU64, Ordering};

/// 全进程同步指纹：最近一次"已知的剪贴板内容"（本端发出或对端写入）
static LAST_SYNCED: AtomicU64 = AtomicU64::new(0);

/// 文本上限（防链路冲击）
pub const MAX_CLIP_TEXT: usize = 1024 * 1024;
/// 图片 PNG 上限（防链路冲击；截图 PNG 通常 0.1-2 MiB）
pub const MAX_CLIP_PNG: usize = 8 * 1024 * 1024;

/// 剪贴板负载（watch/mpsc 共用）
#[derive(Clone)]
pub enum ClipPayload {
    Text(String),
    /// width/height 为像素尺寸；rgba 为 w*h*4 的 RGBA8；png 为编码后传输格式
    Image { width: u32, height: u32, rgba: Vec<u8>, png: Vec<u8> },
}

pub fn hash_payload(p: &ClipPayload) -> u64 {
    match p {
        ClipPayload::Text(t) => rdlink_proto::fnv1a64(t.as_bytes()),
        ClipPayload::Image { width, height, rgba, .. } => {
            let mut h = rdlink_proto::fnv1a64(&width.to_be_bytes());
            h = h.wrapping_mul(0x100000001b3);
            h = rdlink_proto::fnv1a64_with(h, &height.to_be_bytes());
            rdlink_proto::fnv1a64_with(h, rgba)
        }
    }
}

pub struct ClipboardHub {
    /// 本端剪贴板变化（poll 线程 → serve 控制循环），初始 None
    pub changes: tokio::sync::watch::Receiver<Option<(u64, ClipPayload)>>,
    /// 对端写入请求（serve 控制循环 → write 线程）
    pub write_tx: std::sync::mpsc::Sender<ClipPayload>,
}

/// 启动剪贴板双线程。剪贴板不可用时降级：打印警告，方向静默失效（不 panic）。
pub fn spawn() -> ClipboardHub {
    let (write_tx, write_rx) = std::sync::mpsc::channel::<ClipPayload>();
    let (changes_tx, changes_rx) = tokio::sync::watch::channel(None);

    // 写线程：对端剪贴板 → 本机（带持久重试，见内注释）
    std::thread::Builder::new()
        .name("rdlink-clip-write".into())
        .spawn(move || {
            while let Ok(payload) = write_rx.recv() {
                let hash = hash_payload(&payload);
                LAST_SYNCED.store(hash, Ordering::SeqCst);
                let (desc, len) = match &payload {
                    ClipPayload::Text(t) => (format!("文本 {} 字节", t.len()), None),
                    ClipPayload::Image { width, height, png, .. } => (
                        format!("图片 {width}x{height}（PNG {} 字节）", png.len()),
                        Some(png.len()),
                    ),
                };
                println!("[clip] 对端剪贴板写入 {desc}");
                // 剪贴板被其他程序（远控/输入法/微信类常驻）频繁占是常态，
                // 8×300ms 持久重试抢窗口期；全部失败则放弃（日志留痕）
                let mut ok = false;
                for attempt in 1..=8 {
                    let r = match &payload {
                        ClipPayload::Text(t) => write_clipboard_text(t),
                        ClipPayload::Image { width, height, rgba, .. } => {
                            write_clipboard_image(*width, *height, rgba)
                        }
                    };
                    match r {
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
                    let _ = len;
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
                let Some(payload) = read_clipboard_payload() else { continue };
                let hash = hash_payload(&payload);
                if hash != LAST_SYNCED.load(Ordering::SeqCst) {
                    LAST_SYNCED.store(hash, Ordering::SeqCst);
                    let desc = match &payload {
                        ClipPayload::Text(t) => format!("文本 {} 字节", t.len()),
                        ClipPayload::Image { width, height, png, .. } => {
                            format!("图片 {width}x{height}（PNG {} 字节）", png.len())
                        }
                    };
                    println!("[clip] 本端剪贴板变化 hash={hash:016x} {desc}");
                    let _ = changes_tx.send(Some((hash, payload)));
                }
            }
        })
        .expect("剪贴板轮询线程创建失败");

    ClipboardHub { changes: changes_rx, write_tx }
}

/// 读取本机剪贴板为负载：文本优先，无文本试图片（截图）
pub fn read_clipboard_payload() -> Option<ClipPayload> {
    if let Some(t) = read_clipboard_text() {
        if !t.is_empty() {
            if t.len() <= MAX_CLIP_TEXT {
                return Some(ClipPayload::Text(t));
            }
            return None; // 超长文本：跳过本轮
        }
    }
    // 无文本（或空）→ 尝试图片
    let (w, h, rgba) = read_clipboard_image()?;
    Some(ClipPayload::Image { width: w, height: h, rgba: rgba.clone(), png: encode_png(w, h, &rgba)? })
}

fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let img = image::RgbaImage::from_raw(w, h, rgba.to_vec())?;
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .ok()?;
    Some(out.into_inner())
}

pub fn decode_png(png: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some((w, h, img.into_raw()))
}

// ---------- 剪贴板读写原语 ----------

fn with_clipboard<T>(f: impl FnOnce() -> Option<T>) -> Option<T> {
    use windows::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};
    unsafe {
        // 剪贴板易争抢：打开失败重试（与写入路径同一策略）
        for _ in 0..10 {
            if OpenClipboard(None).is_ok() {
                let r = f();
                let _ = CloseClipboard();
                return r;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        None
    }
}

/// 读文本（CF_UNICODETEXT）
pub fn read_clipboard_text() -> Option<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::GetClipboardData;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;
    unsafe {
        with_clipboard(|| {
            let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            let hg = HGLOBAL(h.0);
            let ptr = GlobalLock(hg) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let wide = std::slice::from_raw_parts(ptr, len);
            let s = String::from_utf16_lossy(wide);
            let _ = GlobalUnlock(hg);
            Some(s)
        })
    }
}

/// 读图片（优先 CF_DIBV5，退 CF_DIB）→ (宽, 高, RGBA)
pub fn read_clipboard_image() -> Option<(u32, u32, Vec<u8>)> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::GetClipboardData;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5};
    unsafe {
        with_clipboard(|| {
            for fmt in [CF_DIBV5.0 as u32, CF_DIB.0 as u32] {
                let Ok(h) = GetClipboardData(fmt) else { continue };
                let hg = HGLOBAL(h.0);
                let ptr = GlobalLock(hg) as *const u8;
                if ptr.is_null() {
                    continue;
                }
                let size = GlobalSize(hg);
                let dib = std::slice::from_raw_parts(ptr, size);
                let out = dib_to_rgba(dib);
                let _ = GlobalUnlock(hg);
                if out.is_some() {
                    return out;
                }
            }
            None
        })
    }
}

/// BITMAPINFOHEADER/V5 → RGBA。支持 32bpp（BI_RGB/BI_BITFIELDS）与 24bpp。
/// 底-up（biHeight>0）自动翻转；alpha 全零视为不透明（截图工具常态）。
fn dib_to_rgba(dib: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if dib.len() < 40 {
        return None;
    }
    let rd_u32 = |o: usize| u32::from_le_bytes(dib[o..o + 4].try_into().unwrap());
    let rd_i32 = |o: usize| i32::from_le_bytes(dib[o..o + 4].try_into().unwrap());
    let rd_u16 = |o: usize| u16::from_le_bytes(dib[o..o + 2].try_into().unwrap());

    let header_size = rd_u32(0) as usize;
    let w = rd_i32(4);
    let height_raw = rd_i32(8);
    let bpp = rd_u16(14) as u32;
    let compression = rd_u32(16);
    let top_down = height_raw < 0;
    let h = height_raw.unsigned_abs();
    let w = w as u32;
    if w == 0 || h == 0 || w > 16384 || h > 16384 || header_size as usize > dib.len() {
        return None;
    }
    let (bytes_pp, supported) = match bpp {
        32 => (4usize, compression == 0 || compression == 3), // BI_RGB / BI_BITFIELDS
        24 => (3usize, compression == 0),
        _ => (0, false),
    };
    if !supported {
        return None;
    }
    let row = ((w as usize * bytes_pp) + 3) & !3;
    let px_len = row * h as usize;
    if dib.len() < header_size + px_len {
        return None;
    }
    let px = &dib[header_size..header_size + px_len];

    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let mut any_alpha = false;
    let get = |x: usize, y_src: usize| -> [u8; 4] {
        let o = y_src * row + x * bytes_pp;
        let (b, g, r) = (px[o], px[o + 1], px[o + 2]);
        let a = if bytes_pp == 4 { px[o + 3] } else { 255 };
        [r, g, b, a]
    };
    for y in 0..h as usize {
        let y_src = if top_down { y } else { h as usize - 1 - y };
        for x in 0..w as usize {
            let p = get(x, y_src);
            if p[3] != 0 {
                any_alpha = true;
            }
            let o = (y * w as usize + x) * 4;
            rgba[o..o + 4].copy_from_slice(&p);
        }
    }
    if !any_alpha {
        for p in rgba.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }
    Some((w, h, rgba))
}

/// 写图片：RGBA → 32bpp 顶-down CF_DIB（兼容性最广的格式）
pub fn write_clipboard_image(w: u32, h: u32, rgba: &[u8]) -> windows::core::Result<()> {
    unsafe {
        use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
        use windows::Win32::System::DataExchange::*;
        use windows::Win32::System::Memory::*;
        use windows::Win32::System::Ole::CF_DIB;

        let mut dib = Vec::with_capacity(40 + rgba.len());
        dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
        dib.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
        dib.extend_from_slice(&(h as i32).to_le_bytes()); // biHeight（正=底-up，我们直接给底-up 数据）
        dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
        dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
        dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
        dib.extend_from_slice(&((rgba.len()) as u32).to_le_bytes()); // biSizeImage
        dib.extend_from_slice(&0u32.to_le_bytes()); // biXPelsPerMeter
        dib.extend_from_slice(&0u32.to_le_bytes()); // biYPelsPerMeter
        dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
        dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
        // RGBA → BGRA（alpha 保留）
        for px in rgba.chunks_exact(4) {
            dib.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }

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
            let h = GlobalAlloc(GMEM_MOVEABLE, dib.len()).map_err(|e| format!("GlobalAlloc: {e}"))?;
            let ptr = GlobalLock(h) as *mut u8;
            if ptr.is_null() {
                let _ = GlobalFree(Some(h));
                return Err("GlobalLock: null".into());
            }
            std::ptr::copy_nonoverlapping(dib.as_ptr(), ptr, dib.len());
            let _ = GlobalUnlock(h);
            match SetClipboardData(CF_DIB.0 as u32, Some(HANDLE(h.0))) {
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

/// 写文本（CF_UNICODETEXT）
pub fn write_clipboard_text(text: &str) -> windows::core::Result<()> {
    unsafe {
        use windows::Win32::Foundation::{GlobalFree, HANDLE};
        use windows::Win32::System::DataExchange::*;
        use windows::Win32::System::Memory::*;
        use windows::Win32::System::Ole::CF_UNICODETEXT;
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
