//! 会话层：host 监听 / client 连接，握手后提供 Control(双向)、Video(host→client)、
//! Input(client→host) 三条帧通道。
//!
//! 握手时序（顺序执行，无并发等待，天然无死锁）：
//! ```text
//! client: connect → open_bi(Control) → 发 Hello ─┐
//! host:   accept  → accept_bi(Control) → 读 Hello │
//!         → 校验协议版本 → open_uni(Video)        │
//!         → 回 HelloAck ──────────────────────────┤
//! client: 读 HelloAck → open_uni(Input)           ▼
//! host:   accept_uni(Input)
//! ```

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rdlink_proto::{ControlMsg, Message, PROTOCOL_VERSION};

use crate::cert;
use crate::frame::{read_frame, write_frame, FrameError};

/// 握手整体超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum SessionError {
    Io(String),
    /// 握手失败（版本不匹配/证书 pin 不对/对端拒绝）
    Handshake(String),
    Frame(FrameError),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Io(e) => write!(f, "IO 错误: {e}"),
            SessionError::Handshake(e) => write!(f, "握手失败: {e}"),
            SessionError::Frame(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<FrameError> for SessionError {
    fn from(e: FrameError) -> Self {
        SessionError::Frame(e)
    }
}

// ---------------------------------------------------------------------------
// host 侧
// ---------------------------------------------------------------------------

pub struct HostListener {
    endpoint: Endpoint,
    /// 本机证书指纹（告知主控端用）
    pub fingerprint: String,
}

impl HostListener {
    /// 监听 `addr`（如 `0.0.0.0:9527`），证书从 `cert_dir` 加载或首启生成。
    pub fn listen(addr: std::net::SocketAddr, cert_dir: &Path) -> Result<Self, SessionError> {
        let (cert, key, fingerprint) =
            cert::ensure_host_cert(cert_dir).map_err(|e| SessionError::Io(e.to_string()))?;

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_crypto = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("rustls 协议版本")
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .map_err(|e| SessionError::Io(e.to_string()))?;
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
                .map_err(|e| SessionError::Io(e.to_string()))?,
        ));

        let endpoint = Endpoint::server(server_config, addr)
            .map_err(|e| SessionError::Io(e.to_string()))?;
        Ok(Self { endpoint, fingerprint })
    }

    pub fn local_addr(&self) -> Result<std::net::SocketAddr, SessionError> {
        self.endpoint.local_addr().map_err(|e| SessionError::Io(e.to_string()))
    }

    /// 等待并接受一个 client，完成三通道握手。
    /// `video_info` 携带真实捕获分辨率/解码参数，随握手下发（T7）。
    pub async fn accept(
        &self,
        video_info: ControlMsg,
    ) -> Result<HostSession, SessionError> {
        // 服务器无限期等待新连接；超时只约束握手各步骤
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| SessionError::Handshake("监听端已关闭".into()))?;
        let conn = incoming
            .await
            .map_err(|e| SessionError::Io(e.to_string()))?;
        self.handshake(conn, video_info).await
    }

    async fn handshake(
        &self,
        conn: Connection,
        video_info: ControlMsg,
    ) -> Result<HostSession, SessionError> {
        let (mut control_send, mut control_recv) = timeout("host-accept-bi", conn.accept_bi()).await?;

        let hello = match read_frame(&mut control_recv).await? {
            Some(Message::Control(ControlMsg::Hello { proto_version, client_name })) => {
                if proto_version != PROTOCOL_VERSION {
                    let _ = write_frame(
                        &mut control_send,
                        &Message::Control(ControlMsg::Bye {
                            reason: format!(
                                "协议版本不匹配: host={PROTOCOL_VERSION} client={proto_version}"
                            ),
                        }),
                    )
                    .await;
                    return Err(SessionError::Handshake("client 协议版本不匹配".into()));
                }
                (proto_version, client_name)
            }
            _ => return Err(SessionError::Handshake("期望 Hello".into())),
        };

        // QUIC 语义：uni 流必须写入首帧对端才可见。
        // 顺序：开 Video 流并立刻写 VideoStreamInfo（首帧激活，携带真实参数）
        //      → 回 HelloAck → 收 Input 流（其首帧为 InputStreamReady）
        let mut video = timeout("open-uni", conn.open_uni()).await?;
        write_frame(&mut video, &Message::Control(video_info)).await?;
        write_frame(
            &mut control_send,
            &Message::Control(ControlMsg::HelloAck {
                proto_version: PROTOCOL_VERSION,
                host_name: whoami(),
            }),
        )
        .await?;
        let mut input = timeout("accept-uni", conn.accept_uni()).await?;
        match read_frame(&mut input).await? {
            Some(Message::Control(ControlMsg::InputStreamReady)) => {}
            other => return Err(SessionError::Handshake(format!("期望 InputStreamReady，得到 {other:?}"))),
        }

        Ok(HostSession {
            peer_name: hello.1,
            control_send,
            control_recv,
            video,
            input,
        })
    }
}

/// host 侧会话：持有三条流。
pub struct HostSession {
    /// 主控端自报名称
    pub peer_name: String,
    pub control_send: SendStream,
    pub control_recv: RecvStream,
    pub video: SendStream,
    pub input: RecvStream,
}

// ---------------------------------------------------------------------------
// client 侧
// ---------------------------------------------------------------------------

pub struct ClientSession {
    pub peer_name: String,
    /// host 随握手下发的视频参数（分辨率/extradata）
    pub video_info: ControlMsg,
    pub control_send: SendStream,
    pub control_recv: RecvStream,
    pub video: RecvStream,
    pub input: SendStream,
}

/// 连接并完成握手。`pin` 是 host 证书的 SHA-256 指纹（hex）。
pub async fn connect(
    addr: std::net::SocketAddr,
    pin: &str,
    client_name: &str,
) -> Result<ClientSession, SessionError> {
    let quic_client_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(
        cert::client_crypto(pin).map_err(|e| SessionError::Handshake(e.to_string()))?,
    )
    .map_err(|e| SessionError::Handshake(e.to_string()))?;
    let client_config = quinn::ClientConfig::new(Arc::new(quic_client_crypto));

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())
        .map_err(|e| SessionError::Io(e.to_string()))?;
    endpoint.set_default_client_config(client_config);

    let conn = timeout(
        "quic-connect",
        endpoint.connect(addr, "rdlink-host").map_err(|e| SessionError::Io(e.to_string()))?,
    )
    .await?;

    // 1. Control 双向流 + Hello
        let (mut control_send, mut control_recv) = timeout("client-open-bi", conn.open_bi()).await?;
    write_frame(
        &mut control_send,
        &Message::Control(ControlMsg::Hello {
            proto_version: PROTOCOL_VERSION,
            client_name: client_name.into(),
        }),
    )
    .await?;

    // 2. 等 HelloAck（host 在 ack 前已 open Video uni）
    let ack = match read_frame(&mut control_recv).await? {
        Some(Message::Control(ControlMsg::HelloAck { proto_version, host_name, .. })) => {
            if proto_version != PROTOCOL_VERSION {
                return Err(SessionError::Handshake("host 协议版本不匹配".into()));
            }
            host_name
        }
        Some(Message::Control(ControlMsg::Bye { reason })) => {
            return Err(SessionError::Handshake(format!("被 host 拒绝: {reason}")));
        }
        other => return Err(SessionError::Handshake(format!("期望 HelloAck，得到 {other:?}"))),
    };

    // 3. Video 流（首帧 VideoStreamInfo 已随建流写入）→ Input 流（open 后立刻写首帧激活）
    let mut video = timeout("accept-uni", conn.accept_uni()).await?;
    let video_info = match read_frame(&mut video).await? {
        Some(Message::Control(info @ ControlMsg::VideoStreamInfo { .. })) => info,
        other => return Err(SessionError::Handshake(format!("期望 VideoStreamInfo，得到 {other:?}"))),
    };
    let mut input = timeout("open-uni", conn.open_uni()).await?;
    write_frame(&mut input, &Message::Control(ControlMsg::InputStreamReady)).await?;

    Ok(ClientSession {
        peer_name: ack,
        video_info,
        control_send,
        control_recv,
        video,
        input,
    })
}

// ---------------------------------------------------------------------------

async fn timeout<T, E>(
    step: &str,
    fut: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, SessionError>
where
    E: std::fmt::Display,
{
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(SessionError::Io(e.to_string())),
        Err(_) => Err(SessionError::Handshake(format!("握手步骤超时: {step}"))),
    }
}

fn whoami() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".into())
}
