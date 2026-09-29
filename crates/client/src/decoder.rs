//! 流式视频解码器：裸 Annex-B 包（in-band SPS/PPS，无 extradata）→ BGRA 帧。
//!
//! M1 用软解 h264（从 in-band SPS 自初始化，零配置、绝对可靠；
//! 4060 的 CPU 软解 1080p60 余量充足）。T9/T10 优化时换 cuvid 硬解。

use ffmpeg_the_third as ffmpeg;
use ffmpeg_the_third::codec;
use ffmpeg_the_third::frame;
use ffmpeg_the_third::software::scaling;

/// CRT 的 EAGAIN（Error::Other 存正值 errno）
const POSIX_EAGAIN: i32 = 11;

pub struct StreamDecoder {
    opened: codec::decoder::Video,
    decoded: frame::Video,
    bgra: frame::Video,
    scaler: Option<scaling::context::Context>,
    pub frames: u64,
    /// 最近解码出的帧尺寸
    pub size: (u32, u32),
}

/// 一帧解码结果（紧密排列 BGRA）。
pub struct DecodedFrame<'a> {
    pub bgra: &'a [u8],
    pub width: u32,
    pub height: u32,
}

impl StreamDecoder {
    pub fn new() -> Result<Self, ffmpeg::Error> {
        let h264 = codec::decoder::find_by_name("h264").ok_or(ffmpeg::Error::DecoderNotFound)?;
        // 软解不需要预设宽高——SPS/PPS 在码流内，解码器自行初始化
        let ctx = codec::Context::new();
        let opened = ctx.decoder().open_as(h264)?.video()?;
        Ok(Self {
            opened,
            decoded: frame::Video::empty(),
            bgra: frame::Video::empty(),
            scaler: None,
            frames: 0,
            size: (0, 0),
        })
    }

    /// 喂入一个完整包（一帧），弹出 0..n 帧解码结果（B 帧重排时可能 >1）。
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<(u32, u32)>, ffmpeg::Error> {
        let mut packet = ffmpeg::codec::packet::Packet::copy(data);
        packet.set_pts(Some(self.frames as i64));
        self.opened.send_packet(&packet)?;

        let mut produced = Vec::new();
        loop {
            match self.opened.receive_frame(&mut self.decoded) {
                Ok(()) => {
                    let (w, h) = (self.decoded.width(), self.decoded.height());
                    self.size = (w, h);
                    if self.scaler.is_none() {
                        // 软解输出 yuv420p（nvenc/qsv 的 H.264 主profile），转 BGRA
                        self.bgra = frame::Video::new(ffmpeg::format::Pixel::BGRA, w, h);
                        self.scaler = Some(scaling::context::Context::get(
                            self.decoded.format(),
                            w,
                            h,
                            ffmpeg::format::Pixel::BGRA,
                            w,
                            h,
                            scaling::Flags::BILINEAR,
                        )?);
                    }
                    self.scaler.as_mut().unwrap().run(&self.decoded, &mut self.bgra)?;
                    produced.push((w, h));
                    self.frames += 1;
                }
                Err(ffmpeg::Error::Other { errno }) if errno == POSIX_EAGAIN => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(produced)
    }

    /// 取最近一帧的 BGRA 数据（decode 后、下一次 decode 前有效）。
    pub fn last_bgra(&self) -> DecodedFrame<'_> {
        DecodedFrame {
            bgra: self.bgra.data(0),
            width: self.bgra.width(),
            height: self.bgra.height(),
        }
    }
}
