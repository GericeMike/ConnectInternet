//! rdlink-transport：QUIC 传输层。
//!
//! - [`cert`]：rcgen 自签证书 + 指纹 pin（决策 D5）
//! - [`frame`]：quinn 流上的异步帧读写
//! - [`session`]：host 监听 / client 连接 + 三通道握手
//!   （Control 双向、Video host→client、Input client→host）
//!
//! M2 将在此层为视频通道增加 datagram 实现（`VideoSink`/`VideoSource` trait），
//! M1 视频走可靠流（决策 D1）。

pub mod cert;
pub mod frame;
pub mod session;

pub use frame::{read_frame, write_frame, FrameError};
pub use session::{connect, ClientSession, HostListener, HostSession, SessionError};
