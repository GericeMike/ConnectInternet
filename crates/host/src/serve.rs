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

/// host 配置（rdlink.toml [host] 节，文件不存在用默认值：端口 9527、证书目录 certs/）
#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct HostConf {
    pub port: Option<u16>,
    pub cert_dir: Option<String>,
}

pub fn load_conf() -> HostConf {
    std::fs::read_to_string("rdlink.toml")
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
        .and_then(|v| v.get("host").cloned())
        .and_then(|h| h.try_into::<HostConf>().ok())
        .unwrap_or_default()
}

/// host 进程级时钟原点：VideoFrame.pts 与 Pong 时戳共用同一时钟域（对时前提）
static HOST_EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// host epoch 起的微秒数
fn epoch_us() -> i64 {
    HOST_EPOCH
        .get()
        .map(|t| t.elapsed().as_micros() as i64)
        .unwrap_or(0)
}

pub fn run() {
    let _ = HOST_EPOCH.set(Instant::now());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime 创建失败");
    rt.block_on(async_main());
}

async fn async_main() {
    let conf = load_conf();
    let port = conf.port.unwrap_or(9527);
    let cert_dir = conf.cert_dir.unwrap_or_else(|| "certs".into());
    let addr: std::net::SocketAddr = format!("0.0.0.0:{port}").parse().expect("监听地址格式");
    let listener = HostListener::listen(addr, Path::new(&cert_dir))
        .expect("监听失败（端口被占？删除 certs/ 可重新生成证书）");

    println!("rdlink-host 已就绪");
    println!("监听: {}", listener.local_addr().expect("local_addr"));
    println!("证书指纹（填给 client）: {}", listener.fingerprint);

    // 会话串行闸：accept 循环与会话生命周期解耦（前一会话回收期间新连接的握手
    // 不再被阻塞超时），但同时只允许一个会话占用捕获（后来者握完手等待）
    let gate = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    // 抢占信号：新主控端接入时通知当前会话提前回收（不等 idle timeout 的 10s）
    let (preempt_tx, preempt_rx) = tokio::sync::watch::channel(false);

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
        // 通知旧会话让位
        let _ = preempt_tx.send(true);
        let gate = gate.clone();
        let preempt_tx = preempt_tx.clone();
        let preempt = preempt_rx.clone();
        tokio::spawn(async move {
            let _permit = gate.lock().await; // 前一会话占用捕获时，本会话在此等待
            let _ = preempt_tx.send(false); // 自己上岗后复位信号（供再下一个会话抢占）
            serve_session_inner(session, preempt).await;
            println!("会话结束，等待下一个主控端…");
        });
    }
}

/// 服务一个会话直到断开或被新主控端抢占。
async fn serve_session_inner(
    session: HostSession,
    mut preempt: tokio::sync::watch::Receiver<bool>,
) {
    let HostSession {
        peer_name,
        mut control_send,
        mut control_recv,
        video,
        input,
    } = session;
    // 会话日志已在上层打印 peer_name；此处仅持有供未来按主控端区分策略用
    let _ = &peer_name;

    // 捕获线程 → channel → 发送任务。
    // 有界通道（容量 4）+ 捕获侧 try_send：发送跟不上时丢新帧保低延迟（T10 背压兜底）——
    // 视频流不能丢中间帧（破坏参考链），丢"整帧不入队"是流媒体标准做法；
    // gop 已缩到 90，丢帧后 ≤3s 内必有 IDR 恢复。
    let (tx, mut rx) = mpsc::channel::<VideoFrame>(4);

    // free-threaded 启动：拿到 CaptureControl，会话结束后可从外部主动停止
    // （关键：静止桌面时 WGC 不产帧，捕获线程自己永远发现不了通道关闭）
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let backlog = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let capture_control = start_capture(tx.clone(), ready_tx, backlog.clone());

    // 首帧加速（T9 议题②）：静止桌面 WGC 不产帧，首个 IDR 要等真实画面变化。
    // 捕获就绪后用 1px 光标微推制造脏区立即逼出一帧；300ms 后再推一次兜住边界竞态。
    if ready_rx.recv_timeout(std::time::Duration::from_secs(3)).is_ok() {
        nudge_cursor();
        std::thread::sleep(std::time::Duration::from_millis(300));
        nudge_cursor();
    }

    // 发送任务：拥有 video 流；channel 关闭或写失败即结束。
    // 统计：字节/帧数（会话汇总）+ backlog（背压监控）+ capture→send 分段延迟
    let backlog2 = backlog.clone();
    let video_task = tokio::spawn(async move {
        let mut video = video;
        let mut sent = 0u64;
        let mut bytes = 0u64;
        let mut lat_sum = 0u64;
        let mut lat_max = 0u64;
        let mut last_report = Instant::now();
        while let Some(frame) = rx.recv().await {
            let len = frame.data.len() as u64;
            match write_frame(&mut video, &Message::VideoFrame(frame.clone())).await {
                Ok(()) => {
                    sent += 1;
                    bytes += len;
                    backlog2.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    let lat = (epoch_us() - frame.capture_pts_us).max(0) as u64;
                    lat_sum += lat;
                    lat_max = lat_max.max(lat);
                }
                Err(e) => {
                    eprintln!("video 流写入失败（主控端断开?）: {e}");
                    break;
                }
            }
            if last_report.elapsed().as_secs() >= 5 && sent > 0 {
                println!(
                    "[send] {}fps | capture→send 平均 {}ms / 最大 {}ms",
                    sent / last_report.elapsed().as_secs().max(1),
                    lat_sum / sent / 1000,
                    lat_max / 1000,
                );
                sent = 0;
                lat_sum = 0;
                lat_max = 0;
                last_report = Instant::now();
            }
        }
        (sent, bytes)
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

    // Control 通道：Ping→Pong（带 host 时戳，client 用于对时）/ Bye / 断开 / 新主控端抢占。
    // 抢占时取消 read_frame 半读会损坏 control 帧边界——但该会话即将整体销毁，无碍。
    loop {
        let msg = tokio::select! {
            m = read_frame(&mut control_recv) => m,
            _ = preempt.changed() => {
                if *preempt.borrow() {
                    println!("新主控端接入，当前会话让位");
                    break;
                }
                continue;
            }
        };
        match msg {
            Ok(Some(Message::Control(ControlMsg::Ping { t_us }))) => {
                let host_recv_us = epoch_us();
                let r = write_frame(
                    &mut control_send,
                    &Message::Control(ControlMsg::Pong {
                        t_us,
                        host_recv_us,
                        host_send_us: epoch_us(),
                    }),
                )
                .await;
                if r.is_err() {
                    break;
                }
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
    //      → channel 排空 → 发送/注入任务结束。
    // 超时只是防悬挂兜底（正常路径秒退）；对端异常消失时流要等 idle timeout(10s) 才报错，
    // 这里不等它——新会话抢占优先（超时后任务自然结束，permit 已释放）
    drop(tx);
    drop(control_send);
    drop(control_recv);
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), async {
        tokio::task::spawn_blocking(move || capture_control.stop()).await
    })
    .await;
    // video 任务可能卡在对端死连接的流控写上（等 idle timeout 才报错）——不等待，
    // 它会自行结束；统计改为尽力而为
    if let Ok(sent) = tokio::time::timeout(std::time::Duration::from_millis(300), video_task).await {
        let (sent, bytes) = sent.unwrap_or((0, 0));
        println!("共发送 {sent} 帧视频（{:.1} MiB）", bytes as f64 / 1048576.0);
    }
    if let Ok(Ok((injected, dropped))) =
        tokio::time::timeout(std::time::Duration::from_millis(300), input_task).await
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
    tx: mpsc::Sender<VideoFrame>,
    /// 待发队列深度（发送任务写完一帧减一；背压/T10 监控）
    backlog: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    start: Instant,
    /// 本统计窗口内的帧数（5s 重置，算窗口 fps）
    window_frames: u64,
    /// 背压丢帧计数（T10）
    dropped: u64,
    encoded: u64,
    /// 本窗口编码耗时样本（µs，报告后清空）
    enc_us: Vec<u64>,
    last_report: Instant,
    scratch: Vec<u8>,
    /// 编码器名称（日志）
    enc_name: &'static str,
}

impl GraphicsCaptureApiHandler for ServeCapture {
    /// Flags：出口通道 + 就绪信号 + 队列深度计数（经 Settings 注入）
    type Flags = (
        mpsc::Sender<VideoFrame>,
        std::sync::mpsc::Sender<()>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    );
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let (tx, ready, backlog) = ctx.flags;
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
        // 通知主任务：WGC 会话已建立，可以推首帧了
        let _ = ready.send(());
        Ok(Self {
            enc: SendEncoder(enc),
            tx,
            backlog,
            start: Instant::now(),
            window_frames: 0,
            dropped: 0,
            encoded: 0,
            enc_us: Vec::new(),
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
        let pts_us = epoch_us();
        let w = frame.width() as usize;

        {
            let fb = frame.buffer()?;
            let bgra = fb.as_nopadding_buffer(&mut self.scratch);
            let t0 = Instant::now();
            let packets = self.enc.0.encode(bgra, w * 4, pts_us)?;
            let e = t0.elapsed().as_micros() as u64;
            self.enc_us.push(e); // 窗口统计（p50/p95）
            let enc_us = e as u32; // 随帧下发（协议 v2，client 侧分段打点）
            for p in packets {
                self.encoded += 1;
                match self.tx.try_send(VideoFrame {
                    capture_pts_us: p.pts_us,
                    key: p.key,
                    encode_us: enc_us,
                    data: p.data,
                }) {
                    Ok(()) => {
                        self.backlog.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // 背压兜底（T10）：发送跟不上 → 整帧丢弃（不破坏参考链）
                        self.dropped += 1;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        // 发送端已关闭（会话结束）→ 停止捕获
                        ctrl.stop();
                        return Ok(());
                    }
                }
            }
        }

        self.window_frames += 1;
        if self.last_report.elapsed().as_secs_f32() >= 5.0 {
            let window_s = self.last_report.elapsed().as_secs_f32();
            let (p50, p95) = pct2(&mut self.enc_us, 0.50, 0.95);
            println!(
                "[stats] {:.1}s: {:.1}fps | 编码 p50={:.1}ms p95={:.1}ms | 待发队列 {} | 丢帧 {} | 累计 {} 包（{}）",
                self.start.elapsed().as_secs_f32(),
                self.window_frames as f32 / window_s,
                p50 as f64 / 1000.0,
                p95 as f64 / 1000.0,
                self.backlog.load(std::sync::atomic::Ordering::Relaxed),
                self.dropped,
                self.encoded,
                self.enc_name,
            );
            self.window_frames = 0;
            self.enc_us.clear();
            self.last_report = Instant::now();
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("[capture] 捕获会话关闭");
        Ok(())
    }
}

/// 窗口样本的 p50/p95（µs）；样本耗尽返回 (0,0)
fn pct2(samples: &mut [u64], a: f64, b: f64) -> (u64, u64) {
    if samples.is_empty() {
        return (0, 0);
    }
    samples.sort_unstable();
    let at = |p: f64| samples[((samples.len() as f64 - 1.0) * p).round() as usize];
    (at(a), at(b))
}

/// 1px 光标微推：制造脏区逼 WGC 出帧（首帧加速的最小实现）
fn nudge_cursor() {
    let (x, y) = crate::input::cursor_pos();
    let (x, y) = (x.max(0) as u32, y.max(0) as u32);
    let _ = crate::input::inject(&rdlink_proto::InputEvent::MouseMove { x: x + 1, y });
    let _ = crate::input::inject(&rdlink_proto::InputEvent::MouseMove { x, y });
}

/// 启动捕获（自由线程）：返回外部控制句柄，会话结束用 `stop()` 主动回收。
fn start_capture(
    tx: mpsc::Sender<VideoFrame>,
    ready: std::sync::mpsc::Sender<()>,
    backlog: std::sync::Arc<std::sync::atomic::AtomicUsize>,
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
        (tx, ready, backlog),
    );
    ServeCapture::start_free_threaded(settings).expect("捕获启动失败")
}
