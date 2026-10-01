//! QUIC stream 上的异步帧读写：把 [`rdlink_proto`] 的 length-prefix 帧接到 quinn 流上。

use quinn::{RecvStream, SendStream};
use rdlink_proto::{DecodeError, Message};

#[derive(Debug)]
pub enum FrameError {
    /// 连接断开（对端关闭或网络异常），不可恢复
    Closed,
    /// 网络层错误
    Io(String),
    /// 帧解码错误（超长/损坏），应断开
    Decode(DecodeError),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Closed => write!(f, "连接已断开"),
            FrameError::Io(e) => write!(f, "网络错误: {e}"),
            FrameError::Decode(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// 写一帧（length-prefix）。
pub async fn write_frame(send: &mut SendStream, msg: &Message) -> Result<(), FrameError> {
    let frame = rdlink_proto::encode(msg)
        .map_err(|e| FrameError::Io(format!("编码失败: {e:?}")))?;
    send.write_all(&frame).await.map_err(|e| FrameError::Io(e.to_string()))
}

/// 写任意可序列化类型的帧（M3-2 文件流的 [`rdlink_proto::FileMsg`] 复用同一封装）。
pub async fn write_frame_of<T: serde::Serialize>(
    send: &mut SendStream,
    msg: &T,
) -> Result<(), FrameError> {
    let frame = rdlink_proto::encode(msg)
        .map_err(|e| FrameError::Io(format!("编码失败: {e:?}")))?;
    send.write_all(&frame).await.map_err(|e| FrameError::Io(e.to_string()))
}

/// 读一帧。流被对端**在帧边界**正常关闭时返回 `Ok(None)`；中途关闭是错误。
pub async fn read_frame(recv: &mut RecvStream) -> Result<Option<Message>, FrameError> {
    read_frame_of(recv).await
}

/// 读任意可反序列化类型的帧（与 [`write_frame_of`] 配对）。
pub async fn read_frame_of<T: serde::de::DeserializeOwned>(
    recv: &mut RecvStream,
) -> Result<Option<T>, FrameError> {
    let mut head = [0u8; 4];
    match read_exact(recv, &mut head).await? {
        ReadOutcome::Filled => {}
        ReadOutcome::CleanEof => return Ok(None),
        ReadOutcome::BrokenEof => return Err(FrameError::Closed),
    }
    let len = u32::from_le_bytes(head) as usize;
    if len > rdlink_proto::MAX_FRAME_LEN {
        return Err(FrameError::Decode(DecodeError::TooLarge { len }));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        match read_exact(recv, &mut payload).await? {
            ReadOutcome::Filled => {}
            _ => return Err(FrameError::Closed),
        }
    }
    rdlink_proto::decode(&payload).map(Some).map_err(FrameError::Decode)
}

enum ReadOutcome {
    /// 读满
    Filled,
    /// 一开始就是 EOF（流干净关闭）
    CleanEof,
    /// 读了一半 EOF（帧被打断）
    BrokenEof,
}

async fn read_exact(recv: &mut RecvStream, buf: &mut [u8]) -> Result<ReadOutcome, FrameError> {
    let mut off = 0;
    while off < buf.len() {
        match recv.read(&mut buf[off..]).await.map_err(|e| FrameError::Io(e.to_string()))? {
            Some(n) if n > 0 => off += n,
            // quinn 约定：Ok(None) = 流结束（对端 finish 且数据取尽）
            None => {
                return Ok(if off == 0 {
                    ReadOutcome::CleanEof
                } else {
                    ReadOutcome::BrokenEof
                })
            }
            Some(_) => continue,
        }
    }
    Ok(ReadOutcome::Filled)
}
