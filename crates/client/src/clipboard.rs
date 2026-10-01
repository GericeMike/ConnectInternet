//! M3-1/M3-6：剪贴板同步（client 侧）——文本 + 图片（截图）。
//!
//! 与 host 侧同一套防回环模型：单一同步指纹 LAST_SYNCED，无论"本端轮询到变化"
//! 还是"收到对端写入"，先比对一致即忽略。图片指纹域 = 宽+高+RGBA 像素
//! （PNG 编解码无损，两端像素一致 → 指纹一致）。
//!
//! - poll 线程：arboard 每 500ms 轮询（文本优先；无文本试图片）→ 变化经
//!   watch 通道给控制循环；
//! - write 线程：std mpsc 收对端同步 → arboard 写入。

use rdlink_proto::{ControlMsg, Message};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

static LAST_SYNCED: AtomicU64 = AtomicU64::new(0);

/// 文本上限（与 host 一致）
pub const MAX_CLIP_TEXT: usize = 1024 * 1024;
/// 图片 PNG 上限（与 host 一致）
pub const MAX_CLIP_PNG: usize = 8 * 1024 * 1024;

/// 剪贴板负载
#[derive(Clone)]
pub enum ClipPayload {
    Text(String),
    Image { width: u32, height: u32, rgba: Vec<u8> },
}

fn hash_payload(p: &ClipPayload) -> u64 {
    match p {
        ClipPayload::Text(t) => rdlink_proto::fnv1a64(t.as_bytes()),
        ClipPayload::Image { width, height, rgba } => {
            let mut h = rdlink_proto::fnv1a64(&width.to_be_bytes());
            h = h.wrapping_mul(0x100000001b3);
            h = rdlink_proto::fnv1a64_with(h, &height.to_be_bytes());
            rdlink_proto::fnv1a64_with(h, rgba)
        }
    }
}

/// 剪贴板负载 → 线上消息（文本/图片）
pub fn payload_to_msg(hash: u64, payload: &ClipPayload) -> Message {
    match payload {
        ClipPayload::Text(t) => Message::Control(ControlMsg::ClipboardSync { hash, text: t.clone() }),
        ClipPayload::Image { width, height, rgba } => {
            // RGBA → PNG（无损，指纹域不变）
            let img = image::RgbaImage::from_raw(*width, *height, rgba.clone())
                .expect("rgba 长度与尺寸匹配");
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut png, image::ImageFormat::Png)
                .expect("PNG 编码不应失败");
            Message::Control(ControlMsg::ClipboardImage {
                hash,
                width: *width,
                height: *height,
                png: png.into_inner(),
            })
        }
    }
}

pub struct ClipboardBridge {
    /// 本端剪贴板变化（poll 线程 → 控制循环），初始 None
    pub changes: tokio::sync::watch::Receiver<Option<(u64, ClipPayload)>>,
    /// 对端写入请求（控制循环 → write 线程）
    pub write_tx: std::sync::mpsc::Sender<ClipPayload>,
}

pub fn spawn() -> ClipboardBridge {
    let (write_tx, write_rx) = std::sync::mpsc::channel::<ClipPayload>();
    let (changes_tx, changes_rx) = tokio::sync::watch::channel(None);

    // 写线程：对端剪贴板 → 本机
    std::thread::Builder::new()
        .name("rdlink-clip-write".into())
        .spawn(move || {
            let Ok(mut board) = arboard::Clipboard::new() else {
                eprintln!("[clip] 剪贴板不可用（写方向关闭）");
                return;
            };
            while let Ok(payload) = write_rx.recv() {
                let hash = hash_payload(&payload);
                LAST_SYNCED.store(hash, Ordering::SeqCst);
                match payload {
                    ClipPayload::Text(text) => {
                        println!("[clip] 对端剪贴板写入 文本 {} 字节", text.len());
                        if let Err(e) = board.set_text(text) {
                            eprintln!("[clip] 写入文本失败: {e}");
                        }
                    }
                    ClipPayload::Image { width, height, rgba } => {
                        println!("[clip] 对端剪贴板写入 图片 {width}x{height}");
                        let data = arboard::ImageData {
                            width: width as usize,
                            height: height as usize,
                            bytes: std::borrow::Cow::Owned(rgba),
                        };
                        if let Err(e) = board.set_image(data) {
                            eprintln!("[clip] 写入图片失败: {e}");
                        }
                    }
                }
            }
        })
        .expect("剪贴板写线程创建失败");

    // 轮询线程：本机剪贴板 → 对端（文本优先，无文本试图片）
    std::thread::Builder::new()
        .name("rdlink-clip-poll".into())
        .spawn(move || {
            let Ok(mut board) = arboard::Clipboard::new() else {
                eprintln!("[clip] 剪贴板不可用（读方向关闭）");
                return;
            };
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let payload = read_payload(&mut board);
                let Some(payload) = payload else { continue };
                let hash = hash_payload(&payload);
                if hash != LAST_SYNCED.load(Ordering::SeqCst) {
                    LAST_SYNCED.store(hash, Ordering::SeqCst);
                    let desc = match &payload {
                        ClipPayload::Text(t) => format!("文本 {} 字节", t.len()),
                        ClipPayload::Image { width, height, .. } => {
                            format!("图片 {width}x{height}")
                        }
                    };
                    println!("[clip] 本端剪贴板变化 hash={hash:016x} {desc}");
                    let _ = changes_tx.send(Some((hash, payload)));
                }
            }
        })
        .expect("剪贴板轮询线程创建失败");

    ClipboardBridge { changes: changes_rx, write_tx }
}

/// 读本地剪贴板为负载：文本优先（非空即文本），无文本试图片（截图）
fn read_payload(board: &mut arboard::Clipboard) -> Option<ClipPayload> {
    match board.get_text() {
        Ok(text) if !text.is_empty() => {
            if text.len() <= MAX_CLIP_TEXT {
                return Some(ClipPayload::Text(text));
            }
            return None; // 超长文本跳过
        }
        _ => {}
    }
    // 无文本 → 试图片
    let Ok(img) = board.get_image() else { return None };
    let (w, h) = (img.width as u32, img.height as u32);
    if w == 0 || h == 0 || w > 16384 || h > 16384 {
        return None;
    }
    Some(ClipPayload::Image {
        width: w,
        height: h,
        rgba: img.bytes.into_owned(),
    })
}

/// 会话建立时调用：把 LAST_SYNCED 预置为本地当前剪贴板的指纹。
/// 存量同步方向是 host→client（host 推它的剪贴板给新 client），
/// client 不再首轮推送本地存量，避免两端初始互推打架。
pub fn prime_with_local() {
    let Ok(mut board) = arboard::Clipboard::new() else { return };
    // 文本优先
    if let Ok(text) = board.get_text() {
        if !text.is_empty() {
            LAST_SYNCED.store(rdlink_proto::fnv1a64(text.as_bytes()), Ordering::SeqCst);
            return;
        }
    }
    // 图片
    if let Ok(img) = board.get_image() {
        let (w, h) = (img.width as u32, img.height as u32);
        if w > 0 && h > 0 {
            let mut hsh = rdlink_proto::fnv1a64(&w.to_be_bytes());
            hsh = hsh.wrapping_mul(0x100000001b3);
            hsh = rdlink_proto::fnv1a64_with(hsh, &h.to_be_bytes());
            hsh = rdlink_proto::fnv1a64_with(hsh, &img.bytes);
            LAST_SYNCED.store(hsh, Ordering::SeqCst);
        }
    }
}
