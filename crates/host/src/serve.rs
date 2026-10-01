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
    /// GPU 着色器 NV12 转换（T3a）。独显收益巨大；弱核显（如 UHD 620）上可能与
    /// QSV 编码抢同一块 GPU 和共享内存带宽，导致 fps 下降——此时可关掉回退
    /// BGRA 直读 + swscale 旧路径。默认 true。
    pub gpu_convert: Option<bool>,
    /// 文件上传落地目录（M3-2，默认 %USERPROFILE%\Downloads）
    pub download_dir: Option<String>,
}

pub fn load_conf() -> HostConf {
    let mut conf: HostConf = std::fs::read_to_string("rdlink.toml")
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
        .and_then(|v| v.get("host").cloned())
        .and_then(|h| h.try_into::<HostConf>().ok())
        .unwrap_or_default();
    // 本机覆盖（rdlink.local.toml，git 忽略）：如被控端弱核显关 gpu_convert，
    // 不改入库配置就能按机器调参
    if let Some(lc) = std::fs::read_to_string("rdlink.local.toml")
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
        .and_then(|v| v.get("host").cloned())
        .and_then(|h| h.try_into::<HostConf>().ok())
    {
        if lc.port.is_some() {
            conf.port = lc.port;
        }
        if lc.cert_dir.is_some() {
            conf.cert_dir = lc.cert_dir;
        }
        if lc.gpu_convert.is_some() {
            conf.gpu_convert = lc.gpu_convert;
        }
    }
    conf
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

/// 进程级编码器缓存（M2-1c）：QSV 会话建立 ~200ms,跨会话按分辨率复用。
/// 复用编码器的新会话首帧必须 force_key() 出 IDR（新客户端没有参考链）。
struct EncCacheEntry {
    w: u32,
    h: u32,
    enc: SendEncoder,
}
static ENC_CACHE: std::sync::Mutex<Option<EncCacheEntry>> = std::sync::Mutex::new(None);

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
    // M3-1 剪贴板同步：进程级双线程（监听/写入），会话只拿通道
    let clip = crate::clipboard::spawn();

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
        let clip_rx = clip.changes.clone();
        let clip_tx = clip.write_tx.clone();
        tokio::spawn(async move {
            let _permit = gate.lock().await; // 前一会话占用捕获时，本会话在此等待
            let _ = preempt_tx.send(false); // 自己上岗后复位信号（供再下一个会话抢占）
            serve_session_inner(session, preempt, clip_rx, clip_tx).await;
            println!("会话结束，等待下一个主控端…");
        });
    }
}

/// 服务一个会话直到断开或被新主控端抢占。
async fn serve_session_inner(
    session: HostSession,
    mut preempt: tokio::sync::watch::Receiver<bool>,
    mut clip_rx: tokio::sync::watch::Receiver<(u64, String)>,
    clip_tx: std::sync::mpsc::Sender<(u64, String)>,
) {
    let HostSession {
        peer_name,
        mut control_send,
        mut control_recv,
        video,
        input,
        connection,
    } = session;
    let _ = &peer_name;

    // M3-2：文件传输流服务（随连接生命周期；accept_bi 在连接关闭时自然退出）
    tokio::spawn(crate::filex::serve(connection.clone()));

    // 捕获线程 → channel → 发送任务。
    // 有界通道（容量 4）+ 捕获侧 try_send：发送跟不上时丢新帧保低延迟（T10 背压兜底）——
    // 视频流不能丢中间帧（破坏参考链），丢"整帧不入队"是流媒体标准做法；
    // gop 已缩到 90，丢帧后 ≤3s 内必有 IDR 恢复。
    let t_session = Instant::now(); // M2-1a：会话启动全程分段计时的原点
    let (tx, mut rx) = mpsc::channel::<VideoFrame>(4);

    // 编码器复用（M2-1c）：从进程缓存取，命中（分辨率一致）则省 ~200ms QSV 会话建立，
    // 并强制新会话首帧出 IDR；未命中（首会话/分辨率变了）走捕获线程内现开。
    let monitor_cur = windows_capture::monitor::Monitor::primary().expect("获取主显示器失败");
    let (cw, ch) = (monitor_cur.width().expect("宽度"), monitor_cur.height().expect("高度"));
    let mut cached_enc: Option<SendEncoder> = None;
    {
        let mut g = ENC_CACHE.lock().expect("编码器缓存锁");
        if let Some(mut e) = g.take() {
            if e.w == cw && e.h == ch {
                e.enc.0.force_key(); // 复用首帧必须 IDR
                println!("[session] 命中编码器缓存（省 QSV/编码器打开耗时）");
                cached_enc = Some(e.enc);
            }
            // 分辨率变了 → 旧编码器直接丢弃，本次现开新的
        }
    }
    // free-threaded 启动：拿到 CaptureControl，会话结束后可从外部主动停止
    // （关键：静止桌面时 WGC 不产帧，捕获线程自己永远发现不了通道关闭）
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let backlog = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let capture_control = start_capture(tx.clone(), ready_tx, backlog.clone(), cached_enc);

    // 发送/输入任务必须先于首帧 nudge 启动：首帧入队后要立刻有人消费，
    // 否则会在 channel 里干等 nudge 兜底的 300ms（M2-1a 插桩实测白丢 ~290ms）。
    // video_dead：捕获链路失活信号（M2.5 看门狗）——捕获线程/GPU 死掉时 control
    // 心跳照常，只有 video 断粮；不回收的话主控端永远挂着冻结画面。
    let (video_dead_tx, mut video_dead_rx) = tokio::sync::watch::channel(false);
    let backlog2 = backlog.clone();
    let video_task = tokio::spawn(async move {
        let mut video = video;
        let mut sent = 0u64;
        let mut bytes = 0u64;
        let mut lat_sum = 0u64;
        let mut lat_max = 0u64;
        let mut last_report = Instant::now();
        // M2.5 活性看门狗：静止桌面 WGC 不产帧属正常，4s 无帧先 nudge 探活
        // （必能逼出一帧）；连续 3 次探不出（~12s）判捕获/GPU 死亡 → 回收会话
        let mut silent_probes = 0u32;
        loop {
            let frame = match tokio::time::timeout(std::time::Duration::from_secs(4), rx.recv()).await {
                Ok(Some(frame)) => {
                    silent_probes = 0;
                    frame
                }
                Ok(None) => break, // 通道关闭（会话回收路径）
                Err(_) => {
                    silent_probes += 1;
                    nudge_cursor();
                    if silent_probes >= 3 {
                        eprintln!("⚠️ 视频链路失活（12s 无帧且 nudge 探不出，疑似捕获/GPU 异常）→ 回收会话");
                        let _ = video_dead_tx.send(true);
                        break;
                    }
                    continue;
                }
            };
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

    // 首帧加速（T9 议题②）：静止桌面 WGC 不产帧，首个 IDR 要等真实画面变化。
    // 捕获就绪后用 1px 光标微推制造脏区立即逼出一帧；300ms 后再推一次兜住边界竞态。
    // 等待放 spawn_blocking（std channel 的 recv_timeout 是阻塞调用，不能挂在 worker 线程上）。
    let ready = tokio::task::spawn_blocking(move || ready_rx.recv_timeout(std::time::Duration::from_secs(3)))
        .await
        .unwrap_or(Err(std::sync::mpsc::RecvTimeoutError::Timeout));
    if ready.is_ok() {
        println!("[session] 会话启动: 会话开始→捕获就绪 {}ms", t_session.elapsed().as_millis());
        nudge_cursor();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        nudge_cursor();
    }

    // M3-1 存量同步：会话建立即把 host 当前剪贴板推给 client——此前复制的内容
    // 在无会话期间不会同步（监听是事件驱动），连接时补发，用户场景"被控端先复制、
    // 主控端后连上"才能拿到内容。读失败（GameViewer 类占用）重试几次。
    {
        let mut text = None;
        for _ in 0..3 {
            if let Some(t) = crate::clipboard::read_clipboard_text() {
                if !t.is_empty() {
                    text = Some(t);
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        if let Some(text) = text {
            if text.len() <= crate::clipboard::MAX_CLIP_TEXT {
                let hash = rdlink_proto::fnv1a64(text.as_bytes());
                let _ = write_frame(
                    &mut control_send,
                    &Message::Control(ControlMsg::ClipboardSync { hash, text }),
                )
                .await;
                println!("[clip] 存量剪贴板已推送给新主控端");
            }
        }
    }

    // Control 通道：Ping→Pong（带 host 时戳，client 用于对时）/ Bye / 断开 / 新主控端抢占。
    // 抢占时取消 read_frame 半读会损坏 control 帧边界——但该会话即将整体销毁，无碍。
    // 失联看门狗（M2-1c）：client 被强杀时 QUIC 要等 idle timeout(10s) 才报错，期间死会话
    // 僵占捕获、下一个主控端要先等它回收；client 正常每 500ms 一个 Ping，3s 无 Ping 即判失联。
    let mut last_ping = tokio::time::Instant::now();
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
            _ = video_dead_rx.changed() => {
                // 捕获链路死亡（M2.5 看门狗）：心跳还在但画面断了，必须回收，
                // 否则主控端永远挂着冻结帧
                if *video_dead_rx.borrow() {
                    println!("视频链路失活，回收会话");
                    break;
                }
                continue;
            }
            _ = clip_rx.changed() => {
                // M3-1：本端剪贴板变化 → 同步给 client（空文本是初始值，跳过）
                let (hash, text) = clip_rx.borrow().clone();
                if !text.is_empty() {
                    let r = write_frame(
                        &mut control_send,
                        &Message::Control(ControlMsg::ClipboardSync { hash, text }),
                    )
                    .await;
                    if r.is_err() {
                        break;
                    }
                }
                continue;
            }
            _ = tokio::time::sleep_until(last_ping + std::time::Duration::from_secs(3)) => {
                println!("主控端失联（3s 无 Ping），回收会话");
                break;
            }
        };
        match msg {
            Ok(Some(Message::Control(ControlMsg::Ping { t_us }))) => {
                last_ping = tokio::time::Instant::now();
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
            Ok(Some(Message::Control(ControlMsg::ClipboardSync { hash, text }))) => {
                // M3-1：对端剪贴板 → 交写线程落本机（防回环由 LAST_SYNCED 统一裁决）
                if text.len() <= crate::clipboard::MAX_CLIP_TEXT {
                    let _ = clip_tx.send((hash, text));
                }
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
    // 回收并行化（M2-1c）：被强杀的 client 会让 input 流读挂到超时——stop/video/input
    // 三步串行最坏 900ms，并行后封顶 300ms（抢占路径上新主控端少等一半）；
    // 编码器收回在 stop 完成后经 callback() try_lock 直取。
    let handler = capture_control.callback();
    let (_r_stop, r_video, r_input) = tokio::join!(
        tokio::time::timeout(std::time::Duration::from_millis(300), async {
            tokio::task::spawn_blocking(move || capture_control.stop()).await
        }),
        tokio::time::timeout(std::time::Duration::from_millis(300), video_task),
        tokio::time::timeout(std::time::Duration::from_millis(300), input_task),
    );
    if let Some(mut h) = handler.try_lock() {
        if let Some(e) = h.enc.take() {
            *ENC_CACHE.lock().expect("编码器缓存锁") = Some(EncCacheEntry { w: cw, h: ch, enc: e });
            println!("[session] 编码器已归缓存");
        }
    }
    // video/input 任务尽力收统计（对端死连接的阻塞读由超时兜底）
    if let Ok(sent) = r_video {
        let (sent, bytes) = sent.unwrap_or((0, 0));
        println!("共发送 {sent} 帧视频（{:.1} MiB）", bytes as f64 / 1048576.0);
    }
    if let Ok(Ok((injected, dropped))) = r_input {
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

/// 捕获线程启动参数（经 Settings Flags 注入）
struct CaptureBoot {
    tx: mpsc::Sender<VideoFrame>,
    ready: std::sync::mpsc::Sender<()>,
    backlog: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// 启动时刻（M2-1a 分段计时）
    launch: Instant,
    /// 进程缓存命中的已打开编码器（None = 现开）
    enc: Option<SendEncoder>,
}

struct ServeCapture {
    enc: Option<SendEncoder>,
    tx: mpsc::Sender<VideoFrame>,
    /// 待发队列深度（发送任务写完一帧减一；背压/T10 监控）
    backlog: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// T3a：GPU VideoProcessor 转换器（首个帧纹理到达时惰性初始化；
    /// None+未死 = 还没初始化；None+死 = 初始化失败永久回退 BGRA 路径）
    gpu: Option<crate::gpu::GpuConverter>,
    gpu_dead: bool,
    start: Instant,
    /// 本统计窗口内的帧数（5s 重置，算窗口 fps）
    window_frames: u64,
    /// 背压丢帧计数（T10）
    dropped: u64,
    encoded: u64,
    /// 本窗口编码耗时样本（µs，报告后清空）
    enc_us: Vec<u64>,
    /// 本窗口 GPU 转换+回读耗时样本（µs，T3a 数据）
    gpu_us: Vec<u64>,
    last_report: Instant,
    scratch: Vec<u8>,
    /// 编码器名称（日志）
    enc_name: &'static str,
    /// 捕获线程启动时刻（start_free_threaded 调用瞬间，M2-1a 首帧分段计时）
    launch: Instant,
}

impl GraphicsCaptureApiHandler for ServeCapture {
    type Flags = CaptureBoot;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let CaptureBoot { tx, ready, backlog, launch, enc } = ctx.flags;
        // M2-1a 分段计时：start_free_threaded → new() = WGC 会话激活；
        // open_auto 内部 = NVENC 探测 + QSV/x264 打开（缓存命中时两者皆 0）
        let activation_ms = launch.elapsed().as_millis();
        let monitor = windows_capture::monitor::Monitor::primary()?;
        let (w, h) = (monitor.width()?, monitor.height()?);
        let (enc, enc_open_ms, reused) = match enc {
            Some(e) => (e, 0, true),
            None => {
                let t_enc = Instant::now();
                let (e, _tier) = encoder::open_auto(w, h)?;
                (SendEncoder(e), t_enc.elapsed().as_millis(), false)
            }
        };
        let enc_name = enc.0.name();
        println!(
            "[capture] 启动分段: WGC 激活 {activation_ms}ms | 编码器打开 {enc_open_ms}ms{}（{enc_name}）@ {w}x{h}",
            if reused { "（缓存复用）" } else { "" },
        );
        // 通知主任务：WGC 会话已建立，可以推首帧了
        let _ = ready.send(());
        Ok(Self {
            enc: Some(enc),
            tx,
            backlog,
            gpu: None,
            gpu_dead: !load_conf().gpu_convert.unwrap_or(true),
            start: Instant::now(),
            window_frames: 0,
            dropped: 0,
            encoded: 0,
            enc_us: Vec::new(),
            gpu_us: Vec::new(),
            last_report: Instant::now(),
            scratch: Vec::new(),
            enc_name,
            launch,
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
            let Some(enc) = self.enc.as_mut() else {
                ctrl.stop();
                return Ok(());
            };
            // T3a：优先 GPU 转换（VideoProcessor BGRA→NV12 + 3MB 回读，替代 swscale 10ms）。
            // 转换器在首个帧纹理上惰性初始化（设备从纹理 GetDevice，保证与 WGC 同设备）；
            // 初始化或运行失败 → 永久回退 BGRA 直读 + 编码器内 swscale 旧路径。
            let mut t0: Option<Instant> = None;
            let packets = {
                let mut gpu_out: Option<Vec<encoder::EncodedPacket>> = None;
                if !self.gpu_dead {
                    if self.gpu.is_none() {
                        let tex = frame.as_raw_texture();
                        let dev = unsafe { tex.GetDevice() }.ok();
                        let conv_result = match dev {
                            Some(d) => crate::gpu::GpuConverter::new(
                                &d,
                                frame.width() as u32,
                                frame.height() as u32,
                            ),
                            None => Err(windows::core::Error::from_hresult(
                                windows::core::HRESULT(-1),
                            )),
                        };
                        match conv_result {
                            Ok(g) => {
                                println!("[gpu] 着色器 NV12 转换器就绪（GPU 路径生效）");
                                self.gpu = Some(g);
                            }
                            Err(e) => {
                                eprintln!("⚠️ [gpu] GPU 转换器初始化失败({e}) → 回退 BGRA+swscale 旧路径");
                                self.gpu_dead = true;
                            }
                        }
                    }
                    if let Some(gpu) = self.gpu.as_mut() {
                        let tex = frame.as_raw_texture();
                        let tg = Instant::now();
                        match gpu.convert_and_readback(tex) {
                            Ok(nv) => {
                                self.gpu_us.push(tg.elapsed().as_micros() as u64);
                                t0 = Some(Instant::now());
                                gpu_out = Some(enc.0.encode(
                                    encoder::FrameSrc::Nv12 { buf: nv.buf, pitch: nv.pitch },
                                    pts_us,
                                )?);
                            }
                            Err(e) => {
                                eprintln!("⚠️ [gpu] 转换/回读失败({e}) → 永久回退 BGRA+swscale 旧路径");
                                self.gpu = None;
                                self.gpu_dead = true;
                            }
                        }
                    }
                }
                match gpu_out {
                    Some(p) => p,
                    None => {
                        // 旧路径：WGC 直读 BGRA
                        let fb = frame.buffer()?;
                        let bgra = fb.as_nopadding_buffer(&mut self.scratch);
                        t0 = Some(Instant::now());
                        enc.0.encode(encoder::FrameSrc::Bgra { buf: bgra, pitch: w * 4 }, pts_us)?
                    }
                }
            };
            let t0 = t0.expect("编码计时起点必然被赋值");
            let e = t0.elapsed().as_micros() as u64;
            self.enc_us.push(e); // 窗口统计（p50/p95）
            let enc_us = e as u32; // 随帧下发（协议 v2，client 侧分段打点）
            for p in packets {
                self.encoded += 1;
                if self.encoded == 1 {
                    // M2-1a：首帧分段——线程启动→首帧编码完成入队（含 WGC 激活+编码器打开+等脏区）
                    println!(
                        "[capture] 首帧就绪: 线程启动→首帧入队 {}ms（本帧编码 {}ms）",
                        self.launch.elapsed().as_millis(),
                        enc_us / 1000,
                    );
                }
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
                        // 发送端已关闭（会话结束）→ 停止捕获（编码器由会话侧经 callback() 收回）
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
            let (g50, _) = pct2(&mut self.gpu_us, 0.50, 0.95);
            let gpu_label = if self.gpu_us.is_empty() { "（BGRA回退）" } else { "（NV12 via GPU）" };
            println!(
                "[stats] {:.1}s: {:.1}fps | GPU转+读 p50={:.1}ms{} | 编码 p50={:.1}ms p95={:.1}ms | 待发队列 {} | 丢帧 {} | 累计 {} 包（{}）",
                self.start.elapsed().as_secs_f32(),
                self.window_frames as f32 / window_s,
                g50 as f64 / 1000.0,
                gpu_label,
                p50 as f64 / 1000.0,
                p95 as f64 / 1000.0,
                self.backlog.load(std::sync::atomic::Ordering::Relaxed),
                self.dropped,
                self.encoded,
                self.enc_name,
            );
            self.window_frames = 0;
            self.enc_us.clear();
            self.gpu_us.clear();
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
    enc: Option<SendEncoder>,
) -> windows_capture::capture::CaptureControl<ServeCapture, Box<dyn std::error::Error + Send + Sync>> {
    let monitor = windows_capture::monitor::Monitor::primary().expect("主显示器");
    let launch = Instant::now(); // M2-1a：捕获线程启动计时原点
    let boot = CaptureBoot { tx, ready, backlog, launch, enc };
    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        boot,
    );
    ServeCapture::start_free_threaded(settings).expect("捕获启动失败")
}
