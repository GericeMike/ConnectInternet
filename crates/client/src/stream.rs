//! T7：client 正常运行模式——连接 host → 收流解码（工作线程）→ 帧槽 → winit/wgpu 渲染。
//!
//! 结构：
//! ```text
//! 工作线程: QUIC 收包 → IDR 起播 → 软解 → NV12/YUV→BGRA → 存入帧槽 → request_redraw
//! 主线程  : winit 事件循环，RedrawRequested 取最新帧 → Display 提交渲染（最新帧优先，丢旧帧）
//! ```

use std::sync::{Arc, Mutex};
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use rdlink_proto::{ControlMsg, Message};
use rdlink_transport::{connect, read_frame, write_frame};

use crate::decoder::StreamDecoder;
use crate::display::Display;

/// 生产者-渲染共享帧槽（最新帧优先：生产者覆盖，渲染者取走）。
type FrameSlot = Arc<Mutex<Option<FrameBuf>>>;

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
            fullscreen: true,
            rendered: 0,
            last_title: Instant::now(),
            title_frames: 0,
            first_frame_ms: None,
            connect_started: None,
        }
    }

    fn spawn_stream_thread(&self, window: Arc<Window>) {
        let addr = self.addr.clone();
        let pin = self.pin.clone();
        let slot = self.slot.clone();
        std::thread::Builder::new()
            .name("rdlink-stream".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(stream_loop(addr, pin, slot, window));
            })
            .expect("收流线程创建失败");
    }
}

/// 收流+解码循环（工作线程）。
async fn stream_loop(
    addr: String,
    pin: String,
    slot: FrameSlot,
    window: Arc<Window>,
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
    if let ControlMsg::VideoStreamInfo { width, height, .. } = &session.video_info {
        println!("视频参数: {width}x{height}");
    }

    let mut session = session;
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

    while let Some(msg) = read_frame(&mut session.video).await.transpose() {
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
        &mut session.control_send,
        &Message::Control(ControlMsg::Bye { reason: "client 退出".into() }),
    )
    .await;
    println!("video 流结束（共解码 {} 帧）", decoder.frames);
}

impl ApplicationHandler for StreamApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("rdlink client（连接中…）")
            .with_fullscreen(Some(Fullscreen::Borderless(None)));
        let window = Arc::new(event_loop.create_window(attrs).expect("窗口创建失败"));
        let mut display = Display::new(window.clone(), false);

        println!("渲染就绪，连接 {} …（Esc 退出 / F11 切全屏）", self.addr);
        println!("连接后全屏显示 host 画面 —— 本机自闭环时会看到无限镜像（预期效果）");

        self.connect_started = Some(Instant::now());
        self.spawn_stream_thread(window.clone());
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

            WindowEvent::KeyboardInput { event, .. } => {
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
                        }
                        _ => {}
                    }
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
