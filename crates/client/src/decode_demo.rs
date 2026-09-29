//! T5：client 解码 demo（`client --decode-demo <输入文件>`）。
//!
//! 验收：h264_cuvid（NVDEC）硬解优先、失败退软解；IDR 起播（决策 D4）；
//! NV12→BGRA swscale；前 3 帧落盘 PNG；解码耗时统计 p50/p95。

use std::path::Path;
use std::time::Instant;

use ffmpeg_the_third as ffmpeg;
use ffmpeg_the_third::codec;
use ffmpeg_the_third::format;
use ffmpeg_the_third::frame;
use ffmpeg_the_third::media;
use ffmpeg_the_third::software::scaling;

const CUVID: &str = "h264_cuvid";

pub fn run(input: &str) {
    ffmpeg::init().expect("ffmpeg::init 失败");

    let mut ictx = format::input(input).unwrap_or_else(|e| panic!("打开输入失败: {e}"));

    // 借用 ictx 期间把所有解码器建好（Context::from_parameters 会拷贝参数，之后不再持有借用）
    let (vindex, cuvid_opened, sw_opened) = {
        let stream = ictx
            .streams()
            .find(|s| s.parameters().medium() == media::Type::Video)
            .expect("输入中无视频流");
        println!(
            "输入: {}x{}, 目标解码器: {CUVID}（失败退软解）",
            stream.parameters().width(),
            stream.parameters().height(),
        );
        let cuvid = ffmpeg::decoder::find_by_name(CUVID).and_then(|c| {
            codec::Context::from_parameters(stream.parameters())
                .ok()
                .and_then(|ctx| ctx.decoder().open_as(c).ok().and_then(|o| o.video().ok()))
        });
        let sw = codec::Context::from_parameters(stream.parameters())
            .ok()
            .and_then(|ctx| ctx.decoder().video().ok());
        (stream.index(), cuvid, sw)
    };

    let (dec_name, mut opened) = match cuvid_opened {
        Some(o) => (CUVID.to_string(), o),
        None => {
            eprintln!("!! {CUVID} 不可用（无 NVDEC 或打开失败），退回软解");
            ("h264(软解)".into(), sw_opened.expect("软解也失败"))
        }
    };
    println!("解码器: {dec_name}");

    let mut scaler: Option<scaling::context::Context> = None;
    let mut decoded = frame::Video::empty();
    let mut bgra = frame::Video::empty();
    let mut saved = 0usize;
    let mut frames = 0u64;
    let mut dropped_before_idr = 0u64;
    let mut got_idr = false;
    let mut latencies: Vec<u64> = Vec::new();
    let total_start = Instant::now();

    for item in ictx.packets() {
        let (_, packet) = item.expect("读包失败");
        if packet.stream() != vindex {
            continue;
        }
        // D4：IDR 起播——首个关键帧前丢弃，防花屏
        if !got_idr {
            if !packet.flags().contains(codec::packet::Flags::KEY) {
                dropped_before_idr += 1;
                continue;
            }
            got_idr = true;
            println!("收到首个 IDR（此前丢弃 {dropped_before_idr} 个非关键包）");
        }

        let t0 = Instant::now();
        opened.send_packet(&packet).expect("send_packet");
        loop {
            match opened.receive_frame(&mut decoded) {
                Ok(()) => {
                    frames += 1;
                    if scaler.is_none() {
                        // cuvid 输出 NV12（软解 yuv420p），统一转 BGRA
                        scaler = Some(
                            scaling::context::Context::get(
                                decoded.format(),
                                decoded.width(),
                                decoded.height(),
                                format::Pixel::BGRA,
                                decoded.width(),
                                decoded.height(),
                                scaling::Flags::BILINEAR,
                            )
                            .expect("创建 swscale 失败"),
                        );
                        println!(
                            "解码输出格式: {:?}（{}x{}）",
                            decoded.format(),
                            decoded.width(),
                            decoded.height()
                        );
                    }
                    scaler.as_mut().unwrap().run(&decoded, &mut bgra).expect("swscale");
                    if saved < 3 {
                        let path = format!("decoded-{}.png", saved + 1);
                        save_bgra_png(bgra.data(0), bgra.width(), bgra.height(), Path::new(&path));
                        println!("已保存 {path}");
                        saved += 1;
                    }
                }
                // EAGAIN：本包的帧已取尽
                Err(ffmpeg::Error::Other { errno: 11 }) => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => panic!("解码错误: {e}"),
            }
        }
        latencies.push(t0.elapsed().as_micros() as u64);
    }

    latencies.sort_unstable();
    let p = |q: usize| latencies.get(latencies.len() * q / 100).copied().unwrap_or(0);
    println!(
        "\n===== T5 结果（{dec_name}）=====\n帧 {} | 包 {} | 解码耗时 p50 {}µs / p95 {}µs / max {}µs | 总耗时 {:.2}s\n产物: decoded-*.png（请肉眼检查无花屏/色偏）",
        frames,
        latencies.len(),
        p(50),
        p(95),
        latencies.last().copied().unwrap_or(0),
        total_start.elapsed().as_secs_f32(),
    );
}

fn save_bgra_png(data: &[u8], w: u32, h: u32, path: &Path) {
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for px in data.chunks_exact(4) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]); // BGRA -> RGB
    }
    let img = image::RgbImage::from_raw(w, h, rgb).expect("像素尺寸不匹配");
    img.save(path).expect("PNG 落盘失败");
}
