//! T4:视频编码器封装 —— h264_nvenc 优先,libx264 ultrafast+zerolatency 兜底,同一 trait。
//! 输入:BGRA(可带行 pitch,紧密排列时零额外拷贝)。
//! 输出:Annex-B 起始码格式的完整 NAL,SPS/PPS 在每个 IDR 前 in-band 传输
//! (裸 .h264 可直接播放;T7 的 client 侧配 h264 parser 即可起播,无需单独下发 extradata)。

use ffmpeg_the_third as ffmpeg;
use ffmpeg::codec::encoder::video::Encoder;
use ffmpeg::codec::packet::Packet;
use ffmpeg::dictionary::Dictionary;
use ffmpeg::format::Pixel;
use ffmpeg::frame::Video;
use ffmpeg::software::scaling::{Context as Scaler, Flags as ScaleFlags};

/// M1 默认参数档（T10 起可用环境变量覆盖做参数扫描）：
///   RDLINK_BITRATE_MBPS（默认 50）、RDLINK_GOP（默认 90，缩短关键帧间隔利于丢帧后快速恢复）
/// CBR，无 B 帧。
pub fn bitrate() -> usize {
    std::env::var("RDLINK_BITRATE_MBPS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|mbps| mbps * 1_000_000)
        .unwrap_or(50_000_000)
}
pub fn gop() -> u32 {
    std::env::var("RDLINK_GOP")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(90)
}

/// Windows CRT 的 EAGAIN(Error::Other 存 AVUNERROR 后的正值)
const POSIX_EAGAIN: i32 = 11;

pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub key: bool,
    pub pts_us: i64,
}

/// 视频编码器抽象(T7 视频链路消费)
pub trait VideoEncoder {
    fn name(&self) -> &'static str;
    fn is_hardware(&self) -> bool;
    /// 编码一帧 BGRA(pitch = 行字节数,数据需覆盖 pitch*(h-1)+w*4),返回 0..n 个包
    fn encode(&mut self, bgra: &[u8], pitch: usize, pts_us: i64) -> Result<Vec<EncodedPacket>, ffmpeg::Error>;
    /// 流结束,冲出编码器内残留的包
    fn flush(&mut self) -> Result<Vec<EncodedPacket>, ffmpeg::Error>;
}

/// 打开的编码器档位(兜底顺序 NVENC → QSV → x264)
#[derive(Clone, Copy, PartialEq)]
pub enum EncoderTier {
    Nvenc,
    Qsv,
    X264,
}

/// 打开编码器:NVENC 失败落 Intel QSV 核显硬编,再失败落 x264 软编(均日志高亮)。
pub fn open_auto(width: u32, height: u32) -> Result<(Box<dyn VideoEncoder>, EncoderTier), ffmpeg::Error> {
    match NvencEncoder::open(width, height) {
        Ok(e) => Ok((Box::new(e), EncoderTier::Nvenc)),
        Err(err) => {
            eprintln!("⚠️ h264_nvenc 打开失败({err})→ 尝试 Intel QSV 核显硬编");
            match QsvEncoder::open(width, height) {
                Ok(e) => {
                    eprintln!("✅ 已切换 h264_qsv(核显硬编,无驱动回滚需求)");
                    Ok((Box::new(e), EncoderTier::Qsv))
                }
                Err(err2) => {
                    eprintln!("⚠️⚠️ h264_qsv 也失败({err2})→ 最后兜底 libx264 ultrafast+zerolatency 软编,性能将显著下降 ⚠️⚠️");
                    let e = X264Encoder::open(width, height)?;
                    Ok((Box::new(e), EncoderTier::X264))
                }
            }
        }
    }
}

/// ffmpeg 类型内含裸指针(官方仅 Packet 标了 Send);本包装只做所有权转移、
/// 无跨线程并发访问,windows-capture 要求 handler 可 Send,移动语义下是安全的。
pub struct SendEncoder(pub Box<dyn VideoEncoder>);
unsafe impl Send for SendEncoder {}

/// BGRA 拷入预分配的 Video 帧(逐行,pitch 与 stride 可不同)
fn copy_bgra_into_frame(frame: &mut Video, bgra: &[u8], pitch: usize) {
    let stride = frame.stride(0);
    let height = frame.height() as usize;
    let row = frame.width() as usize * 4;
    let dst = frame.data_mut(0);
    for y in 0..height {
        dst[y * stride..y * stride + row].copy_from_slice(&bgra[y * pitch..y * pitch + row]);
    }
}

/// 循环取包:EAGAIN = 编码器暂时没出包(正常),Eof = 已冲干
fn drain(encoder: &mut Encoder, packet: &mut Packet) -> Result<Vec<EncodedPacket>, ffmpeg::Error> {
    let mut out = Vec::new();
    loop {
        match encoder.receive_packet(packet) {
            Ok(()) => out.push(EncodedPacket {
                data: packet.data().unwrap_or_default().to_vec(),
                key: packet.is_key(),
                pts_us: packet.pts().unwrap_or(0),
            }),
            Err(ffmpeg::Error::Other { errno }) if errno == POSIX_EAGAIN => break,
            Err(ffmpeg::Error::Eof) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

// ---------- NVENC:p1 + ull + delay 0,bgr0 直喂(免 swscale) ----------

struct NvencEncoder {
    encoder: Encoder,
    frame: Video,
    packet: Packet,
}

impl NvencEncoder {
    fn open(width: u32, height: u32) -> Result<Self, ffmpeg::Error> {
        let codec = ffmpeg::encoder::find_by_name("h264_nvenc").ok_or(ffmpeg::Error::EncoderNotFound)?;
        let mut ctx = ffmpeg::codec::Context::new_with_codec(codec).encoder().video()?;
        ctx.set_width(width);
        ctx.set_height(height);
        ctx.set_format(Pixel::BGRZ); // BGR0:NVENC 原生支持,内部转 NV12
        ctx.set_bit_rate(bitrate());
        ctx.set_gop(gop());
        ctx.set_max_b_frames(0);
        ctx.set_time_base((1, 1_000_000)); // pts = 微秒
        // 名义 60fps:码率控制按 1/60s 每帧预算分配(串流标准做法)。
        // 不声明时 x264 用 time_base(1µs)当帧时长 → 每帧预算 50bit → QP 崩到 51 全 skip
        ctx.set_frame_rate(Some((60, 1)));
        let mut opts = Dictionary::new();
        opts.set("preset", "p1"); // 速度最快档,画质靠码率补
        opts.set("tune", "ull"); // ultra low latency:禁前视/B帧缓冲
        opts.set("rc", "cbr");
        opts.set("delay", "0"); // 输入一帧立即出一帧
        // open_as_with 返回 video::Encoder(已打开;Deref 链直达 send_frame/receive_packet)
        let encoder = ctx.open_as_with(codec, opts)?;
        Ok(Self { encoder, frame: Video::new(Pixel::BGRZ, width, height), packet: Packet::empty() })
    }
}

impl VideoEncoder for NvencEncoder {
    fn name(&self) -> &'static str {
        "h264_nvenc"
    }
    fn is_hardware(&self) -> bool {
        true
    }

    fn encode(&mut self, bgra: &[u8], pitch: usize, pts_us: i64) -> Result<Vec<EncodedPacket>, ffmpeg::Error> {
        copy_bgra_into_frame(&mut self.frame, bgra, pitch);
        self.frame.set_pts(Some(pts_us));
        self.encoder.send_frame(&self.frame)?;
        drain(&mut self.encoder, &mut self.packet)
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>, ffmpeg::Error> {
        self.encoder.send_eof()?;
        drain(&mut self.encoder, &mut self.packet)
    }
}

// ---------- Intel QSV:核显硬编(R580 掐 Pascal NVENC 后的主路径),BGRA→NV12 ----------

struct QsvEncoder {
    encoder: Encoder,
    bgra_frame: Video,
    nv12_frame: Video,
    scaler: Scaler,
    packet: Packet,
}

impl QsvEncoder {
    fn open(width: u32, height: u32) -> Result<Self, ffmpeg::Error> {
        let codec = ffmpeg::encoder::find_by_name("h264_qsv").ok_or(ffmpeg::Error::EncoderNotFound)?;
        let mut ctx = ffmpeg::codec::Context::new_with_codec(codec).encoder().video()?;
        ctx.set_width(width);
        ctx.set_height(height);
        ctx.set_format(Pixel::NV12); // QSV 只吃 nv12,swscale 转换
        ctx.set_bit_rate(bitrate());
        ctx.set_gop(gop());
        ctx.set_max_b_frames(0);
        ctx.set_time_base((1, 1_000_000)); // pts = 微秒
        ctx.set_frame_rate(Some((60, 1))); // 码率控制的每帧预算基准(同 x264 坑)
        let mut opts = Dictionary::new();
        opts.set("preset", "veryfast");
        opts.set("async_depth", "1"); // 默认 4,降为 1 换最低延迟
        opts.set("low_power", "1"); // VDENC 低功耗快路径(远程桌面场景设计,延迟最低)
        opts.set("scenario", "1"); // MFX_SCENARIO_DISPLAY_REMOTE(选项是整数枚举,字符串常量名会被当表达式报错)
        let encoder = ctx.open_as_with(codec, opts)?;
        let scaler = Scaler::get(
            Pixel::BGRZ,
            width,
            height,
            Pixel::NV12,
            width,
            height,
            ScaleFlags::FAST_BILINEAR, // 桌面内容足够;比 BILINEAR 省数毫秒(弱 CPU 关键)
        )?;
        Ok(Self {
            encoder,
            bgra_frame: Video::new(Pixel::BGRZ, width, height),
            nv12_frame: Video::new(Pixel::NV12, width, height),
            scaler,
            packet: Packet::empty(),
        })
    }
}

impl VideoEncoder for QsvEncoder {
    fn name(&self) -> &'static str {
        "h264_qsv"
    }
    fn is_hardware(&self) -> bool {
        true
    }

    fn encode(&mut self, bgra: &[u8], pitch: usize, pts_us: i64) -> Result<Vec<EncodedPacket>, ffmpeg::Error> {
        copy_bgra_into_frame(&mut self.bgra_frame, bgra, pitch);
        self.bgra_frame.set_pts(Some(pts_us));
        self.scaler.run(&self.bgra_frame, &mut self.nv12_frame)?;
        self.nv12_frame.set_pts(Some(pts_us));
        self.encoder.send_frame(&self.nv12_frame)?;
        drain(&mut self.encoder, &mut self.packet)
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>, ffmpeg::Error> {
        self.encoder.send_eof()?;
        drain(&mut self.encoder, &mut self.packet)
    }
}

// ---------- x264 兜底:ultrafast + zerolatency,swscale BGRA→YUV420P ----------

struct X264Encoder {
    encoder: Encoder,
    bgra_frame: Video,
    yuv_frame: Video,
    scaler: Scaler,
    packet: Packet,
}

impl X264Encoder {
    fn open(width: u32, height: u32) -> Result<Self, ffmpeg::Error> {
        let codec = ffmpeg::encoder::find_by_name("libx264").ok_or(ffmpeg::Error::EncoderNotFound)?;
        let mut ctx = ffmpeg::codec::Context::new_with_codec(codec).encoder().video()?;
        ctx.set_width(width);
        ctx.set_height(height);
        ctx.set_format(Pixel::YUV420P); // x264 不吃 bgr0,swscale 转换
        ctx.set_bit_rate(bitrate());
        ctx.set_gop(gop());
        ctx.set_max_b_frames(0);
        ctx.set_time_base((1, 1_000_000)); // pts = 微秒
        ctx.set_frame_rate(Some((60, 1))); // 同 NVENC:码率控制的每帧预算基准
        let mut opts = Dictionary::new();
        opts.set("preset", "ultrafast");
        opts.set("tune", "zerolatency");
        let encoder = ctx.open_as_with(codec, opts)?;
        let scaler = Scaler::get(
            Pixel::BGRZ,
            width,
            height,
            Pixel::YUV420P,
            width,
            height,
            ScaleFlags::BILINEAR,
        )?;
        Ok(Self {
            encoder,
            bgra_frame: Video::new(Pixel::BGRZ, width, height),
            yuv_frame: Video::new(Pixel::YUV420P, width, height),
            scaler,
            packet: Packet::empty(),
        })
    }
}

impl VideoEncoder for X264Encoder {
    fn name(&self) -> &'static str {
        "libx264"
    }
    fn is_hardware(&self) -> bool {
        false
    }

    fn encode(&mut self, bgra: &[u8], pitch: usize, pts_us: i64) -> Result<Vec<EncodedPacket>, ffmpeg::Error> {
        copy_bgra_into_frame(&mut self.bgra_frame, bgra, pitch);
        self.bgra_frame.set_pts(Some(pts_us));
        self.scaler.run(&self.bgra_frame, &mut self.yuv_frame)?;
        self.yuv_frame.set_pts(Some(pts_us));
        self.encoder.send_frame(&self.yuv_frame)?;
        drain(&mut self.encoder, &mut self.packet)
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>, ffmpeg::Error> {
        self.encoder.send_eof()?;
        drain(&mut self.encoder, &mut self.packet)
    }
}
