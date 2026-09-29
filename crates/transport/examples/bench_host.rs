//! 两台真机的传输 benchmark —— host 侧。
//! 用法：`cargo run -p rdlink-transport --example bench_host`
//! 打印本机指纹后等待连接（默认端口 9527）。

use std::path::Path;
use std::time::Instant;

use rdlink_proto::{ControlMsg, Message, VideoFrame};
use rdlink_transport::{read_frame, write_frame, HostListener};

const FRAMES: usize = 500;
const FRAME_BYTES: usize = 120 * 1024; // 模拟高码率视频帧
const PINGS: usize = 100;

#[tokio::main]
async fn main() {
    let listener = HostListener::listen("0.0.0.0:9527".parse().unwrap(), Path::new("certs-bench"))
        .expect("监听失败（端口被占？）");
    println!("监听 {}", listener.local_addr().unwrap());
    println!("证书指纹（填给 bench_client）: {}", listener.fingerprint);

    let info = ControlMsg::VideoStreamInfo {
        width: 1920,
        height: 1080,
        extradata: Vec::new(),
    };
    let mut session = listener.accept(info).await.expect("accept");
    println!("已连接主控端: {}", session.peer_name);

    // RTT：echo ping
    for _ in 0..PINGS {
        match read_frame(&mut session.control_recv).await.unwrap() {
            Some(Message::Control(ControlMsg::Ping { t_us })) => {
                write_frame(
                    &mut session.control_send,
                    &Message::Control(ControlMsg::Pong { t_us }),
                )
                .await
                .unwrap();
            }
            _ => panic!("期望 Ping"),
        }
    }

    // 吞吐：host → client 大帧
    let payload = vec![0x42u8; FRAME_BYTES];
    let t = Instant::now();
    for i in 0..FRAMES {
        write_frame(
            &mut session.video,
            &Message::VideoFrame(VideoFrame {
                capture_pts_us: i as i64,
                key: i % 30 == 0,
                data: payload.clone(),
            }),
        )
        .await
        .unwrap();
    }
    session.video.finish().unwrap();
    let total_mb = FRAMES * FRAME_BYTES / (1024 * 1024);
    println!(
        "host 发送完成: {total_mb} MiB, 耗时 {:.2}s（以 client 确认为准）",
        t.elapsed().as_secs_f32()
    );

    // 等 client 收完的确认，避免提前退出拆连接
    match read_frame(&mut session.control_recv).await {
        Ok(Some(Message::Control(ControlMsg::Bye { reason }))) => println!("client 确认: {reason}"),
        other => eprintln!("未收到确认: {other:?}"),
    }
}
