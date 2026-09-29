//! T3：WGC 屏幕捕获 demo（`host --capture-demo`）。
//!
//! 验收：主显示器连续捕获 30 秒，每秒打印实际帧数/分辨率/单帧字节数，
//! 均匀落盘 5 张 PNG；结束时汇总实际 fps。
//!
//! 注意：WGC 是脏区驱动——静止桌面不产帧（帧数不涨是正常现象，动鼠标即出帧）。

use std::time::Instant;

use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::encoder::ImageFormat;
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

const DURATION_SECS: u64 = 30;
const PNG_COUNT: usize = 5;

/// 列出所有显示器（M4 monitor_index 配置的基础）。
pub fn list_monitors() {
    match Monitor::enumerate() {
        Ok(monitors) => {
            println!("{:<4} {:<28} {:<12} {:<10}", "idx", "name", "分辨率", "刷新率");
            for m in monitors {
                println!(
                    "{:<4} {:<28} {:<12} {:<10}",
                    m.index().unwrap_or(usize::MAX),
                    m.name().unwrap_or_else(|_| "?".into()),
                    format!("{}x{}", m.width().unwrap_or(0), m.height().unwrap_or(0)),
                    format!("{}Hz", m.refresh_rate().unwrap_or(0)),
                );
            }
        }
        Err(e) => eprintln!("枚举显示器失败: {e}"),
    }
}

struct CaptureDemo {
    start: Instant,
    saved: usize,
    last_report: Instant,
}

// start() 阻塞式捕获结束后读取统计（handler 无法返回值，走全局原子）
static TOTAL_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LAST_W: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static LAST_H: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static LAST_FRAME_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl GraphicsCaptureApiHandler for CaptureDemo {
    type Flags = ();
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(_ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            start: Instant::now(),
            saved: 0,
            last_report: Instant::now(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        ctrl: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        use std::sync::atomic::Ordering::Relaxed;

        let frames = TOTAL_FRAMES.fetch_add(1, Relaxed) + 1;
        LAST_W.store(frame.width(), Relaxed);
        LAST_H.store(frame.height(), Relaxed);
        // 有效 BGRA 字节（不含行对齐 padding）
        LAST_FRAME_BYTES.store(frame.width() as usize * frame.height() as usize * 4, Relaxed);

        if self.last_report.elapsed().as_secs_f32() >= 1.0 {
            println!(
                "[{:>2}s] 累计帧数 {:>5} | {}x{} | 单帧 BGRA {:.1} MiB",
                self.start.elapsed().as_secs(),
                frames,
                LAST_W.load(Relaxed),
                LAST_H.load(Relaxed),
                LAST_FRAME_BYTES.load(Relaxed) as f64 / (1024.0 * 1024.0),
            );
            self.last_report = Instant::now();
        }

        // 按时间均匀存 PNG（不能按帧数：静止桌面不产帧）
        let next_at = (self.saved + 1) * (DURATION_SECS as usize / PNG_COUNT);
        if self.saved < PNG_COUNT && self.start.elapsed().as_secs() as usize >= next_at {
            let path = format!("capture-{}.png", self.saved + 1);
            frame.save_as_image(&path, ImageFormat::Png)?;
            println!("      -> 已保存 {path}");
            self.saved += 1;
        }

        if self.start.elapsed().as_secs() >= DURATION_SECS {
            ctrl.stop();
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// 主显示器捕获 30 秒 demo。
pub fn run() {
    use std::sync::atomic::Ordering::Relaxed;

    let monitor = Monitor::primary().expect("获取主显示器失败");
    println!(
        "主显示器: {} ({}x{} @{}Hz)，开始 {} 秒捕获（期间请随机移动鼠标以产生帧）...",
        monitor.name().unwrap_or_else(|_| "?".into()),
        monitor.width().unwrap_or(0),
        monitor.height().unwrap_or(0),
        monitor.refresh_rate().unwrap_or(0),
        DURATION_SECS,
    );

    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        // NVENC 原生吃 bgr0/bgra，无需转换
        ColorFormat::Bgra8,
        (),
    );

    let start = Instant::now();
    CaptureDemo::start(settings).expect("捕获失败");
    let elapsed = start.elapsed().as_secs_f32();

    let frames = TOTAL_FRAMES.load(Relaxed);
    let (w, h) = (LAST_W.load(Relaxed), LAST_H.load(Relaxed));
    println!(
        "\n===== T3 结果 =====\n实际运行 {elapsed:.1}s | 总帧数 {frames} | 平均 {:.1}fps | 分辨率 {w}x{h}\n（WGC 脏区驱动，静止时帧率低属正常；capture-*.png 已落盘，请肉眼检查）",
        frames as f64 / elapsed as f64,
    );
}
