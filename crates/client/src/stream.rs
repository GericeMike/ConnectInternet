//! T7：client 正常运行模式——连接 host → 收流解码（工作线程）→ 帧槽 → winit/wgpu 渲染。
//!
//! 结构：
//! ```text
//! 工作线程: QUIC 收包 → IDR 起播 → 软解 → NV12/YUV→BGRA → 存入帧槽 → request_redraw
//! 主线程  : winit 事件循环，RedrawRequested 取最新帧 → Display 提交渲染（最新帧优先，丢旧帧）
//! ```

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use rdlink_proto::{ControlMsg, InputEvent, Message};
use rdlink_transport::{connect, read_frame, write_frame};

use crate::decoder::StreamDecoder;
use crate::display::Display;
use crate::input_map::vk_from_key;

/// client 进程时钟原点（与对时/Ping 时戳共用时钟域）
static CLIENT_EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
fn client_epoch_us() -> i64 {
    CLIENT_EPOCH.get().map(|t| t.elapsed().as_micros() as i64).unwrap_or(0)
}

/// 网络对时结果（control 任务写，视频循环/标题读）
#[derive(Default)]
struct NetStats {
    rtt_us: AtomicU64,
    /// client_epoch → host_epoch 的偏移：host_time = client_time + offset
    offset_us: AtomicI64,
    pongs: AtomicU32,
}

/// 每秒汇总给标题栏的统计（视频循环写，渲染线程读）
#[derive(Default)]
struct UiStats {
    fps: AtomicU64,
    e2e_p50_us: AtomicU64,
    e2e_p95_us: AtomicU64,
    enc_avg_us: AtomicU64,
    dec_avg_us: AtomicU64,
    rtt_us: AtomicU64,
    /// 连接状态：0=正常 1=连接中 2=重连中（M2.5 自动重连）
    conn_state: AtomicU64,
    conn_attempt: AtomicU64,
}

/// 生产者-渲染共享帧槽（最新帧优先：生产者覆盖，渲染者取走）。
type FrameSlot = Arc<Mutex<Option<FrameBuf>>>;
/// host 画面分辨率（连接后由收流线程写入，输入坐标映射用）
type HostSize = Arc<Mutex<(u32, u32)>>;
/// 输入事件出口槽（按会话重建：断线重连后旧通道作废，收流线程换入新 Sender）
type InputTxSlot = Arc<Mutex<Option<mpsc::UnboundedSender<InputEvent>>>>;

struct FrameBuf {
    bgra: Vec<u8>,
    w: u32,
    h: u32,
    /// 本地接收完成时刻（测"解码→渲染"段延迟用）
    recv_at: Instant,
}

/// client 配置（rdlink.toml [client] 节，文件不存在用默认值）
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct ClientConf {
    vsync: Option<bool>,
}

fn load_conf() -> ClientConf {
    std::fs::read_to_string("rdlink.toml")
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
        .and_then(|v| v.get("client").cloned())
        .and_then(|c| c.try_into::<ClientConf>().ok())
        .unwrap_or_default()
}

pub fn run(addr: &str, pin: &str) {
    let _ = CLIENT_EPOCH.set(Instant::now());
    let vsync = load_conf().vsync.unwrap_or(false);
    let event_loop = EventLoop::new().expect("事件循环创建失败");
    let mut app = StreamApp::new(addr.to_string(), pin.to_string(), vsync);
    event_loop.run_app(&mut app).expect("事件循环异常退出");
    println!("client 已退出");
}

struct StreamApp {
    addr: String,
    pin: String,
    window: Option<Arc<Window>>,
    display: Option<Display>,
    /// 渲染 vsync 开关（rdlink.toml [client].vsync，默认关=低延迟）
    display_vsync: bool,
    slot: FrameSlot,
    host_size: HostSize,
    /// 输入事件出口槽（winit 主线程 → 当前会话的发送任务；重连时由收流线程换新）
    input_tx_slot: InputTxSlot,
    /// 退出信号（主线程 → 收流线程：先把 Bye 真正发完再退出）
    shutdown_tx: Option<mpsc::Sender<()>>,
    fullscreen: bool,
    // 渲染统计
    rendered: u64,
    last_title: Instant,
    title_frames: u64,
    /// 每秒汇总统计（视频线程写，标题栏读）
    ui: Arc<UiStats>,
    /// 首帧渲染耗时（连接开始→首帧上屏）
    pub first_frame_ms: Option<f32>,
    connect_started: Option<Instant>,
}

impl StreamApp {
    fn new(addr: String, pin: String, vsync: bool) -> Self {
        Self {
            addr,
            pin,
            window: None,
            display: None,
            display_vsync: vsync,
            slot: Arc::new(Mutex::new(None)),
            host_size: Arc::new(Mutex::new((0, 0))),
            input_tx_slot: Arc::new(Mutex::new(None)),
            shutdown_tx: None,
            fullscreen: false,
            rendered: 0,
            last_title: Instant::now(),
            title_frames: 0,
            ui: Arc::new(UiStats::default()),
            first_frame_ms: None,
            connect_started: None,
        }
    }

    fn forward_input(&self, ev: InputEvent) {
        if let Some(tx) = self.input_tx_slot.lock().unwrap().as_ref() {
            let _ = tx.send(ev); // 通道关闭（会话已断）时静默丢弃
        }
    }

    fn spawn_stream_thread(
        &self,
        window: Arc<Window>,
        shutdown_rx: mpsc::Receiver<()>,
    ) {
        let addr = self.addr.clone();
        let pin = self.pin.clone();
        let slot = self.slot.clone();
        let host_size = self.host_size.clone();
        let ui = self.ui.clone();
        let input_tx_slot = self.input_tx_slot.clone();
        std::thread::Builder::new()
            .name("rdlink-stream".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(stream_loop(
                    addr, pin, slot, host_size, ui, input_tx_slot, window, shutdown_rx,
                ));
            })
            .expect("收流线程创建失败");
    }

    /// 通知收流线程优雅收尾（发 Bye），并给它一点时间真正送出
    fn begin_shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.blocking_send(());
            // Bye 很小，1.5s 足够 QUIC 完成发送
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
    }
}

/// 会话结束原因
enum SessionExit {
    /// 用户主动退出（Esc/关窗）
    User,
    /// 连接丢失（M2.5：监督循环自动重连）
    Lost,
}

/// 收流+解码（工作线程）。M2.5 起为监督循环：连接丢失后指数退避自动重连，
/// 窗口/渲染全程复用（画面保留最后一帧，标题栏显示重连进度）。
async fn stream_loop(
    addr: String,
    pin: String,
    slot: FrameSlot,
    host_size: HostSize,
    ui: Arc<UiStats>,
    input_tx_slot: InputTxSlot,
    window: Arc<Window>,
    mut shutdown_rx: mpsc::Receiver<()>,
) {
    let mut attempt: u64 = 0;
    // M3-1 剪贴板：进程级 poll/write 双线程，会话只拿通道
    let clip = crate::clipboard::spawn();
    crate::clipboard::prime_with_local(); // 存量方向为 host→client，抑制 client 首轮推送
    loop {
        // 重连退避：1s→2s→4s→8s→10s 封顶（首次连接不等待）
        if attempt > 0 {
            let backoff = Duration::from_millis(std::cmp::min(500u64 << attempt.min(5), 10_000));
            ui.conn_state.store(2, Ordering::Relaxed);
            ui.conn_attempt.store(attempt, Ordering::Relaxed);
            println!("第 {attempt} 次重连（{backoff:?} 后重试）…");
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = shutdown_rx.recv() => return, // 等待重连期间用户退出
            }
        }
        ui.conn_state.store(1, Ordering::Relaxed);
        let t0 = Instant::now();
        let session = match connect(addr.parse().expect("地址格式: ip:port"), &pin, "rdlink-client").await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("连接失败: {e}");
                attempt += 1;
                continue;
            }
        };
        println!("已连接 host: {}（握手 {:?}）", session.peer_name, t0.elapsed());
        attempt = 0;
        ui.conn_state.store(0, Ordering::Relaxed);

        // 输入通道按会话重建：换入新 Sender，旧通道随旧会话销毁
        // （断线期间积在旧通道里的事件直接作废，不重放）
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        *input_tx_slot.lock().unwrap() = Some(input_tx);

        match session_run(
            session, input_rx, slot.clone(), host_size.clone(), ui.clone(), window.clone(),
            clip.changes.clone(), clip.write_tx.clone(), &mut shutdown_rx,
        )
        .await
        {
            SessionExit::User => {
                *input_tx_slot.lock().unwrap() = None;
                return;
            }
            SessionExit::Lost => {
                *input_tx_slot.lock().unwrap() = None;
                attempt += 1; // 退避从 1s 起
            }
        }
    }
}

/// 单个会话：对时 + 视频收流解码 + 输入转发 + 剪贴板同步，直到断线或用户退出。
#[allow(clippy::too_many_arguments)]
async fn session_run(
    session: rdlink_transport::ClientSession,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
    slot: FrameSlot,
    host_size: HostSize,
    ui: Arc<UiStats>,
    window: Arc<Window>,
    mut clip_rx: tokio::sync::watch::Receiver<(u64, String)>,
    clip_tx: std::sync::mpsc::Sender<(u64, String)>,
    shutdown: &mut mpsc::Receiver<()>,
) -> SessionExit {
    let rdlink_transport::ClientSession {
        peer_name: _,
        video_info,
        control_send,
        control_recv,
        mut video,
        mut input,
    } = session;

    // 记录 host 分辨率（输入坐标映射用）
    if let ControlMsg::VideoStreamInfo { width, height, .. } = &video_info {
        println!("视频参数: {width}x{height}");
        *host_size.lock().unwrap() = (*width, *height);
    }

    // 输入发送任务：winit 主线程 → input_rx → Input 流（写失败=断线，任务退出）
    tokio::spawn(async move {
        while let Some(ev) = input_rx.recv().await {
            if write_frame(&mut input, &Message::Input(ev)).await.is_err() {
                break; // host 断开
            }
        }
    });

    // ---- 对时（T9）：Ping 每 500ms；Pong 四时间戳法算 offset/RTT ----
    let net = Arc::new(NetStats::default());
    {
        let net = net.clone();
        let clip_tx = clip_tx.clone();
        let mut control_recv = control_recv;
        // 收方向：只读不 select（避免半读取消破坏帧同步）。
        // Pong → 对时；ClipboardSync → 交写线程落本机（其余忽略）
        tokio::spawn(async move {
            while let Ok(Some(msg)) = read_frame(&mut control_recv).await {
                match msg {
                    Message::Control(ControlMsg::Pong { t_us, host_recv_us, host_send_us }) => {
                        let now = client_epoch_us();
                        net.rtt_us.store((now - t_us).max(0) as u64, Ordering::Relaxed);
                        net.offset_us.store(
                            ((host_recv_us + host_send_us) / 2) - ((t_us + now) / 2),
                            Ordering::Relaxed,
                        );
                        net.pongs.fetch_add(1, Ordering::Relaxed);
                    }
                    Message::Control(ControlMsg::ClipboardSync { hash, text }) => {
                        if text.len() <= crate::clipboard::MAX_CLIP_TEXT {
                            let _ = clip_tx.send((hash, text));
                        }
                    }
                    _ => {}
                }
            }
        });
    }
    let (bye_tx, mut bye_rx) = mpsc::channel::<String>(1);
    {
        let mut control_send = control_send;
        // 发方向：定时 Ping；会话结束后经 bye 通道发 Bye
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if write_frame(&mut control_send, &Message::Control(ControlMsg::Ping {
                            t_us: client_epoch_us(),
                        }))
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                    reason = bye_rx.recv() => {
                        let _ = write_frame(
                            &mut control_send,
                            &Message::Control(ControlMsg::Bye { reason: reason.unwrap_or_default() }),
                        )
                        .await;
                        break;
                    }
                    // M3-1：本端剪贴板变化 → 同步给 host（空文本是初始值，跳过）
                    _ = clip_rx.changed() => {
                        let (hash, text) = clip_rx.borrow().clone();
                        if !text.is_empty()
                            && write_frame(
                                &mut control_send,
                                &Message::Control(ControlMsg::ClipboardSync { hash, text }),
                            )
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    }

    let mut decoder = match StreamDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("解码器初始化失败: {e}");
            return SessionExit::Lost;
        }
    };
    let mut got_idr = false;
    let mut skipped_before_idr = 0u64;
    let mut decode_buf: Vec<u8> = Vec::new();
    // 每秒统计累积器
    let mut e2e_samples: Vec<u64> = Vec::with_capacity(128);
    let mut enc_sum = 0u64;
    let mut dec_sum = 0u64;
    let mut sec_frames = 0u64;
    let mut sec_started = Instant::now();
    let mut exit = SessionExit::Lost;

    loop {
        // select 场景取消 read_frame 半读会丢帧——仅在退出时发生，可接受
        let msg = tokio::select! {
            m = read_frame(&mut video) => match m.transpose() {
                Some(Ok(m)) => m,
                Some(Err(e)) => {
                    eprintln!("video 流错误: {e}");
                    break;
                }
                None => {
                    println!("video 流关闭");
                    break;
                }
            },
            _ = shutdown.recv() => {
                // 干净退出：经 control 任务发 Bye（host 立刻感知回收）
                let _ = bye_tx.send("client 退出".into()).await;
                tokio::time::sleep(Duration::from_millis(200)).await;
                exit = SessionExit::User;
                break;
            }
        };
        let Message::VideoFrame(f) = msg else {
            // M3-1：对端剪贴板 → 交写线程落本机（防回环由 LAST_SYNCED 统一裁决）
            if let Message::Control(ControlMsg::ClipboardSync { hash, text }) = msg {
                if text.len() <= crate::clipboard::MAX_CLIP_TEXT {
                    let _ = clip_tx.send((hash, text));
                }
            }
            continue;
        };

        // D4：IDR 起播
        if !got_idr {
            if !f.key {
                skipped_before_idr += 1;
                continue;
            }
            got_idr = true;
            println!("首 IDR 到达（丢弃 {skipped_before_idr} 个非关键包）");
        }

        // 端到端（host 采集 → client 收包），对时有效后才有意义。
        // host 时钟 = client 时钟 + offset（四时间戳法），故 c_capture = pts - offset
        // e2e = c_now - c_capture = c_now + offset - pts
        if net.pongs.load(Ordering::Relaxed) > 0 {
            let offset = net.offset_us.load(Ordering::Relaxed) as i64;
            let e2e = (client_epoch_us() + offset - f.capture_pts_us).max(0) as u64;
            e2e_samples.push(e2e);
        }
        enc_sum += f.encode_us as u64;

        let t_dec = Instant::now();
        if let Err(e) = decoder.decode(&f.data) {
            eprintln!("解码错误: {e}");
            break;
        }
        dec_sum += t_dec.elapsed().as_micros() as u64;

        let last = decoder.last_bgra();
        if last.width > 0 && !last.bgra.is_empty() {
            decode_buf.resize(last.bgra.len(), 0);
            decode_buf.copy_from_slice(last.bgra);
            let mut guard = slot.lock().unwrap();
            // 缓冲复用：取回旧帧的 Vec 覆盖使用，避免每帧 8MB 分配
            let old = guard
                .replace(FrameBuf {
                    bgra: std::mem::take(&mut decode_buf),
                    w: last.width,
                    h: last.height,
                    recv_at: Instant::now(),
                });
            drop(guard);
            if let Some(old) = old {
                decode_buf = old.bgra;
            }
            window.request_redraw();
        }

        // 每秒汇总 → UiStats（标题栏读）
        sec_frames += 1;
        if sec_started.elapsed() >= Duration::from_secs(1) {
            e2e_samples.sort_unstable();
            let p = |q: usize| e2e_samples.get(e2e_samples.len() * q / 100).copied().unwrap_or(0);
            ui.fps.store(sec_frames, Ordering::Relaxed);
            ui.e2e_p50_us.store(p(50), Ordering::Relaxed);
            ui.e2e_p95_us.store(p(95), Ordering::Relaxed);
            ui.enc_avg_us.store(enc_sum / sec_frames.max(1) as u64, Ordering::Relaxed);
            ui.dec_avg_us.store(dec_sum / sec_frames.max(1) as u64, Ordering::Relaxed);
            ui.rtt_us.store(net.rtt_us.load(Ordering::Relaxed), Ordering::Relaxed);
            println!(
                "[stats] {}fps | e2e p50={}ms p95={}ms | enc {}ms dec {}ms | rtt {}ms",
                sec_frames,
                p(50) / 1000,
                p(95) / 1000,
                enc_sum / sec_frames.max(1) as u64 / 1000,
                dec_sum / sec_frames.max(1) as u64 / 1000,
                net.rtt_us.load(Ordering::Relaxed) / 1000,
            );
            e2e_samples.clear();
            enc_sum = 0;
            dec_sum = 0;
            sec_frames = 0;
            sec_started = Instant::now();
        }
    }

    println!(
        "会话结束（{}）（共解码 {} 帧）",
        match exit {
            SessionExit::User => "用户退出",
            SessionExit::Lost => "连接丢失",
        },
        decoder.frames
    );
    exit
}

impl ApplicationHandler for StreamApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // 默认带边框窗口（最小化/最大化/关闭按钮齐全），F11 切全屏
        let attrs = Window::default_attributes()
            .with_title("rdlink client（连接中…）")
            .with_inner_size(winit::dpi::LogicalSize::new(1440.0, 860.0))
            .with_min_inner_size(winit::dpi::LogicalSize::new(640.0, 360.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("窗口创建失败"));
        let mut display = Display::new(window.clone(), self.display_vsync);

        println!("渲染就绪，连接 {} …（窗口模式 | F11 切全屏 | Esc 退出）", self.addr);

        self.connect_started = Some(Instant::now());
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        self.shutdown_tx = Some(shutdown_tx);
        self.spawn_stream_thread(window.clone(), shutdown_rx);
        display.render();
        self.display = Some(display);
        self.window = Some(window);
        self.window.as_ref().unwrap().request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(window) = self.window.clone() else { return };
        match event {
            WindowEvent::CloseRequested => {
                self.begin_shutdown();
                event_loop.exit();
            }

            WindowEvent::Resized(size) => {
                if let Some(d) = self.display.as_mut() {
                    d.resize(size.width, size.height);
                }
            }

            // ---------- T8 输入捕获（本地热键先拦截，其余转发 host） ----------
            WindowEvent::KeyboardInput { event, .. } => {
                // 本地热键
                if event.state == ElementState::Pressed {
                    match event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.begin_shutdown();
                            event_loop.exit();
                        }
                        Key::Named(NamedKey::F11) => {
                            self.fullscreen = !self.fullscreen;
                            if self.fullscreen {
                                window.set_fullscreen(Some(Fullscreen::Borderless(None)));
                            } else {
                                window.set_fullscreen(None);
                            }
                            return;
                        }
                        _ => {}
                    }
                }
                // 转发（物理键 → VK）
                if let Some(vk) = vk_from_key(event.physical_key) {
                    self.forward_input(InputEvent::Key {
                        vk,
                        down: event.state == ElementState::Pressed,
                    });
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                // 窗口物理坐标 → 归一化 → host 屏幕像素
                let inner = window.inner_size();
                if inner.width > 0 && inner.height > 0 {
                    let (hw, hh) = *self.host_size.lock().unwrap();
                    if hw > 0 && hh > 0 {
                        let x = ((position.x / inner.width as f64) * hw as f64) as u32;
                        let y = ((position.y / inner.height as f64) * hh as f64) as u32;
                        self.forward_input(InputEvent::MouseMove {
                            x: x.min(hw - 1),
                            y: y.min(hh - 1),
                        });
                    }
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    MouseButton::Left => rdlink_proto::MouseButton::Left,
                    MouseButton::Right => rdlink_proto::MouseButton::Right,
                    MouseButton::Middle => rdlink_proto::MouseButton::Middle,
                    MouseButton::Back => rdlink_proto::MouseButton::X1,
                    MouseButton::Forward => rdlink_proto::MouseButton::X2,
                    _ => return,
                };
                self.forward_input(InputEvent::MouseButton {
                    button: b,
                    down: state == ElementState::Pressed,
                });
            }

            WindowEvent::MouseWheel { delta, .. } => {
                // LineDelta 单位 = 滚轮格；PixelDelta（精密触控板）按 ~53px/格折算
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x as i32, y as i32),
                    MouseScrollDelta::PixelDelta(p) => {
                        ((p.x / 53.0) as i32, (p.y / 53.0) as i32)
                    }
                };
                if dx != 0 || dy != 0 {
                    self.forward_input(InputEvent::MouseWheel { dx, dy });
                }
            }

            WindowEvent::RedrawRequested => {
                let Some(d) = self.display.as_mut() else { return };
                // 取最新帧（渲染不及时则旧帧被覆盖丢弃——丢旧保新）
                let frame = self.slot.lock().unwrap().take();
                if let Some(f) = frame {
                    let hold_us = f.recv_at.elapsed().as_micros();
                    d.submit_bgra(f.w, f.h, &f.bgra);
                    d.render();
                    self.rendered += 1;
                    self.title_frames += 1;

                    if self.first_frame_ms.is_none() {
                        if let Some(t0) = self.connect_started {
                            self.first_frame_ms = Some(t0.elapsed().as_secs_f32() * 1000.0);
                            println!(
                                "★ 首帧上屏：连接→渲染 {:.0}ms（T7 验收线 1000ms）",
                                self.first_frame_ms.unwrap()
                            );
                        }
                    }
                    let _ = hold_us; // T9 打点用
                }

                if self.last_title.elapsed().as_secs_f32() >= 1.0 {
                    let u = &self.ui;
                    let state = u.conn_state.load(Ordering::Relaxed);
                    if state > 0 {
                        // 断线/连接中：标题栏给重连反馈（画面保留最后一帧）
                        let label = if state == 2 { "重连中" } else { "连接中" };
                        window.set_title(&format!(
                            "rdlink | {label}（第 {} 次）… | rtt {}ms",
                            u.conn_attempt.load(Ordering::Relaxed),
                            u.rtt_us.load(Ordering::Relaxed) / 1000,
                        ));
                    } else {
                        let r = |v: &AtomicU64| v.load(Ordering::Relaxed) / 1000;
                        window.set_title(&format!(
                            "rdlink | {}fps | e2e {}ms(p95 {}) | enc {} dec {} | rtt {}",
                            u.fps.load(Ordering::Relaxed),
                            r(&u.e2e_p50_us),
                            r(&u.e2e_p95_us),
                            r(&u.enc_avg_us),
                            r(&u.dec_avg_us),
                            r(&u.rtt_us),
                        ));
                    }
                    self.title_frames = 0;
                    self.last_title = Instant::now();
                }
                // 无新帧不主动重绘（等待 request_redraw），降低空转 CPU
            }

            _ => {}
        }
    }
}
