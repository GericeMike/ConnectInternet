//! 两台真机的传输 benchmark —— client 侧。
//! 用法：`cargo run -p rdlink-transport --example bench_client -- <host_ip> <指纹>`
//! 先跑 bench_host 拿指纹。

use std::time::Instant;

use rdlink_proto::{ControlMsg, Message};
use rdlink_transport::{connect, read_frame, write_frame};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let ip = args.next().unwrap_or_else(|| "127.0.0.1".into());
    let pin = args.next().unwrap_or_else(|| {
        eprintln!("用法: bench_client <host_ip> <证书指纹>");
        std::process::exit(2);
    });

    let addr = format!("{ip}:9527").parse().unwrap();
    let t0 = Instant::now();
    let mut session = connect(addr, &pin, "bench-client").await.expect("连接失败");
    println!("已连接 host: {}（握手耗时 {:?}）", session.peer_name, t0.elapsed());

    // RTT
    let mut samples = Vec::new();
    for _ in 0..100 {
        let t = Instant::now();
        write_frame(
            &mut session.control_send,
            &Message::Control(ControlMsg::Ping { t_us: 0 }),
        )
        .await
        .unwrap();
        match read_frame(&mut session.control_recv).await.unwrap() {
            Some(Message::Control(ControlMsg::Pong { .. })) => {}
            other => panic!("期望 Pong，得到 {other:?}"),
        }
        samples.push(t.elapsed());
    }
    samples.sort();
    let p50 = samples[samples.len() / 2];
    let p95 = samples[samples.len() * 95 / 100];
    println!("RTT p50 = {:?}  p95 = {:?}", p50, p95);

    // 吞吐：收满 FRAMES 帧直到 EOF
    let t = Instant::now();
    let (mut frames, mut bytes) = (0usize, 0usize);
    while let Some(msg) = read_frame(&mut session.video).await.unwrap() {
        match msg {
            Message::VideoFrame(f) => {
                frames += 1;
                bytes += f.data.len();
            }
            other => panic!("video 通道期望 VideoFrame，得到 {other:?}"),
        }
    }
    let secs = t.elapsed().as_secs_f32();
    let mbps = bytes as f64 * 8.0 / secs as f64 / 1_000_000.0;
    println!(
        "接收 {frames} 帧 / {} MiB，耗时 {secs:.2}s，吞吐 {mbps:.0} Mbps",
        bytes / (1024 * 1024)
    );

    // 通知 host 数据已收完，避免其提前退出拆掉连接
    let _ = write_frame(
        &mut session.control_send,
        &Message::Control(ControlMsg::Bye { reason: "bench 完成".into() }),
    )
    .await;
}
