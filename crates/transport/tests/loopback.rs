//! 回环集成测试：同进程内 host + client 完成 QUIC 握手与三通道数据交换。
//! 两台真机的吞吐 benchmark 见 examples/。

use std::time::Instant;

use rdlink_proto::{ControlMsg, InputEvent, Message, MouseButton, VideoFrame, PROTOCOL_VERSION};
use rdlink_transport::{connect, read_frame, write_frame, HostListener};

fn temp_cert_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rdlink-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test]
async fn handshake_and_exchange_all_three_channels() {
    let cert_dir = temp_cert_dir("main");
    let listener = HostListener::listen("127.0.0.1:0".parse().unwrap(), &cert_dir).unwrap();
    let addr = listener.local_addr().unwrap();
    let pin = listener.fingerprint.clone();

    // host 侧：握手 → 回 Ping → 在 Video 通道发一帧 → 读一条 Input
    let host_task = tokio::spawn(async move {
        let info = ControlMsg::VideoStreamInfo {
            width: 1920,
            height: 1080,
            extradata: vec![1, 2, 3],
        };
        let mut session = listener.accept(info).await.expect("host accept");

        // Control: 收 Ping 回 Pong（带 host 时戳）
        if let Some(Message::Control(ControlMsg::Ping { t_us })) =
            read_frame(&mut session.control_recv).await.unwrap()
        {
            write_frame(
                &mut session.control_send,
                &Message::Control(ControlMsg::Pong {
                    t_us,
                    host_recv_us: t_us + 10,
                    host_send_us: t_us + 15,
                }),
            )
            .await
            .unwrap();
        } else {
            panic!("期望 Ping");
        }

        // Video: 发一帧
        write_frame(
            &mut session.video,
            &Message::VideoFrame(VideoFrame {
                capture_pts_us: 12345,
                key: true,
                data: vec![0x55; 2048],
            }),
        )
        .await
        .unwrap();

        // Input: 读一条
        let input_msg = read_frame(&mut session.input).await.unwrap();
        session.peer_name.clone()
    });

    // client 侧
    let mut client = connect(addr, &pin, "test-client").await.expect("client connect");
    let host_name = client.peer_name.clone();
    assert!(!host_name.is_empty(), "应收到 host 自报名称");

    // Control: Ping/Pong 测 RTT
    let t0 = Instant::now();
    write_frame(
        &mut client.control_send,
        &Message::Control(ControlMsg::Ping { t_us: 0 }),
    )
    .await
    .unwrap();
    match read_frame(&mut client.control_recv).await.unwrap().unwrap() {
        Message::Control(ControlMsg::Pong { .. }) => {}
        other => panic!("期望 Pong，得到 {other:?}"),
    }
    let rtt = t0.elapsed();
    assert!(rtt.as_millis() < 50, "回环 RTT 应 <50ms，实测 {rtt:?}");

    // Video: 收帧
    match read_frame(&mut client.video).await.unwrap().unwrap() {
        Message::VideoFrame(f) => {
            assert!(f.key);
            assert_eq!(f.data.len(), 2048);
        }
        other => panic!("期望 VideoFrame，得到 {other:?}"),
    }

    // Input: 发事件
    write_frame(
        &mut client.input,
        &Message::Input(InputEvent::MouseButton {
            button: MouseButton::Left,
            down: true,
        }),
    )
    .await
    .unwrap();

    let host_seen_name = host_task.await.unwrap();
    assert_eq!(host_seen_name, "test-client");
    assert_eq!(PROTOCOL_VERSION, 2);
}

#[tokio::test]
async fn wrong_pin_rejected() {
    let cert_dir = temp_cert_dir("pin");
    let listener = HostListener::listen("127.0.0.1:0".parse().unwrap(), &cert_dir).unwrap();
    let addr = listener.local_addr().unwrap();

    // 错误指纹：证书验证应失败，连接被拒
    let bad_pin = "0".repeat(64);
    let result = connect(addr, &bad_pin, "intruder").await;
    assert!(result.is_err(), "错误指纹必须握手失败");
}
