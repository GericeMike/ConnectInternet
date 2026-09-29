//! 输入注入链路探针（T7/T8 联调）—— 连上 host 的 Input 流并发送一批注入事件。
//! 用法：`cargo run --release -p rdlink-transport --example input_probe -- <host_ip> <指纹>`
//! 在 host 机器上观察光标是否移动/按键是否生效（配合 GetCursorPos 可程序化验证）。

use std::time::Duration;

use rdlink_proto::{ControlMsg, InputEvent, Message};
use rdlink_transport::{connect, write_frame};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let ip = args.next().unwrap_or_else(|| "127.0.0.1".into());
    let pin = args.next().unwrap_or_else(|| {
        eprintln!("用法: input_probe <host_ip> <证书指纹>");
        std::process::exit(2);
    });

    let addr = format!("{ip}:9527").parse().unwrap();
    let mut session = connect(addr, &pin, "input-probe").await.expect("连接失败");
    println!("已连接 host: {}", session.peer_name);
    println!("3 秒后开始注入（把鼠标从当前焦点窗口移开可避免误触）…");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 光标画"口"字路线（远离屏幕边缘）
    let events: Vec<InputEvent> = vec![
        InputEvent::MouseMove { x: 600, y: 400 },
        InputEvent::MouseMove { x: 1200, y: 400 },
        InputEvent::MouseMove { x: 1200, y: 800 },
        InputEvent::MouseMove { x: 600, y: 800 },
        InputEvent::MouseMove { x: 600, y: 400 },
    ];
    for e in &events {
        write_frame(&mut session.input, &Message::Input(e.clone())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    println!("已发送 {} 个 MouseMove(光标应回到 600,400)", events.len());

    // Shift 按下→抬起(host 侧 GetAsyncKeyState 可验证)
    write_frame(&mut session.input, &Message::Input(InputEvent::Key { vk: 0x10, down: true })).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    write_frame(&mut session.input, &Message::Input(InputEvent::Key { vk: 0x10, down: false })).await.unwrap();

    // 优雅断开(host 会打印会话统计)
    write_frame(&mut session.control_send, &Message::Control(ControlMsg::Bye { reason: "input-probe 完成".into() }))
        .await
        .unwrap();
    println!("完成,已断开");
}
