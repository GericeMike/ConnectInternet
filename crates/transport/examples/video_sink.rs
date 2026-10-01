//! 无头收流端(冒烟测试用):连接 host,只收视频帧并丢弃,统计帧数/字节/码率。
//! 不解码不渲染——把负载降到最低,纯测 host 侧管线(捕获→转换→编码→发送)长稳。
//! 用法:`cargo run --release -p rdlink-transport --example video_sink -- <ip> <指纹> [秒]`

use std::time::{Duration, Instant};

use rdlink_proto::{ControlMsg, Message};
use rdlink_transport::{connect, read_frame, write_frame};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let ip = args.next().unwrap_or_else(|| "127.0.0.1".into());
    let pin = args.next().unwrap_or_else(|| {
        eprintln!("用法: video_sink <host_ip> <证书指纹> [秒]");
        std::process::exit(2);
    });
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);

    let addr = format!("{ip}:9527").parse().unwrap();
    let password = std::env::var("RDLINK_PASSWORD").ok();
    let mut session = connect(addr, &pin, "video-sink", password.as_deref()).await.expect("连接失败");
    println!("已连接 host: {}（收流 {}s）", session.peer_name, secs);

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut frames = 0u64;
    let mut window_frames = 0u64; // 本报告窗口的帧数(窗口 fps 用)
    let mut bytes = 0u64;
    let mut keys = 0u64;
    let mut last_pts: Option<i64> = None;
    let mut max_pts_gap_us: i64 = 0;
    let mut last_report = Instant::now();

    // 心跳(保活,也防止 host 端"3s 无 Ping 回收")
    let mut control = session.control_send;
    tokio::spawn(async move {
        loop {
            if write_frame(&mut control, &Message::Control(ControlMsg::Ping { t_us: 0 })).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    let mut video = session.video;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        // 每 10s 打一行中期摘要
        let timeout = std::cmp::min(deadline - now, Duration::from_secs(1));
        match tokio::time::timeout(timeout, read_frame(&mut video)).await {
            Err(_) => {}
            Ok(Err(e)) => {
                eprintln!("video 流错误: {e}");
                break;
            }
            Ok(Ok(Some(Message::VideoFrame(f)))) => {
                frames += 1;
                window_frames += 1;
                bytes += f.data.len() as u64;
                if f.key {
                    keys += 1;
                }
                if let Some(p) = last_pts {
                    let gap = f.capture_pts_us - p;
                    if gap > max_pts_gap_us {
                        max_pts_gap_us = gap;
                    }
                }
                last_pts = Some(f.capture_pts_us);
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) => {
                println!("host 关闭了视频流");
                break;
            }
        }
        if last_report.elapsed().as_secs() >= 10 {
            println!(
                "[{:.0}s] 累计 {frames} 帧（{keys} 关键帧）{:.1} MiB，窗口 {:.1}fps",
                (secs as f64 - (deadline - Instant::now()).as_secs_f64()).max(0.0),
                bytes as f64 / 1048576.0,
                window_frames as f64 / last_report.elapsed().as_secs_f64()
            );
            window_frames = 0;
            last_report = Instant::now();
        }
    }

    let elapsed = secs as f64 - (deadline - Instant::now()).as_secs_f64();
    println!("──── sink 结果 ────");
    println!("时长 {elapsed:.0}s | {frames} 帧（{keys} 关键帧）| {:.1} MiB | 平均 {:.1}fps",
        bytes as f64 / 1048576.0, frames as f64 / elapsed.max(1.0));
    println!("最大帧间隔(pts gap): {}ms", max_pts_gap_us as f64 / 1000.0);
    // 直接断开即可:host 侧有失联看门狗回收(不写 Bye——control_send 已在心跳任务里)
}
