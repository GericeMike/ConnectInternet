//! rdlink-transport：QUIC 传输层。
//!
//! 职责（M1/T2 交付）：
//! - host 侧监听 / client 侧连接（quinn + rustls）
//! - 三条固定 stream：Control(0, 双向)、Video(1, host→client)、Input(2, client→host)
//! - 证书：rcgen 运行时生成自签证书（决策 D5），指纹 pin 进配置
//! - 为 M2 的 datagram 迁移预留 trait 抽象：
//!
//! ```ignore
//! trait VideoSink   { async fn send_frame(&self, f: EncodedFrame); }
//! trait VideoSource { async fn next_frame(&self) -> Option<EncodedFrame>; }
//! ```
