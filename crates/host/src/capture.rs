//! T3/T4:WGC 屏幕捕获(windows-capture crate)+ 编码 demo。
//! M1 管线:D3D11 纹理 → staging 拷贝 → Map → 系统 BGRA 内存 → (T4)编码器 → out.h264。
//! 帧率特性:WGC 脏区驱动,静止桌面不产帧(M1 接受,client 保持最后一帧)。

use std::io::Write;
use std::time::{Duration, Instant};

use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::encoder::ImageFormat;
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::encoder;

const DEMO_DURATION: Duration = Duration::from_secs(30);
const ENCODE_DURATION: Duration = Duration::from_secs(15);
const PNG_COUNT: u32 = 5;

#[derive(Clone, Copy)]
enum DemoMode {
    /// T3:仅统计帧率/尺寸,存 PNG
    Stats,
    /// T4:全管线抓帧→编码→落盘 out.h264
    Encode,
}

struct CaptureDemo {
    mode: DemoMode,
    start: Instant,
    deadline: Duration,
    per_second: Vec<u32>,
    dims: (u32, u32),
    buf_bytes: usize,
    last_reported_sec: usize,
    // T3:PNG 快照
    png_saved: u32,
    last_png_at: Option<Duration>,
    // T4:编码(SendEncoder:ffmpeg 类型无 Send,见 encoder.rs 说明)
    encoder: Option<encoder::SendEncoder>,
    encode_out: Option<std::fs::File>,
    encode_durations: Vec<Duration>,
    packets: usize,
    keyframes: usize,
    encoded_bytes: u64,
}

impl GraphicsCaptureApiHandler for CaptureDemo {
    type Flags = (DemoMode, u32, u32); // (模式, 宽, 高)
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let (mode, width, height) = ctx.flags;
        let (deadline, encoder, encode_out) = match mode {
            DemoMode::Stats => (DEMO_DURATION, None, None),
            DemoMode::Encode => {
                let (enc, tier) = encoder::open_auto(width, height)?;
                let tier_label = match tier {
                    encoder::EncoderTier::Nvenc => "(独显硬编)",
                    encoder::EncoderTier::Qsv => "(核显硬编)",
                    encoder::EncoderTier::X264 => "(软编兜底!)",
                };
                println!(
                    "[demo] 编码器: {}{tier_label} @ {width}x{height},CBR {}Mbps gop {}",
                    enc.name(),
                    encoder::BITRATE / 1_000_000,
                    encoder::GOP,
                );
                let out = std::fs::File::create("capture/out.h264")?;
                (ENCODE_DURATION, Some(encoder::SendEncoder(enc)), Some(out))
            }
        };
        Ok(Self {
            mode,
            start: Instant::now(),
            deadline,
            per_second: Vec::new(),
            dims: (0, 0),
            buf_bytes: 0,
            last_reported_sec: 0,
            png_saved: 0,
            last_png_at: None,
            encoder,
            encode_out,
            encode_durations: Vec::new(),
            packets: 0,
            keyframes: 0,
            encoded_bytes: 0,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let t = self.start.elapsed();
        let sec = t.as_secs() as usize;
        if sec >= self.per_second.len() {
            self.per_second.resize(sec + 1, 0);
        }
        self.per_second[sec] += 1;
        self.dims = (frame.width(), frame.height());
        let pts_us = t.as_micros() as i64;

        // M1 管线:GPU → staging → Map → CPU BGRA(紧密排列;pitch 有 padding 时才发生拷贝)
        if let (Some(w), Some(out)) = (&mut self.encoder, &mut self.encode_out) {
            let mut scratch = Vec::new();
            let buf = frame.buffer()?;
            self.buf_bytes = buf.row_pitch() as usize * buf.height() as usize;
            let bgra = buf.as_nopadding_buffer(&mut scratch);
            let t0 = Instant::now();
            let packets = w.0.encode(bgra, self.dims.0 as usize * 4, pts_us)?;
            self.encode_durations.push(t0.elapsed());
            for p in packets {
                self.encoded_bytes += p.data.len() as u64;
                self.packets += 1;
                if p.key {
                    self.keyframes += 1;
                }
                out.write_all(&p.data)?;
            }
        } else {
            {
                let buf = frame.buffer()?;
                self.buf_bytes = buf.row_pitch() as usize * buf.height() as usize;
            }
            // T3 stats 模式:PNG 快照(首帧立即存,之后按时间均分)
            let due = match self.last_png_at {
                None => true,
                Some(last) => t - last >= DEMO_DURATION / PNG_COUNT,
            };
            if due && self.png_saved < PNG_COUNT {
                let path = format!("capture/frame-{}.png", self.png_saved + 1);
                frame.save_as_image(&path, ImageFormat::Png)?;
                println!("[demo] 已存 {path} ({}x{})", self.dims.0, self.dims.1);
                self.png_saved += 1;
                self.last_png_at = Some(t);
            }
        }

        if sec > self.last_reported_sec {
            let total: u32 = self.per_second.iter().sum();
            let extra = match self.mode {
                DemoMode::Stats => String::new(),
                DemoMode::Encode => format!(" 编码 {} 帧", self.encode_durations.len()),
            };
            println!("[demo] t={sec:>2}s 累计 {total} 帧{extra}");
            self.last_reported_sec = sec;
        }

        if t >= self.deadline {
            self.finish_encode()?;
            self.report();
            capture_control.stop();
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("[demo] 捕获会话关闭");
        Ok(())
    }
}

fn percentile(durations: &[Duration], pct: f64) -> Duration {
    if durations.is_empty() {
        return Duration::ZERO;
    }
    let mut v: Vec<u128> = durations.iter().map(|d| d.as_micros()).collect();
    v.sort_unstable();
    let idx = ((v.len() as f64 - 1.0) * pct).round() as usize;
    Duration::from_micros(v[idx.min(v.len() - 1)] as u64)
}

impl CaptureDemo {
    fn report(&self) {
        let total: u32 = self.per_second.iter().sum();
        let secs = self.per_second.len().max(1) as f64;
        let (w, h) = self.dims;
        println!("──────── demo 结果 ────────");
        println!("帧尺寸   : {w}x{h} BGRA ({} 字节/帧, {:.1} MiB)", self.buf_bytes, self.buf_bytes as f64 / 1048576.0);
        println!("捕获帧数 : {total} / {}s (平均 {:.1} fps)", self.deadline.as_secs(), total as f64 / secs);

        if let Some(w) = &self.encoder {
            let enc = &w.0;
            println!("编码器   : {} ({})", enc.name(), if enc.is_hardware() { "硬编" } else { "软编兜底" });
            println!("编码耗时 : p50={} p95={} max={} (验收线 p95<10ms)",
                percentile(&self.encode_durations, 0.50).as_micros(),
                percentile(&self.encode_durations, 0.95).as_micros(),
                percentile(&self.encode_durations, 1.00).as_micros());
            let bitrate_mbps = self.encoded_bytes as f64 * 8.0 / self.deadline.as_secs_f64() / 1e6;
            println!("输出     : {} 包 / {} 关键帧 / {:.1} MiB → capture/out.h264 (实际码率 {:.1}Mbps)",
                self.packets, self.keyframes, self.encoded_bytes as f64 / 1048576.0, bitrate_mbps);
        } else {
            println!("PNG 快照 : {}/{} 张在 capture/ 目录", self.png_saved, PNG_COUNT);
        }
        println!("逐秒帧数 : {:?}", self.per_second);
        println!("提示     : 逐秒序列里 0 或个位数 = 该秒桌面静止(脏区驱动特性)");
    }

    /// 冲刷编码器,把残留包写完(T4:demo 结束时调用)
    fn finish_encode(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let (Some(w), Some(out)) = (&mut self.encoder, &mut self.encode_out) {
            for p in w.0.flush()? {
                self.encoded_bytes += p.data.len() as u64;
                self.packets += 1;
                if p.key {
                    self.keyframes += 1;
                }
                out.write_all(&p.data)?;
            }
            out.flush()?;
        }
        Ok(())
    }
}

fn build_settings(monitor: Monitor, flags: (DemoMode, u32, u32)) -> Settings<(DemoMode, u32, u32), Monitor> {
    Settings::new(
        monitor,
        CursorCaptureSettings::Default, // 光标画进帧(被控端回显)
        // 黄边无法去除:IsBorderRequired 需 build 20348+,消费版 Win10(19045)没有 → 实测 Default 也无黄边
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8, // NVENC bgr0 直喂格式
        flags,
    )
}

/// `host --capture-demo`(T3):抓主显示器 30 秒,打印实际 fps 与帧尺寸,存 5 张 PNG。
/// 完全静止的桌面不会产帧——测试时动一下鼠标。
pub fn capture_demo() {
    std::fs::create_dir_all("capture").expect("创建 capture/ 目录失败");

    let monitor = Monitor::primary().expect("找不到主显示器");
    println!(
        "[demo] 主显示器: {} ({}x{})",
        monitor.name().unwrap_or_default(),
        monitor.width().unwrap_or(0),
        monitor.height().unwrap_or(0)
    );
    println!("[demo] 开始抓帧 {} 秒(动一下鼠标可产生帧)…", DEMO_DURATION.as_secs());

    let w = monitor.width().unwrap_or(0);
    let h = monitor.height().unwrap_or(0);
    let settings = build_settings(monitor, (DemoMode::Stats, w, h));
    CaptureDemo::start(settings).expect("WGC 捕获失败");
}

/// `host --encode-demo`(T4):抓帧 15 秒 → 编码(NUENC 优先,x264 兜底)→ 落盘 capture/out.h264。
pub fn encode_demo() {
    std::fs::create_dir_all("capture").expect("创建 capture/ 目录失败");

    let monitor = Monitor::primary().expect("找不到主显示器");
    let w = monitor.width().unwrap_or(0);
    let h = monitor.height().unwrap_or(0);
    println!("[demo] 主显示器: {} ({}x{})", monitor.name().unwrap_or_default(), w, h);
    println!("[demo] 抓帧+编码 {} 秒,输出 capture/out.h264 …", ENCODE_DURATION.as_secs());

    let settings = build_settings(monitor, (DemoMode::Encode, w, h));
    // start() 返回后 handler 已被消费,统计在 on_frame_arrived 的 report() 打印
    CaptureDemo::start(settings).expect("WGC 捕获失败");
}

/// `host --list-monitors`（M4 monitor_index 配置的基础）：列出所有显示器。
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
