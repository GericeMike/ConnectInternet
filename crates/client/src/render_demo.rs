//! T6：渲染 demo（`client --render-demo [vsync]`）。
//!
//! 不走网络，CPU 生成动态测试图 → Display 管线渲染。验证渲染链路独立于解码/网络。
//! 验收：稳定 60fps（no-vsync 应能更高）；F11 全屏/窗口切换不崩溃；Esc 退出。

use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::display::Display;

const SRC_W: u32 = 1920;
const SRC_H: u32 = 1080;

pub fn run(vsync: bool) {
    let event_loop = EventLoop::new().expect("事件循环创建失败");
    let mut app = RenderDemoApp::new(vsync);
    event_loop.run_app(&mut app).expect("事件循环异常退出");
}

struct RenderDemoApp {
    vsync: bool,
    window: Option<Arc<Window>>,
    display: Option<Display>,
    frame_buf: Vec<u8>,
    frame_no: u64,
    start: Instant,
    last_title: Instant,
    presents: u64,
    fullscreen: bool,
}

impl RenderDemoApp {
    fn new(vsync: bool) -> Self {
        Self {
            vsync,
            window: None,
            display: None,
            frame_buf: vec![0u8; (SRC_W * SRC_H * 4) as usize],
            frame_no: 0,
            start: Instant::now(),
            last_title: Instant::now(),
            presents: 0,
            fullscreen: true,
        }
    }

    /// CPU 测试图：彩条 + 移动方块 + 扫描线 + 帧计数块
    fn draw_pattern(&mut self) {
        let t = self.start.elapsed().as_secs_f32();
        let (w, h) = (SRC_W as usize, SRC_H as usize);
        let buf = &mut self.frame_buf;

        const COLORS: [[u8; 3]; 7] = [
            [192, 192, 192], [192, 192, 0], [0, 192, 192], [0, 192, 0],
            [192, 0, 192], [192, 0, 0], [0, 0, 192],
        ];
        let bar_h = h / 4;
        for y in 0..bar_h {
            let ci = y * 7 / bar_h;
            let [b, g, r] = COLORS[ci.min(6)];
            for x in 0..w {
                let o = (y * w + x) * 4;
                buf[o] = b;
                buf[o + 1] = g;
                buf[o + 2] = r;
                buf[o + 3] = 255;
            }
        }

        // 中部：黑色背景 + 移动方块
        for y in bar_h..h {
            for x in 0..w {
                let o = (y * w + x) * 4;
                buf[o] = 16;
                buf[o + 1] = 16;
                buf[o + 2] = 16;
                buf[o + 3] = 255;
            }
        }
        let sq = 240usize;
        let sq_x = ((t * 600.0) as usize) % (w - sq);
        let sq_y = bar_h + (h - bar_h) / 2 - sq / 2 + ((t * 240.0) as usize) % 200 - 100;
        for y in sq_y.saturating_sub(sq)..sq_y.saturating_add(sq).min(h) {
            for x in sq_x..sq_x + sq {
                let o = (y * w + x) * 4;
                buf[o] = 0;
                buf[o + 1] = 216;
                buf[o + 2] = 216;
                buf[o + 3] = 255;
            }
        }

        // 扫描线
        let scan_x = ((t * 1400.0) as usize) % w;
        for y in bar_h..h {
            for dx in 0..6 {
                let x = (scan_x + dx).min(w - 1);
                let o = (y * w + x) * 4;
                buf[o] = 255;
                buf[o + 1] = 255;
                buf[o + 2] = 255;
            }
        }

        // 底部帧计数块（16 格二进制指示）
        let cell = 32usize;
        let base_y = h - cell * 2;
        for bit in 0..16usize {
            let on = (self.frame_no >> bit) & 1 == 1;
            let x0 = 40 + bit * (cell + 8);
            for y in base_y..base_y + cell {
                for x in x0..x0 + cell {
                    if x < w {
                        let o = (y * w + x) * 4;
                        let v = if on { 255 } else { 40 };
                        buf[o] = v;
                        buf[o + 1] = if on { 140 } else { 40 };
                        buf[o + 2] = v;
                    }
                }
            }
        }
    }
}

impl ApplicationHandler for RenderDemoApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("rdlink render-demo")
            .with_fullscreen(Some(Fullscreen::Borderless(None)));
        let window = Arc::new(event_loop.create_window(attrs).expect("窗口创建失败"));
        let mut display = Display::new(window.clone(), self.vsync);
        println!(
            "渲染 demo：{}x{} 源 → {}x{} 窗口 | present: {} | Esc 退出 / F11 切全屏",
            SRC_W,
            SRC_H,
            window.inner_size().width,
            window.inner_size().height,
            display.present_mode_name(),
        );
        display.render(); // 先渲一帧黑底
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
                window.request_redraw();
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
                self.draw_pattern();
                let Some(d) = self.display.as_mut() else { return };
                d.submit_bgra(SRC_W, SRC_H, &self.frame_buf);
                d.render();
                self.frame_no += 1;
                self.presents += 1;

                if self.last_title.elapsed().as_secs_f32() >= 1.0 {
                    let fps = self.presents;
                    let mode = if self.vsync { "vsync" } else { "no-vsync" };
                    window.set_title(&format!(
                        "rdlink render-demo | {fps} fps | {mode} | 帧 {}",
                        self.frame_no
                    ));
                    println!("[render-demo] {fps} fps ({mode}) | 累计帧 {}", self.frame_no);
                    self.presents = 0;
                    self.last_title = Instant::now();
                }
                window.request_redraw();
            }

            _ => {}
        }
    }
}
