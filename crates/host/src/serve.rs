//! T7：host 正常运行模式——监听 QUIC → 每会话开捕获线程（WGC+编码）→ Video 流发送。
//!
//! 结构：
//! ```text
//! 捕获线程(WGC msg loop) → encoder → unbounded channel → async 发送任务 → client
//! 主任务：握手后读 Control 通道（Ping/Pong、Bye），会话结束回收全部资源
//! ```

use std::path::Path;
use std::time::Instant;

use rdlink_proto::{ControlMsg, Message, VideoFrame};
use rdlink_transport::{read_frame, write_frame, HostListener, HostSession};
use tokio::sync::mpsc;

use crate::encoder::{self, SendEncoder};

/// 默认监听端口（M1 固定，M3 进配置）。
pub const LISTEN_ADDR: &str = "0.0.0.0:9527";

pub fn run() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime 创建失败");
    rt.block_on(async_main());
}

async fn async_main() {
    let listener = HostListener::listen(
        LISTEN_ADDR.parse().expect("监听地址格式"),
        Path::new("certs"),
    )
    .expect("监听失败（端口被占？删除 certs/ 可重新生成证书）");

    println!("rdlink-host 已就绪");
    println!("监听: {}", listener.local_addr().expect("local_addr"));
    println!("证书指纹（填给 client）: {}", listener.fingerprint);

    loop {
        // 每个会话重新取主显示器（分辨率可能在会话间变化）
        let monitor = windows_capture::monitor::Monitor::primary().expect("获取主显示器失败");
        let (w, h) = (monitor.width().expect("宽度"), monitor.height().expect("高度"));
        let info = ControlMsg::VideoStreamInfo { width: w, height: h, extradata: Vec::new() };

        let session = match listener.accept(info).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("握手失败: {e}");
                continue;
            }
        };
        println!("主控端已连接: {}（{w}x{h}）", session.peer_name);
        serve_session(session).await;
        println!("会话结束，等待下一个主控端…");
    }
}

/// 服务一个会话直到断开。
async fn serve_session(session: HostSession) {
    let HostSession {
        peer_name,
        mut control_send,
        mut control_recv,
        video,
        input,
    } = session;
    // 会话日志已在上层打印 peer_name；此处仅持有供未来按主控端区分策略用
    let _ = &peer_name;

    // 捕获线程 → channel → 发送任务
    let (tx, mut rx) = mpsc::unbounded_channel::<VideoFrame>();

    // free-threaded 启动：拿到 CaptureControl，会话结束后可从外部主动停止
    // （关键：静止桌面时 WGC 不产帧，捕获线程自己永远发现不了通道关闭）
    let capture_control = start_capture(tx.clone());

    // 发送任务：拥有 video 流；channel 关闭或写失败即结束
    let video_task = tokio::spawn(async move {
        let mut video = video;
        let mut sent = 0u64;
        while let Some(frame) = rx.recv().await {
            match write_frame(&mut video, &Message::VideoFrame(frame)).await {
                Ok(()) => sent += 1,
                Err(e) => {
                    eprintln!("video 流写入失败（主控端断开?）: {e}");
                    break;
                }
            }
        }
        sent
    });

    // 输入注入任务（T8）：Input 流 → SendInput。
    // SendInput 单次 <1ms,直接在异步任务里调用(M1);UIPI(焦点在提权窗口)时注入被系统丢弃并计数。
    let input_task = tokio::spawn(async move {
        let mut input = input;
        let mut injected = 0u64;
        let mut dropped = 0u64;
        while let Ok(Some(msg)) = read_frame(&mut input).await {
            if let Message::Input(event) = msg {
                match crate::input::inject(&event) {
                    Ok(()) => injected += 1,
                    Err(e) => {
                        dropped += 1;
                        if dropped <= 3 {
                            eprintln!("[input] 注入失败(UIPI?): {e}");
                        }
                    }
                }
                if injected > 0 && injected % 100 == 0 {
                    println!("[input] 已注入 {injected} 个事件");
                }
            }
        }
        (injected, dropped)
    });

    // Control 通道：Ping→Pong / Bye / 断开检测
    loop {
        match read_frame(&mut control_recv).await {
            Ok(Some(Message::Control(ControlMsg::Ping { t_us }))) => {
                let _ = write_frame(&mut control_send, &Message::Control(ControlMsg::Pong { t_us })).await;
            }
            Ok(Some(Message::Control(ControlMsg::Bye { reason }))) => {
                println!("主控端主动断开: {reason}");
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                println!("control 通道关闭");
                break;
            }
            Err(e) => {
                println!("control 通道错误: {e}");
                break;
            }
        }
    }

    // 回收：drop 剩余 tx → 主动停捕获（WM_QUIT；静止桌面时线程不会自己发现通道关闭）
    //      → channel 排空 → 发送/注入任务结束
    drop(tx);
    drop(control_send);
    drop(control_recv);
    let _ = tokio::task::spawn_blocking(move || capture_control.stop()).await;
    if let Ok(sent) = tokio::time::timeout(std::time::Duration::from_secs(10), video_task).await {
        println!("共发送 {} 帧视频", sent.unwrap_or(0));
    }
    if let Ok(Ok((injected, dropped))) =
        tokio::time::timeout(std::time::Duration::from_secs(5), input_task).await
    {
        if injected > 0 || dropped > 0 {
            println!("共注入 {injected} 个输入事件（{dropped} 个被系统拒绝）");
        }
    }
}

// ---------------------------------------------------------------------------
// 捕获线程（阻塞式 WGC 消息循环）
// ---------------------------------------------------------------------------

use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

struct ServeCapture {
    enc: SendEncoder,
    tx: mpsc::UnboundedSender<VideoFrame>,
    start: Instant,
    frames: u64,
    encoded: u64,
    last_report: Instant,
    scratch: Vec<u8>,
    /// 编码器名称（日志）
    enc_name: &'static str,
}

impl GraphicsCaptureApiHandler for ServeCapture {
    /// Flags 即捕获线程的出口通道（经 Settings 注入）
    type Flags = mpsc::UnboundedSender<VideoFrame>;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let tx = ctx.flags;
        let monitor = windows_capture::monitor::Monitor::primary()?;
        let (w, h) = (monitor.width()?, monitor.height()?);
        let (enc, tier) = encoder::open_auto(w, h)?;
        let enc_name = enc.name();
        let tier_label = match tier {
            encoder::EncoderTier::Nvenc => "NVENC 硬编",
            encoder::EncoderTier::Qsv => "QSV 核显硬编",
            encoder::EncoderTier::X264 => "x264 软编兜底",
        };
        println!("[capture] 编码器: {enc_name}（{tier_label}）@ {w}x{h}");
        Ok(Self {
            enc: SendEncoder(enc),
            tx,
            start: Instant::now(),
            frames: 0,
            encoded: 0,
            last_report: Instant::now(),
            scratch: Vec::new(),
            enc_name,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        ctrl: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let pts_us = self.start.elapsed().as_micros() as i64;
        let w = frame.width() as usize;

        {
            let fb = frame.buffer()?;
            let bgra = fb.as_nopadding_buffer(&mut self.scratch);
            let packets = self.enc.0.encode(bgra, w * 4, pts_us)?;
            for p in packets {
                self.encoded += 1;
                if self
                    .tx
                    .send(VideoFrame {
                        capture_pts_us: p.pts_us,
                        key: p.key,
                        data: p.data,
                    })
                    .is_err()
                {
                    // 发送端已关闭（会话结束）→ 停止捕获
                    ctrl.stop();
                    return Ok(());
                }
            }
        }

        self.frames += 1;
        if self.last_report.elapsed().as_secs_f32() >= 5.0 {
            let secs = self.start.elapsed().as_secs_f32();
            println!(
                "[capture] {} {:.0}fps | 编码 {} 包 ({})",
                self.enc_name,
                self.frames as f32 / secs,
                self.encoded,
                secs as u64
            );
            self.last_report = Instant::now();
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("[capture] 捕获会话关闭");
        Ok(())
    }
}

/// 启动捕获（自由线程）：返回外部控制句柄，会话结束用 `stop()` 主动回收。
fn start_capture(
    tx: mpsc::UnboundedSender<VideoFrame>,
) -> windows_capture::capture::CaptureControl<ServeCapture, Box<dyn std::error::Error + Send + Sync>> {
    let monitor = windows_capture::monitor::Monitor::primary().expect("主显示器");
    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        tx,
    );
    ServeCapture::start_free_threaded(settings).expect("捕获启动失败")
}
