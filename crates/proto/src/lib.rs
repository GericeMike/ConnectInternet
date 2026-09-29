//! rdlink-proto：rdlink 的线上协议消息定义。
//!
//! 所有跨进程消息（控制信令、视频帧、输入事件）都在这里定义，
//! host 与 client 共同依赖本 crate，保证两端编解码一致。
//!
//! M1 交付物（T1）：
//! - [`Message`] 枚举（Control / VideoFrame / Input 三类）
//! - length-prefix 帧封装工具（`[u32 len][bincode bytes]`）
//! - roundtrip 单元测试

/// 协议版本号。两端 Hello 阶段校验，不一致则拒绝连接。
pub const PROTOCOL_VERSION: u32 = 1;
