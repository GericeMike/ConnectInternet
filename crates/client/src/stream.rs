//! T7：client 正常运行模式——连接 host → 收流解码（工作线程）→ 帧槽 → winit/wgpu 渲染。
//!
//! 结构：
//! ```text
//! 工作线程: QUIC 收包 → IDR 起播 → 软解 → NV12/YUV→BGRA → 存入帧槽 → request_redraw
//! 主线程  : winit 事件循环，RedrawRequested 取最新帧 → Display 提交渲染（最新帧优先，丢旧帧）
//! ```

use std::sync::{Arc, Mutex};
use std::time::Instant;

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

/// 生产者-渲染共享帧槽（最新帧优先：生产者覆盖，渲染者取走）。
type FrameSlot = Arc<Mutex<Option<FrameBuf>>>;
/// host 画面分辨率（连接后由收流线程写入，输入坐标映射用）
type HostSize = Arc<Mutex<(u32, u32)>>;

struct FrameBuf {
    bgra: Vec<u8>,
    w: u32,
    h: u32,
    /// 本地接收完成时刻（测"解码→渲染"段延迟用）
    recv_at: Instant,
}

pub fn run(addr: &str, pin: &str) {
    let event_loop = EventLoop::new().expect("事件循环创建失败");
    let mut app = StreamApp::new(addr.to_string(), pin.to_string());
    event_loop.run_app(&mut app).expect("事件循环异常退出");
    println!("client 已退出");
}

struct StreamApp {
    addr: String,
    pin: String,
    window: Option<Arc<Window>>,
    display: Option<Display>,
    slot: FrameSlot,
    host_size: HostSize,
    /// 输入事件出口（winit 主线程 → 收流线程的发送任务）
    input_tx: Option<mpsc::UnboundedSender<InputEvent>>,
    fullscreen: bool,
    // 渲染统计
    rendered: u64,
    last_title: Instant,
    title_frames: u64,
    /// 首帧渲染耗时（连接开始→首帧上屏）
    pub first_frame_ms: Option<f32>,
    connect_started: Option<Instant>,
}

impl StreamApp {
    fn new(addr: String, pin: String) -> Self {
        Self {
            addr,
            pin,
            window: None,
            display: None,
            slot: Arc::new(Mutex::new(None)),
            host_size: Arc::new(Mutex::new((0, 0))),
            input_tx: None,
            fullscreen: false,
            rendered: 0,
            last_title: Instant::now(),
            title_frames: 0,
            first_frame_ms: None,
            connect_started: None,
        }
    }

    fn forward_input(&self, ev: InputEvent) {
        if let Some(tx) = &self.input_tx {
            let _ = tx.send(ev); // 通道关闭（会话已断）时静默丢弃
        }
    }

    fn spawn_stream_thread(
        &self,
        window: Arc<Window>,
        input_rx: mpsc::UnboundedReceiver<InputEvent>,
    ) {
        let addr = self.addr.clone();
        let pin = self.pin.clone();
        let slot = self.slot.clone();
        let host_size = self.host_size.clone();
        std::thread::Builder::new()
            .name("rdlink-stream".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(stream_loop(addr, pin, slot, host_size, window, input_rx));
            })
            .expect("收流线程创建失败");
    }
}

/// 收流+解码循环（工作线程）。同时跑两个任务：视频收流解码、输入事件发送。
async fn stream_loop(
    addr: String,
    pin: String,
    slot: FrameSlot,
    host_size: HostSize,
    window: Arc<Window>,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
) {
    let t0 = Instant::now();
    let session = match connect(
        addr.parse().expect("地址格式: ip:port"),
        &pin,
        "rdlink-client",
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("连接失败: {e}");
            return;
        }
    };
    println!(
        "已连接 host: {}（握手 {:?}）",
        session.peer_name,
        t0.elapsed()
    );

    // 解构会话：input 流交给发送任务，video 留在本循环
    let rdlink_transport::ClientSession {
        peer_name: _,
        video_info,
        mut control_send,
        control_recv: _,
        mut video,
        mut input,
    } = session;

    // 记录 host 分辨率（输入坐标映射用）
    if let ControlMsg::VideoStreamInfo { width, height, .. } = &video_info {
        println!("视频参数: {width}x{height}");
        *host_size.lock().unwrap() = (*width, *height);
    }

    // 输入发送任务：winit 主线程 → input_rx → Input 流
    let input_task = tokio::spawn(async move {
        let mut sent = 0u64;
        while let Some(ev) = input_rx.recv().await {
            if write_frame(&mut input, &Message::Input(ev)).await.is_err() {
                break; // host 断开
            }
            sent += 1;
        }
        sent
    });

    let mut decoder = match StreamDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("解码器初始化失败: {e}");
            return;
        }
    };
    let mut got_idr = false;
    let mut skipped_before_idr = 0u64;
    let mut decode_buf: Vec<u8> = Vec::new();

    while let Some(msg) = read_frame(&mut video).await.transpose() {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                eprintln!("video 流错误: {e}");
                break;
            }
        };
        let Message::VideoFrame(f) = msg else { continue };

        // D4：IDR 起播
        if !got_idr {
            if !f.key {
                skipped_before_idr += 1;
                continue;
            }
            got_idr = true;
            println!("首 IDR 到达（丢弃 {skipped_before_idr} 个非关键包，连接→此刻 {:?}）", t0.elapsed());
        }

        if let Err(e) = decoder.decode(&f.data) {
            eprintln!("解码错误: {e}");
            break;
        }
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
    }

    // 干净退出：通知 host
    let _ = write_frame(
        &mut control_send,
        &Message::Control(ControlMsg::Bye { reason: "client 退出".into() }),
    )
    .await;
    println!("video 流结束（共解码 {} 帧，输入事件已发 {} 条）", decoder.frames, input_task.await.unwrap_or(0));
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
        let mut display = Display::new(window.clone(), false);

        println!("渲染就绪，连接 {} …（窗口模式 | F11 切全屏 | Esc 退出）", self.addr);

        self.connect_started = Some(Instant::now());
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        self.input_tx = Some(input_tx);
        self.spawn_stream_thread(window.clone(), input_rx);
        display.render();
        self.display = Some(display);
        self.window = Some(window);
        self.window.as_ref().unwrap().request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(window) = self.window.clone() else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

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
                        Key::Named(NamedKey::Escape) => event_loop.exit(),
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
                    window.set_title(&format!(
                        "rdlink client | {} fps | 累计 {} 帧",
                        self.title_frames, self.rendered
                    ));
                    self.title_frames = 0;
                    self.last_title = Instant::now();
                }
                // 无新帧不主动重绘（等待 request_redraw），降低空转 CPU
            }

            _ => {}
        }
    }
}
