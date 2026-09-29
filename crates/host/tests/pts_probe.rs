//! pts 传递回归测试:复刻 encode-demo 路径(BGRZ 拷贝 → swscale → YUV 帧),
//! 验证 set_pts 的微秒时间戳原样到达 x264 输出包(T4 曾因 time_base/帧率问题翻车)。

use ffmpeg_the_third as ffmpeg;
use ffmpeg::codec::Context;
use ffmpeg::codec::packet::Packet;
use ffmpeg::dictionary::Dictionary;
use ffmpeg::format::Pixel;
use ffmpeg::frame::Video;

#[test]
fn pts_survives_x264() {
    ffmpeg::init().unwrap();
    let codec = ffmpeg::encoder::find_by_name("libx264").unwrap();
    let mut ctx = Context::new_with_codec(codec).encoder().video().unwrap();
    ctx.set_width(1920);
    ctx.set_height(1080);
    ctx.set_format(Pixel::YUV420P);
    ctx.set_time_base((1, 1_000_000));
    ctx.set_frame_rate(Some((60, 1))); // 码率控制的每帧预算基准(不设则每帧预算仅 50bit → QP 崩 51)
    let mut opts = Dictionary::new();
    opts.set("preset", "ultrafast");
    opts.set("tune", "zerolatency");
    let mut encoder = ctx.open_as_with(codec, opts).unwrap();

    // 完全复刻 demo 路径:BGRZ 帧拷贝 → swscale → YUV 帧
    let mut scaler = ffmpeg::software::scaling::Context::get(
        Pixel::BGRZ, 1920, 1080, Pixel::YUV420P, 1920, 1080,
        ffmpeg::software::scaling::Flags::BILINEAR,
    )
    .unwrap();
    let mut bgra_frame = Video::new(Pixel::BGRZ, 1920, 1080);
    let mut yuv_frame = Video::new(Pixel::YUV420P, 1920, 1080);

    let mut packet = Packet::empty();
    let mut got = Vec::new();
    for i in 0..3 {
        let pts_us = 800_000i64 + i as i64 * 35_714; // demo 特征:非零起点 + ~28fps 间隔
        let val = 40 + i as u8 * 80;
        bgra_frame.data_mut(0).fill(val);
        bgra_frame.set_pts(Some(pts_us));
        scaler.run(&bgra_frame, &mut yuv_frame).unwrap();
        yuv_frame.set_pts(Some(pts_us));
        assert_eq!(yuv_frame.pts(), Some(pts_us), "send 前 pts 就错了");
        encoder.send_frame(&yuv_frame).unwrap();
        while let Ok(()) = encoder.receive_packet(&mut packet) {
            got.push(packet.pts().unwrap_or(-1));
        }
    }
    encoder.send_eof().unwrap();
    while let Ok(()) = encoder.receive_packet(&mut packet) {
        got.push(packet.pts().unwrap_or(-1));
    }
    println!("输入 pts: [800000, 835714, 871428] 输出 pts: {got:?}");
    assert!(
        got.starts_with(&[800_000, 835_714, 871_428]),
        "pts 未原样到达输出: {got:?}"
    );
}
