//! rdlink-proto：rdlink 的线上协议消息定义。
//!
//! 所有跨进程消息（控制信令、视频帧、输入事件）都在这里定义，
//! host 与 client 共同依赖本 crate，保证两端编解码一致。
//!
//! 帧封装：`[u32 LE 长度][bincode(Message)]`，适用于 QUIC stream 逐段读取。

use serde::{Deserialize, Serialize};

/// 协议版本号。两端 Hello 阶段校验，不一致则拒绝连接。
/// v2: VideoFrame 加 encode_us；Pong 加 host_recv_us/host_send_us（T9 打点）
/// v3: ControlMsg 加 ClipboardSync（M3-1 剪贴板同步）
/// v4: FileMsg::Request 加 to_view（M3-2 拖拽落前台 Explorer 文件夹/桌面）
/// v5: ControlMsg 加 ClipboardImage（M3-6 截图/图片剪贴板同步，PNG 编码传输）
pub const PROTOCOL_VERSION: u32 = 5;

/// 单帧最大长度（16 MiB）：1080p60 高码率下一帧远小于此值，超限视为对端异常。
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// 所有线上消息的顶层封装。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// 控制信令（Control stream，双向）
    Control(ControlMsg),
    /// 编码后的视频帧（Video stream，host→client）
    VideoFrame(VideoFrame),
    /// 输入事件（Input stream，client→host）
    Input(InputEvent),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMsg {
    /// client → host：发起连接
    Hello { proto_version: u32, client_name: String },
    /// host → client：接受连接（Control 通道）
    HelloAck {
        proto_version: u32,
        host_name: String,
    },
    /// host → client：**Video uni 流首帧**。
    /// QUIC 语义：uni 流必须写入首帧数据对端才可见（accept_uni 才能返回），
    /// 因此建流与元信息下发绑定为原子操作。
    VideoStreamInfo {
        width: u32,
        height: u32,
        /// SPS/PPS 等解码器初始化数据（T4 接入真实值）
        extradata: Vec<u8>,
    },
    /// client → host：**Input uni 流首帧**，作用同上（流激活）。
    InputStreamReady,
    /// 双向：测量 RTT + 估算两机时钟偏移（client 发）
    /// t_us = 发送时刻（client 时钟，epoch 微秒）
    Ping { t_us: i64 },
    /// host 回：携带 host 收/发时刻（host 时钟，epoch 微秒）。
    /// client 用四时间戳法算 offset = ((hr+hs)/2) - ((t_send+t_recv)/2)
    Pong { t_us: i64, host_recv_us: i64, host_send_us: i64 },
    /// 优雅断开
    Bye { reason: String },
    /// 双向（M3-1）：本机剪贴板文本变化。
    /// hash = 内容 FNV-1a 64 位指纹，接收端与 last_synced 比对一致则忽略
    /// （抑制"我写入→本端监听到变化→再同步回去"的回环）。
    /// 文本上限 1 MiB，超限发送方直接丢弃。
    ClipboardSync { hash: u64, text: String },
    /// 双向（M3-6）：本机剪贴板**图片**变化（截图等）。
    /// png 为 RGBA 转 PNG 的编码结果（无损回环比对：两端都以 RGBA 像素做指纹）。
    /// 上限 8 MiB，超限发送方直接丢弃。
    ClipboardImage { hash: u64, width: u32, height: u32, png: Vec<u8> },
}

/// FNV-1a 64 位内容指纹（M3-1 剪贴板回环抑制用）
pub fn fnv1a64(data: &[u8]) -> u64 {
    fnv1a64_with(0xcbf29ce484222325, data)
}

/// 带种子续算版本（图片指纹 = 宽、高、像素三段续算）
pub fn fnv1a64_with(seed: u64, data: &[u8]) -> u64 {
    let mut h = seed;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// M3-2：文件传输块大小（256 KiB）
pub const FILE_CHUNK: usize = 256 * 1024;

/// M3-2：文件传输流消息（独立 bi stream，**一操作一流**，流隔离天然免掉消息 id）。
/// 方向约定：Up=client 上传（数据 client→host，落 host Downloads）；
///          Down=client 下载（数据 host→client，源在 host Downloads）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FileMsg {
    /// client → host：列下载目录（M3 固定 host Downloads，path 扩展预留）
    ListReq { path: String },
    /// host → client：目录列表
    ListReply { entries: Vec<FileEntry> },
    /// client → host：发起传输。
    /// to_view=true（拖拽上传）：落到被控端**前台 Explorer 正在显示的文件夹**，
    ///   前台是桌面则落桌面，都不是则落 Downloads 兜底；
    /// to_view=false（脚本/运维上传）：固定落 Downloads。
    Request { dir: XferDir, name: String, size: u64, to_view: bool },
    /// host → client：接受
    Accept,
    /// host → client：拒绝（路径非法/大小不符/IO）
    Reject { reason: String },
    /// 数据块（Up: client→host；Down: host→client）
    Chunk { offset: u64, data: Vec<u8> },
    /// 发送方收尾：全部内容 SHA-256（hex）
    Done { sha256: String },
    /// 接收方校验通过
    Ok,
    /// 接收方校验失败/IO 错误
    Fail { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub mtime: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum XferDir {
    Up,
    Down,
}

/// 一帧编码后的 H.264 数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoFrame {
    /// 采集时刻（host epoch 单调时钟，微秒），用于延迟统计
    pub capture_pts_us: i64,
    /// 是否关键帧（IDR）。client 在收到首个 key=true 前不送解码器。
    pub key: bool,
    /// host 侧编码耗时（采集回调内，微秒）——分段打点用
    pub encode_us: u32,
    pub data: Vec<u8>,
}

/// 键鼠输入事件。坐标为被控端屏幕像素（client 侧已完成换算）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMove { x: u32, y: u32 },
    MouseButton { button: MouseButton, down: bool },
    MouseWheel { dx: i32, dy: i32 },
    /// Windows 虚拟键码
    Key { vk: u16, down: bool },
    /// 文字输入兜底（KEYEVENTF_UNICODE）
    UnicodeChar { ch: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

// ---------------------------------------------------------------------------
// 帧编解码
// ---------------------------------------------------------------------------

/// 编码错误。
#[derive(Debug)]
pub enum EncodeError {
    /// 序列化后超过 [`MAX_FRAME_LEN`]
    TooLarge { len: usize },
}

/// 解码错误。
#[derive(Debug)]
pub enum DecodeError {
    /// 单帧超过 [`MAX_FRAME_LEN`]，对端异常，应断开
    TooLarge { len: usize },
    /// bincode 反序列化失败（数据损坏或协议版本不匹配）
    Malformed(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::TooLarge { len } => write!(f, "帧超长: {len} 字节"),
            DecodeError::Malformed(e) => write!(f, "帧损坏: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// 解码一条带长度前缀的字节帧。
pub fn encode<T: serde::Serialize>(msg: &T) -> Result<Vec<u8>, EncodeError> {
    let payload = bincode::serde::encode_to_vec(msg, bincode::config::standard())
        .expect("bincode 编码不应失败（无 IO）");
    if payload.len() > MAX_FRAME_LEN {
        return Err(EncodeError::TooLarge { len: payload.len() });
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// 反序列化帧载荷（不含长度前缀）。供传输层使用。
pub fn decode<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Result<T, DecodeError> {
    bincode::serde::decode_from_slice(payload, bincode::config::standard())
        .map(|(msg, _)| msg)
        .map_err(|e| DecodeError::Malformed(e.to_string()))
}

/// 流式帧解码器：喂入任意切割的字节流，逐条弹出完整消息。
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一段原始字节（切割方式任意）。
    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// 弹出下一条完整消息；数据不足时返回 `Ok(None)`。
    pub fn next_message(&mut self) -> Result<Option<Message>, DecodeError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
            as usize;
        if len > MAX_FRAME_LEN {
            return Err(DecodeError::TooLarge { len });
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let payload = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        bincode::serde::decode_from_slice(&payload, bincode::config::standard())
            .map(|(msg, _)| Some(msg))
            .map_err(|e| DecodeError::Malformed(e.to_string()))
    }

    /// 缓冲区内已积压的字节数（背压监测用）。
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Message) {
        let bytes = encode(&msg).unwrap();
        let mut dec = FrameDecoder::new();
        dec.feed(&bytes);
        let got = dec.next_message().unwrap().unwrap();
        assert_eq!(got, msg);
        assert!(dec.next_message().unwrap().is_none());
    }

    #[test]
    fn roundtrip_all_kinds() {
        roundtrip(Message::Control(ControlMsg::Hello {
            proto_version: PROTOCOL_VERSION,
            client_name: "4060-laptop".into(),
        }));
        roundtrip(Message::Control(ControlMsg::HelloAck {
            proto_version: PROTOCOL_VERSION,
            host_name: "mx250".into(),
        }));
        roundtrip(Message::Control(ControlMsg::VideoStreamInfo {
            width: 1920,
            height: 1080,
            extradata: vec![0x67, 0x64, 0x00, 0x1f],
        }));
        roundtrip(Message::Control(ControlMsg::InputStreamReady));
        roundtrip(Message::Control(ControlMsg::Ping { t_us: 12345 }));
        roundtrip(Message::Control(ControlMsg::Pong {
            t_us: 12345,
            host_recv_us: 67890,
            host_send_us: 67899,
        }));
        roundtrip(Message::Control(ControlMsg::Bye { reason: "测试".into() }));
        roundtrip(Message::VideoFrame(VideoFrame {
            capture_pts_us: 999_999,
            key: true,
            encode_us: 5210,
            data: vec![1, 2, 3, 4, 5],
        }));
        roundtrip(Message::Input(InputEvent::MouseMove { x: 1920, y: 0 }));
        roundtrip(Message::Input(InputEvent::MouseButton {
            button: MouseButton::X1,
            down: true,
        }));
        roundtrip(Message::Input(InputEvent::MouseWheel { dx: 0, dy: -120 }));
        roundtrip(Message::Input(InputEvent::Key { vk: 0x25, down: false }));
        roundtrip(Message::Input(InputEvent::UnicodeChar { ch: 0x4e2d }));
    }

    /// 大帧（模拟视频关键帧）+ 逐字节切割喂入
    #[test]
    fn byte_by_byte_feed_large_frame() {
        let msg = Message::VideoFrame(VideoFrame {
            capture_pts_us: 42,
            key: true,
            encode_us: 4800,
            data: vec![0xAB; 300_000],
        });
        let bytes = encode(&msg).unwrap();
        let mut dec = FrameDecoder::new();
        for b in &bytes {
            dec.feed(std::slice::from_ref(b));
            // 未喂完前不得弹出消息
        }
        assert_eq!(dec.next_message().unwrap().unwrap(), msg);
    }

    /// 多帧粘包
    #[test]
    fn multiple_frames_in_one_chunk() {
        let frames: Vec<Message> = vec![
            Message::Control(ControlMsg::Ping { t_us: 1 }),
            Message::Input(InputEvent::MouseMove { x: 5, y: 6 }),
            Message::VideoFrame(VideoFrame {
                capture_pts_us: 7,
                key: false,
                encode_us: 4100,
                data: vec![9; 4096],
            }),
        ];
        let mut stream = Vec::new();
        for f in &frames {
            stream.extend_from_slice(&encode(f).unwrap());
        }
        let mut dec = FrameDecoder::new();
        dec.feed(&stream);
        for expect in frames {
            assert_eq!(dec.next_message().unwrap().unwrap(), expect);
        }
        assert!(dec.next_message().unwrap().is_none());
        assert_eq!(dec.buffered(), 0);
    }

    #[test]
    fn malformed_payload_rejected() {
        let mut stream = vec![8u8, 0, 0, 0]; // len=8
        stream.extend_from_slice(&[0xFF; 8]); // 垃圾载荷
        let mut dec = FrameDecoder::new();
        dec.feed(&stream);
        assert!(dec.next_message().is_err());
    }
}
