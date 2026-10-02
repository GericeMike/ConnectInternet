//! T7：host 正常运行模式——监听 QUIC → 每会话开捕获线程（WGC+编码）→ Video 流发送。
//!
//! 结构：
//! ```text
//! 捕获线程(WGC msg loop) → encoder → unbounded channel → async 发送任务 → client
//! 主任务：握手后读 Control 通道（Ping/Pong、Bye），会话结束回收全部资源
//! ```

use std::path::Path;
use std::time::Instant;

use rdlink_proto::{ControlMsg, Message, PowerActionKind, VideoFrame};
use rdlink_transport::{read_frame, write_frame, HostListener, HostSession};
use tokio::sync::mpsc;

use crate::encoder::{self, SendEncoder};

/// host 配置（rdlink.toml [host] 节，文件不存在用默认值：端口 9527、证书目录 certs/）
#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct HostConf {
    pub port: Option<u16>,
    pub cert_dir: Option<String>,
    /// GPU 着色器 NV12 转换（T3a）。独显收益巨大；弱核显（如 UHD 620）上可能与
    /// QSV 编码抢同一块 GPU 和共享内存带宽，导致 fps 下降——此时可关掉回退
    /// BGRA 直读 + swscale 旧路径。默认 true。
    pub gpu_convert: Option<bool>,
    /// 文件上传落地目录（M3-2，默认 %USERPROFILE%\Downloads）
    pub download_dir: Option<String>,
    /// M3-5 密码认证：盐 hex（[host] auth_salt）
    pub auth_salt: Option<String>,
    /// M3-5 密码认证：密钥 K hex（[host] auth_key，host --set-password 生成）
    pub auth_key: Option<String>,
    /// M4-T2：默认捕获显示器下标（0 = 主屏；会话中可用主控端 F10 热切）
    pub monitor_index: Option<u32>,
}

pub fn load_conf() -> HostConf {
    let mut conf: HostConf = std::fs::read_to_string("rdlink.toml")
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
        .and_then(|v| v.get("host").cloned())
        .and_then(|h| h.try_into::<HostConf>().ok())
        .unwrap_or_default();
    // 本机覆盖（rdlink.local.toml，git 忽略）：如被控端弱核显关 gpu_convert，
    // 不改入库配置就能按机器调参
    if let Some(lc) = std::fs::read_to_string("rdlink.local.toml")
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
        .and_then(|v| v.get("host").cloned())
        .and_then(|h| h.try_into::<HostConf>().ok())
    {
        if lc.port.is_some() {
            conf.port = lc.port;
        }
        if lc.cert_dir.is_some() {
            conf.cert_dir = lc.cert_dir;
        }
        if lc.gpu_convert.is_some() {
            conf.gpu_convert = lc.gpu_convert;
        }
        if lc.download_dir.is_some() {
            conf.download_dir = lc.download_dir;
        }
        if lc.monitor_index.is_some() {
            conf.monitor_index = lc.monitor_index;
        }
        // M3-5：认证配置成对生效（盐/密钥缺一不可）
        if lc.auth_salt.is_some() && lc.auth_key.is_some() {
            conf.auth_salt = lc.auth_salt;
            conf.auth_key = lc.auth_key;
        }
    }
    conf
}

/// host 进程级时钟原点：VideoFrame.pts 与 Pong 时戳共用同一时钟域（对时前提）
static HOST_EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// M4-T3：目标码率档位（bps）。控制循环按"基础档/传输压档/链路质量升降"算出
/// 写入；捕获线程每帧比对，变化即按新码率重建编码器（≥2s 间隔防抖）。
/// 0 = 未设置（用基础档）。
static TIER_BPS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// host epoch 起的微秒数
fn epoch_us() -> i64 {
    HOST_EPOCH
        .get()
        .map(|t| t.elapsed().as_micros() as i64)
        .unwrap_or(0)
}

/// 剪贴板负载 → 线上消息（文本/图片）
fn payload_to_msg(hash: u64, payload: &crate::clipboard::ClipPayload) -> Message {
    match payload {
        crate::clipboard::ClipPayload::Text(t) => {
            Message::Control(ControlMsg::ClipboardSync { hash, text: t.clone() })
        }
        crate::clipboard::ClipPayload::Image { width, height, png, .. } => {
            Message::Control(ControlMsg::ClipboardImage { hash, width: *width, height: *height, png: png.clone() })
        }
    }
}

/// 进程级编码器缓存（M2-1c）：QSV 会话建立 ~200ms,跨会话按分辨率复用。
/// 复用编码器的新会话首帧必须 force_key() 出 IDR（新客户端没有参考链）。
struct EncCacheEntry {
    w: u32,
    h: u32,
    enc: SendEncoder,
}

/// 捕获线程 → 发送任务的视频通道条目（M4-T2）。
/// Frame = 编码帧；Info = 流尺寸变化（切屏/分辨率迁移），由 video_task 作为
/// ControlMsg::VideoStreamInfo 写入视频流——与帧同流保序，client 先收 Info
/// 再收新尺寸帧，不存在跨流竞态。
enum VideoItem {
    Frame(VideoFrame),
    Info { width: u32, height: u32 },
}
static ENC_CACHE: std::sync::Mutex<Option<EncCacheEntry>> = std::sync::Mutex::new(None);

pub fn run() {
    let _ = HOST_EPOCH.set(Instant::now());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime 创建失败");
    rt.block_on(async_main());
}

async fn async_main() {
    let conf = load_conf();
    let port = conf.port.unwrap_or(9527);
    let cert_dir = conf.cert_dir.unwrap_or_else(|| "certs".into());
    let addr: std::net::SocketAddr = format!("0.0.0.0:{port}").parse().expect("监听地址格式");
    let auth = conf
        .auth_salt
        .zip(conf.auth_key)
        .filter(|(_, k)| !k.is_empty());
    let listener = HostListener::listen(addr, Path::new(&cert_dir), auth)
        .expect("监听失败（端口被占？删除 certs/ 可重新生成证书）");

    println!("rdlink-host 已就绪");
    println!("监听: {}", listener.local_addr().expect("local_addr"));
    println!("证书指纹（填给 client）: {}", listener.fingerprint);

    // 会话串行闸：accept 循环与会话生命周期解耦（前一会话回收期间新连接的握手
    // 不再被阻塞超时），但同时只允许一个会话占用捕获（后来者握完手等待）
    let gate = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    // 抢占信号：新主控端接入时通知当前会话提前回收（不等 idle timeout 的 10s）
    let (preempt_tx, preempt_rx) = tokio::sync::watch::channel(false);
    // M3-1 剪贴板同步：进程级双线程（监听/写入），会话只拿通道
    let clip = crate::clipboard::spawn();

    loop {
        // 每个会话重新解析捕获目标（分辨率/显示器拓扑可能在会话间变化）。
        // M4-T2：默认屏由 [host] monitor_index 指定（缺省主屏）。
        // resolve 内部已含"枚举为空回退主屏"逻辑，再失败即环境不可用。
        let prefer = conf.monitor_index;
        let (_monitor, _rect, mon_info) =
            crate::monitors::resolve_capture_target(prefer).expect("捕获目标解析失败");
        let (w, h) = (mon_info.width, mon_info.height);
        let info = ControlMsg::VideoStreamInfo { width: w, height: h, extradata: Vec::new() };

        let session = match listener.accept(info).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("握手失败: {e}");
                continue;
            }
        };
        println!("主控端已连接: {}（{w}x{h}）{}", session.peer_name, if session.peer_is_tool { "［工具连接］" } else { "" });
        // 通知旧会话让位——仅 viewer 连接（M4-T3.2：CLI 工具不抢占视频会话）
        if !session.peer_is_tool {
            let _ = preempt_tx.send(true);
        }
        let gate = gate.clone();
        let preempt_tx = preempt_tx.clone();
        let preempt = preempt_rx.clone();
        let clip_rx = clip.changes.clone();
        let clip_tx = clip.write_tx.clone();
        let is_tool = session.peer_is_tool;
        tokio::spawn(async move {
            if !is_tool {
                let _permit = gate.lock().await; // 前一会话占用捕获时，本会话在此等待
                let _ = preempt_tx.send(false); // 自己上岗后复位信号（供再下一个会话抢占）
            }
            serve_session_inner(session, preempt, clip_rx, clip_tx).await;
            println!("会话结束，等待下一个主控端…");
        });
    }
}

/// 服务一个会话直到断开或被新主控端抢占。
async fn serve_session_inner(
    session: HostSession,
    mut preempt: tokio::sync::watch::Receiver<bool>,
    mut clip_rx: tokio::sync::watch::Receiver<Option<(u64, crate::clipboard::ClipPayload)>>,
    clip_tx: std::sync::mpsc::Sender<crate::clipboard::ClipPayload>,
) {
    let HostSession {
        peer_name,
        peer_is_tool: is_tool,
        mut control_send,
        mut control_recv,
        video,
        input,
        connection,
    } = session;
    let _ = &peer_name;

    // M3-2：文件传输流服务（随连接生命周期；accept_bi 在连接关闭时自然退出）
    tokio::spawn(crate::filex::serve(connection.clone()));

    // M4-T3.2：CLI 工具连接（--upload/--procs 等）——只走控制流+文件流，
    // 不占捕获、不推视频、不注入输入、不推剪贴板（工具会话推剪贴板会干扰
    // 在场 viewer 的同步语义）。
    if is_tool {
        println!("[session] 工具会话：仅控制流+文件流");
        drop(video);
        drop(input);
        let mut last_ping = tokio::time::Instant::now();
        loop {
            let msg = tokio::select! {
                m = read_frame(&mut control_recv) => m,
                _ = preempt.changed() => {
                    if *preempt.borrow() {
                        // 工具会话也让位于新 viewer（不让的话会挡抢占信号复位）
                        println!("新主控端接入，工具会话让位");
                        break;
                    }
                    continue;
                }
                _ = tokio::time::sleep_until(last_ping + std::time::Duration::from_secs(3)) => {
                    println!("工具会话失联（3s 无 Ping），回收");
                    break;
                }
            };
            match msg {
                Ok(Some(Message::Control(ControlMsg::Ping { t_us }))) => {
                    last_ping = tokio::time::Instant::now();
                    let host_recv_us = epoch_us();
                    let r = write_frame(
                        &mut control_send,
                        &Message::Control(ControlMsg::Pong {
                            t_us,
                            host_recv_us,
                            host_send_us: epoch_us(),
                        }),
                    )
                    .await;
                    if r.is_err() {
                        break;
                    }
                }
                Ok(Some(Message::Control(ControlMsg::Bye { reason }))) => {
                    println!("工具会话主动断开: {reason}");
                    break;
                }
                Ok(Some(Message::Control(ControlMsg::ProcListReq))) => {
                    let entries = crate::procs::list();
                    if write_frame(
                        &mut control_send,
                        &Message::Control(ControlMsg::ProcListReply { entries }),
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
                Ok(Some(Message::Control(ControlMsg::ProcKill { pid }))) => {
                    let (ok, reason) = match crate::procs::kill(pid) {
                        Ok(()) => (true, String::new()),
                        Err(e) => (false, e),
                    };
                    if write_frame(
                        &mut control_send,
                        &Message::Control(ControlMsg::ProcKillResult { pid, ok, reason }),
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => {
                    println!("工具会话 control 错误: {e}");
                    break;
                }
            }
        }
        drop(control_send);
        drop(control_recv);
        return;
    }

    // M3-1/M3-6 存量同步：会话建立即把 host 当前剪贴板（文本或图片）推给 client——
    // 此前复制的内容在无会话期间不会同步，连接时补发。读失败（GameViewer 类
    // 占用）重试几次。
    {
        let mut payload = None;
        for _ in 0..3 {
            if let Some(p) = crate::clipboard::read_clipboard_payload() {
                payload = Some(p);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        if let Some(p) = payload {
            let hash = crate::clipboard::hash_payload(&p);
            let msg = payload_to_msg(hash, &p);
            let _ = write_frame(&mut control_send, &msg).await;
            println!("[clip] 存量剪贴板已推送给新主控端");
        }
    }

    // 捕获线程 → channel → 发送任务。
    // 有界通道（容量 4）+ 捕获侧 try_send：发送跟不上时丢新帧保低延迟（T10 背压兜底）——
    // 视频流不能丢中间帧（破坏参考链），丢"整帧不入队"是流媒体标准做法；
    // gop 已缩到 90，丢帧后 ≤3s 内必有 IDR 恢复。
    let t_session = Instant::now(); // M2-1a：会话启动全程分段计时的原点
    let (tx, mut rx) = mpsc::channel::<VideoItem>(4);

    // 编码器复用（M2-1c）：从进程缓存取，命中（分辨率一致）则省 ~200ms QSV 会话建立，
    // 并强制新会话首帧出 IDR；未命中（首会话/分辨率变了）走捕获线程内现开。
    let prefer = load_conf().monitor_index;
    let (_m, _r, mon_info) =
        crate::monitors::resolve_capture_target(prefer).expect("捕获目标解析失败");
    let (cw, ch) = (mon_info.width, mon_info.height);
    let mut cached_enc: Option<SendEncoder> = None;
    {
        let mut g = ENC_CACHE.lock().expect("编码器缓存锁");
        if let Some(mut e) = g.take() {
            if e.w == cw && e.h == ch {
                e.enc.0.force_key(); // 复用首帧必须 IDR
                println!("[session] 命中编码器缓存（省 QSV/编码器打开耗时）");
                cached_enc = Some(e.enc);
            }
            // 分辨率变了 → 旧编码器直接丢弃，本次现开新的
        }
    }
    // free-threaded 启动：拿到 CaptureControl，会话结束后可从外部主动停止
    // （关键：静止桌面时 WGC 不产帧，捕获线程自己永远发现不了通道关闭）。
    // M4-T2：capture_control 装进 Option——会话中 MonitorSelect 热切屏时要
    // take() 旧的 stop 掉再换新的。
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let backlog = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (init_monitor, _init_rect, init_info) =
        crate::monitors::resolve_capture_target(prefer).expect("捕获目标解析失败");
    let mut capture_control: Option<ServeCaptureControl> = Some(start_capture(
        tx.clone(),
        ready_tx,
        backlog.clone(),
        cached_enc,
        init_monitor,
        init_info,
    ));

    // M4-T2.4：分辨率/拓扑监视。WGC 帧池不随显示模式缩放——用户改分辨率后
    // 若不重建，画面会一直以旧尺寸缩放发送（糊且坐标映射错）。2s 比对活动
    // 显示器矩形，变了通知控制循环会话内重建（与切屏同路径）。
    let (res_tx, mut res_rx) = mpsc::channel::<()>(1);
    let (resmon_stop_tx, mut resmon_stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if crate::monitors::active_size_changed() {
                        let _ = res_tx.try_send(());
                    }
                }
                _ = resmon_stop_rx.changed() => break,
            }
        }
    });

    // 发送/输入任务必须先于首帧 nudge 启动：首帧入队后要立刻有人消费，
    // 否则会在 channel 里干等 nudge 兜底的 300ms（M2-1a 插桩实测白丢 ~290ms）。
    // video_dead：捕获链路失活信号（M2.5 看门狗）——捕获线程/GPU 死掉时 control
    // 心跳照常，只有 video 断粮；不回收的话主控端永远挂着冻结画面。
    let (video_dead_tx, mut video_dead_rx) = tokio::sync::watch::channel(false);
    let backlog2 = backlog.clone();
    let video_task = tokio::spawn(async move {
        let mut video = video;
        let mut sent = 0u64;
        let mut bytes = 0u64;
        let mut lat_sum = 0u64;
        let mut lat_max = 0u64;
        let mut last_report = Instant::now();
        // M2.5 活性看门狗：静止桌面 WGC 不产帧属正常，4s 无帧先 nudge 探活
        // （必能逼出一帧）；连续 3 次探不出（~12s）判捕获/GPU 死亡 → 回收会话
        let mut silent_probes = 0u32;
        loop {
            let frame = match tokio::time::timeout(std::time::Duration::from_secs(4), rx.recv()).await {
                Ok(Some(item)) => {
                    silent_probes = 0;
                    item
                }
                Ok(None) => break, // 通道关闭（会话回收路径）
                Err(_) => {
                    silent_probes += 1;
                    nudge_cursor();
                    if silent_probes >= 3 {
                        eprintln!("⚠️ 视频链路失活（12s 无帧且 nudge 探不出，疑似捕获/GPU 异常）→ 回收会话");
                        let _ = video_dead_tx.send(true);
                        break;
                    }
                    continue;
                }
            };
            // M4-T2：尺寸切换信令与帧同流（保序），client 先收信令再收新尺寸帧
            let frame = match frame {
                VideoItem::Info { width, height } => {
                    if write_frame(
                        &mut video,
                        &Message::Control(ControlMsg::VideoStreamInfo {
                            width,
                            height,
                            extradata: Vec::new(),
                        }),
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                    println!("[video] 流尺寸切换 {width}x{height} 已下发");
                    continue;
                }
                VideoItem::Frame(f) => f,
            };
            let len = frame.data.len() as u64;
            match write_frame(&mut video, &Message::VideoFrame(frame.clone())).await {
                Ok(()) => {
                    sent += 1;
                    bytes += len;
                    backlog2.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    let lat = (epoch_us() - frame.capture_pts_us).max(0) as u64;
                    lat_sum += lat;
                    lat_max = lat_max.max(lat);
                }
                Err(e) => {
                    eprintln!("video 流写入失败（主控端断开?）: {e}");
                    break;
                }
            }
            if last_report.elapsed().as_secs() >= 5 && sent > 0 {
                println!(
                    "[send] {}fps | capture→send 平均 {}ms / 最大 {}ms",
                    sent / last_report.elapsed().as_secs().max(1),
                    lat_sum / sent / 1000,
                    lat_max / 1000,
                );
                sent = 0;
                lat_sum = 0;
                lat_max = 0;
                last_report = Instant::now();
            }
        }
        (sent, bytes)
    });

    // 输入注入任务（T8）：Input 流 → SendInput。
    // SendInput 单次 <1ms,直接在异步任务里调用(M1);UIPI(焦点在提权窗口)时注入被系统丢弃并计数。
    let input_task = tokio::spawn(async move {
        let mut input = input;
        let mut injected = 0u64;
        let mut dropped = 0u64;
        while let Ok(Some(msg)) = read_frame(&mut input).await {
            if let Message::Input(event) = msg {
                match crate::input::inject(&event) {
                    Ok(()) => injected += 1,
                    Err(e) => {
                        dropped += 1;
                        if dropped <= 3 {
                            eprintln!("[input] 注入失败(UIPI?): {e}");
                        }
                    }
                }
                if injected > 0 && injected % 100 == 0 {
                    println!("[input] 已注入 {injected} 个事件");
                }
            }
        }
        (injected, dropped)
    });

    // 首帧加速（T9 议题②）：静止桌面 WGC 不产帧，首个 IDR 要等真实画面变化。
    // 捕获就绪后用 1px 光标微推制造脏区立即逼出一帧；300ms 后再推一次兜住边界竞态。
    // 等待放 spawn_blocking（std channel 的 recv_timeout 是阻塞调用，不能挂在 worker 线程上）。
    let ready = tokio::task::spawn_blocking(move || ready_rx.recv_timeout(std::time::Duration::from_secs(3)))
        .await
        .unwrap_or(Err(std::sync::mpsc::RecvTimeoutError::Timeout));
    if ready.is_ok() {
        println!("[session] 会话启动: 会话开始→捕获就绪 {}ms", t_session.elapsed().as_millis());
        nudge_cursor();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        nudge_cursor();
    }

    // Control 通道：Ping→Pong（带 host 时戳，client 用于对时）/ Bye / 断开 / 新主控端抢占。
    // 抢占时取消 read_frame 半读会损坏 control 帧边界——但该会话即将整体销毁，无碍。
    // 失联看门狗（M2-1c）：client 被强杀时 QUIC 要等 idle timeout(10s) 才报错，期间死会话
    // 僵占捕获、下一个主控端要先等它回收；client 正常每 500ms 一个 Ping，3s 无 Ping 即判失联。
    // M4-T3：码率档位控制。档位梯 = [基础档, 30M, 15M, 8M] 截到 ≤ 基础档；
    // 传输进行中压到 ≤15M（T3.2）；client LinkQuality（2s 一发）滞回升降档
    // （T3.1）：p95>100ms×3 连发降一档（~6s），p95<50ms×15 连发升一档（~30s）。
    // 结果写 TIER_BPS，捕获线程检测变化后原地重建编码器。
    let base_bps = encoder::bitrate();
    TIER_BPS.store(base_bps, std::sync::atomic::Ordering::Relaxed);
    let mut ladder: Vec<usize> =
        [50_000_000usize, 30_000_000, 15_000_000, 8_000_000]
            .into_iter()
            .filter(|&b| b <= base_bps)
            .collect();
    if ladder.is_empty() {
        ladder.push(base_bps);
    }
    let mut loss_lvl = 0usize;
    let mut bad_streak = 0u32;
    let mut good_streak = 0u32;
    let mut tier_tick = tokio::time::interval(std::time::Duration::from_secs(1));

    let mut last_ping = tokio::time::Instant::now();
    loop {
        let msg = tokio::select! {
            m = read_frame(&mut control_recv) => m,
            _ = preempt.changed() => {
                if *preempt.borrow() {
                    println!("新主控端接入，当前会话让位");
                    break;
                }
                continue;
            }
            _ = video_dead_rx.changed() => {
                // 捕获链路死亡（M2.5 看门狗）：心跳还在但画面断了，必须回收，
                // 否则主控端永远挂着冻结帧
                if *video_dead_rx.borrow() {
                    println!("视频链路失活，回收会话");
                    break;
                }
                continue;
            }
            _ = clip_rx.changed() => {
                // M3-1/M3-6：本端剪贴板变化（文本或图片）→ 同步给 client
                let (hash, payload) = match clip_rx.borrow().clone() {
                    Some(x) => x,
                    None => continue,
                };
                let msg = payload_to_msg(hash, &payload);
                if write_frame(&mut control_send, &msg).await.is_err() {
                    break;
                }
                continue;
            }
            _ = res_rx.recv() => {
                // M4-T2.4：分辨率/拓扑变化 → 按设备名重解析并会话内重建捕获
                match crate::monitors::resolve_capture_device(&crate::monitors::active_device())
                {
                    Ok((m, _r, info)) => {
                        println!(
                            "[monitor] 分辨率/拓扑变化 → 重建捕获（{} {}x{}）",
                            info.device_name, info.width, info.height
                        );
                        rebuild_capture(m, info, &mut capture_control, &tx, &backlog).await;
                    }
                    Err(e) => eprintln!("[monitor] 变化后目标解析失败: {e}"),
                }
                continue;
            }
            _ = tier_tick.tick() => {
                // M4-T3：算目标档并写入（捕获线程消费）
                let mut idx = loss_lvl.min(ladder.len() - 1);
                let nactive = crate::filex::active_transfers();
                if nactive > 0 {
                    while idx + 1 < ladder.len() && ladder[idx] > 15_000_000 {
                        idx += 1;
                    }
                }
                let target = ladder[idx];
                if TIER_BPS.load(std::sync::atomic::Ordering::Relaxed) != target {
                    println!(
                        "[tier] 码率档位 → {} Mbps（链路档 #{loss_lvl}，传输 {nactive} 个）",
                        target / 1_000_000
                    );
                    TIER_BPS.store(target, std::sync::atomic::Ordering::Relaxed);
                }
                continue;
            }
            _ = tokio::time::sleep_until(last_ping + std::time::Duration::from_secs(3)) => {
                println!("主控端失联（3s 无 Ping），回收会话");
                break;
            }
        };
        match msg {
            Ok(Some(Message::Control(ControlMsg::Ping { t_us }))) => {
                last_ping = tokio::time::Instant::now();
                let host_recv_us = epoch_us();
                let r = write_frame(
                    &mut control_send,
                    &Message::Control(ControlMsg::Pong {
                        t_us,
                        host_recv_us,
                        host_send_us: epoch_us(),
                    }),
                )
                .await;
                if r.is_err() {
                    break;
                }
            }
            Ok(Some(Message::Control(ControlMsg::Bye { reason }))) => {
                println!("主控端主动断开: {reason}");
                break;
            }
            Ok(Some(Message::Control(ControlMsg::ClipboardSync { hash, text }))) => {
                // M3-1：对端文本 → 交写线程落本机（防回环由 LAST_SYNCED 统一裁决）
                if text.len() <= crate::clipboard::MAX_CLIP_TEXT {
                    let _ = clip_tx.send(crate::clipboard::ClipPayload::Text(text));
                }
            }
            Ok(Some(Message::Control(ControlMsg::ClipboardImage { hash, width, height, png }))) => {
                // M3-6：对端图片 → PNG 解码后交写线程落本机
                if png.len() <= crate::clipboard::MAX_CLIP_PNG {
                    match crate::clipboard::decode_png(&png) {
                        Some((w, h, rgba)) => {
                            let _ = clip_tx.send(crate::clipboard::ClipPayload::Image {
                                width: w,
                                height: h,
                                rgba,
                                png,
                            });
                        }
                        None => eprintln!("[clip] 对端图片 PNG 解码失败"),
                    }
                }
            }
            Ok(Some(Message::Control(ControlMsg::PowerAction { action }))) => {
                // M3-3：电源动作。Shutdown/Restart 执行后连接/机器随之下线，
                // 无需也无处回复——同步执行（毫秒级），不阻塞控制循环。
                let name = match action {
                    PowerActionKind::Lock => "锁屏",
                    PowerActionKind::Sleep => "睡眠",
                    PowerActionKind::Shutdown => "关机",
                    PowerActionKind::Restart => "重启",
                };
                println!("[power] 收到电源动作: {name}，执行…");
                if let Err(e) = crate::power::execute(action) {
                    eprintln!("[power] {name} 执行失败: {e}");
                }
            }
            Ok(Some(Message::Control(ControlMsg::ProcListReq))) => {
                // M3-4：进程列表（内部双采样阻塞 ~300ms）
                let entries = crate::procs::list();
                if write_frame(
                    &mut control_send,
                    &Message::Control(ControlMsg::ProcListReply { entries }),
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            Ok(Some(Message::Control(ControlMsg::ProcKill { pid }))) => {
                // M3-4：结束进程
                let (ok, reason) = match crate::procs::kill(pid) {
                    Ok(()) => (true, String::new()),
                    Err(e) => (false, e),
                };
                println!("[procs] 结束进程 {pid}: {}", if ok { "成功" } else { &reason });
                if write_frame(
                    &mut control_send,
                    &Message::Control(ControlMsg::ProcKillResult { pid, ok, reason }),
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            Ok(Some(Message::Control(ControlMsg::MonitorListReq))) => {
                // M4-T2：显示器枚举（active 按设备名对齐）
                let monitors = crate::monitors::enumerate();
                let active = monitors
                    .iter()
                    .position(|m| m.device_name == crate::monitors::active_device())
                    .map(|i| i as u32)
                    .unwrap_or(0);
                if write_frame(
                    &mut control_send,
                    &Message::Control(ControlMsg::MonitorList { active, monitors }),
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            Ok(Some(Message::Control(ControlMsg::MonitorSelect { index }))) => {
                // M4-T2：会话内热切屏（与分辨率重建共用 rebuild_capture）
                let (m, _rect, ninfo) = match crate::monitors::resolve_capture_target(Some(index)) {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("[monitor] 目标解析失败: {e}");
                        continue;
                    }
                };
                println!(
                    "[monitor] 切换捕获屏 → {}（{}x{}）",
                    ninfo.device_name, ninfo.width, ninfo.height
                );
                rebuild_capture(m, ninfo, &mut capture_control, &tx, &backlog).await;
            }
            Ok(Some(Message::Control(ControlMsg::LinkQuality { e2e_p95_us, recv_fps }))) => {
                // M4-T3.1：链路质量反馈（滞回升降档）。只在真实收流时判定——
                // 静止桌面 WGC 不产帧，低 fps 下的延迟分位数没有链路意义。
                if recv_fps >= 5 {
                    if e2e_p95_us > 100_000 {
                        bad_streak += 1;
                        good_streak = 0;
                        if bad_streak >= 3 && loss_lvl + 1 < ladder.len() {
                            loss_lvl += 1;
                            bad_streak = 0;
                            println!("[tier] 链路质量差（p95 {}ms）→ 降档至 #{}", e2e_p95_us / 1000, loss_lvl);
                        }
                    } else if e2e_p95_us < 50_000 {
                        good_streak += 1;
                        bad_streak = 0;
                        if good_streak >= 15 && loss_lvl > 0 {
                            loss_lvl -= 1;
                            good_streak = 0;
                            println!("[tier] 链路质量良好（p95 {}ms）→ 升档至 #{}", e2e_p95_us / 1000, loss_lvl);
                        }
                    } else {
                        bad_streak = 0;
                        good_streak = 0;
                    }
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                println!("control 通道关闭");
                break;
            }
            Err(e) => {
                println!("control 通道错误: {e}");
                break;
            }
        }
    }

    // 回收：drop 剩余 tx → 主动停捕获（WM_QUIT；静止桌面时线程不会自己发现通道关闭）
    //      → channel 排空 → 发送/注入任务结束。
    // 超时只是防悬挂兜底（正常路径秒退）；对端异常消失时流要等 idle timeout(10s) 才报错，
    // 这里不等它——新会话抢占优先（超时后任务自然结束，permit 已释放）
    drop(tx);
    drop(control_send);
    drop(control_recv);
    let _ = resmon_stop_tx.send(true); // M4-T2.4：停分辨率监视任务
    // 回收并行化（M2-1c）：被强杀的 client 会让 input 流读挂到超时——stop/video/input
    // 三步串行最坏 900ms，并行后封顶 300ms（抢占路径上新主控端少等一半）；
    // 编码器收回在 stop 完成后经 callback() try_lock 直取（尺寸取 handler 实时值
    // ——会话中分辨率可能变过，缓存按最后实际尺寸归档才能命中）。
    let (_r_stop, r_video, r_input) = tokio::join!(
        async {
            if let Some(cc) = capture_control.take() {
                let handler = cc.callback();
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(300),
                    tokio::task::spawn_blocking(move || cc.stop()),
                )
                .await;
                if let Some(mut h) = handler.try_lock() {
                    let (lw, lh) = (h.w, h.h);
                    if let Some(e) = h.enc.take() {
                        *ENC_CACHE.lock().expect("编码器缓存锁") =
                            Some(EncCacheEntry { w: lw, h: lh, enc: e });
                        println!("[session] 编码器已归缓存（{lw}x{lh}）");
                    }
                }
            }
        },
        tokio::time::timeout(std::time::Duration::from_millis(300), video_task),
        tokio::time::timeout(std::time::Duration::from_millis(300), input_task),
    );
    // video/input 任务尽力收统计（对端死连接的阻塞读由超时兜底）
    if let Ok(sent) = r_video {
        let (sent, bytes) = sent.unwrap_or((0, 0));
        println!("共发送 {sent} 帧视频（{:.1} MiB）", bytes as f64 / 1048576.0);
    }
    if let Ok(Ok((injected, dropped))) = r_input {
        if injected > 0 || dropped > 0 {
            println!("共注入 {injected} 个输入事件（{dropped} 个被系统拒绝）");
        }
    }
}

// ---------------------------------------------------------------------------
// 捕获线程（阻塞式 WGC 消息循环）
// ---------------------------------------------------------------------------

use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

/// 捕获线程启动参数（经 Settings Flags 注入）
struct CaptureBoot {
    tx: mpsc::Sender<VideoItem>,
    ready: std::sync::mpsc::Sender<()>,
    backlog: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// 启动时刻（M2-1a 分段计时）
    launch: Instant,
    /// 进程缓存命中的已打开编码器（None = 现开）
    enc: Option<SendEncoder>,
    /// 捕获目标（M4-T2：尺寸/原点/设备名）
    info: rdlink_proto::MonitorInfo,
}

/// 捕获控制句柄类型别名（会话中热切屏要 take/replace，装 Option 用）
type ServeCaptureControl = windows_capture::capture::CaptureControl<
    ServeCapture,
    Box<dyn std::error::Error + Send + Sync>,
>;

/// 向视频通道可靠投递尺寸信令（有界通道满时短暂重试；会话已死则放弃）
fn send_info(tx: &mpsc::Sender<VideoItem>, width: u32, height: u32) {
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    while Instant::now() < deadline {
        match tx.try_send(VideoItem::Info { width, height }) {
            Ok(()) => return,
            Err(mpsc::error::TrySendError::Full(_)) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return,
        }
    }
    eprintln!("[capture] 尺寸信令投递超时（通道拥塞 2s）");
}

struct ServeCapture {
    enc: Option<SendEncoder>,
    tx: mpsc::Sender<VideoItem>,
    /// 当前编码器尺寸（M4-T2.4：帧尺寸变化检测的基准）
    w: u32,
    h: u32,
    /// 当前编码码率（M4-T3：与 TIER_BPS 比对，变了就重建）
    bps: usize,
    /// 上次码率重建时刻（M4-T3 防抖：≥2s 才允许再切）
    last_tier_switch: Instant,
    /// 待发队列深度（发送任务写完一帧减一；背压/T10 监控）
    backlog: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// T3a：GPU VideoProcessor 转换器（首个帧纹理到达时惰性初始化；
    /// None+未死 = 还没初始化；None+死 = 初始化失败永久回退 BGRA 路径）
    gpu: Option<crate::gpu::GpuConverter>,
    gpu_dead: bool,
    start: Instant,
    /// 本统计窗口内的帧数（5s 重置，算窗口 fps）
    window_frames: u64,
    /// 背压丢帧计数（T10）
    dropped: u64,
    encoded: u64,
    /// 本窗口编码耗时样本（µs，报告后清空）
    enc_us: Vec<u64>,
    /// 本窗口 GPU 转换+回读耗时样本（µs，T3a 数据）
    gpu_us: Vec<u64>,
    last_report: Instant,
    scratch: Vec<u8>,
    /// 编码器名称（日志）
    enc_name: &'static str,
    /// 捕获线程启动时刻（start_free_threaded 调用瞬间，M2-1a 首帧分段计时）
    launch: Instant,
}

impl GraphicsCaptureApiHandler for ServeCapture {
    type Flags = CaptureBoot;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let CaptureBoot { tx, ready, backlog, launch, enc, info } = ctx.flags;
        // M2-1a 分段计时：start_free_threaded → new() = WGC 会话激活；
        // open_auto 内部 = NVENC 探测 + QSV/x264 打开（缓存命中时两者皆 0）
        let activation_ms = launch.elapsed().as_millis();
        let (w, h) = (info.width, info.height);
        // M4-T2：登记当前捕获屏矩形（输入映射 MouseMove → 虚拟桌面绝对坐标）
        crate::monitors::set_active(
            &info.device_name,
            crate::monitors::MonRect { x: info.x, y: info.y, w, h },
        );
        // M4-T2.4：捕获建立即下发尺寸信令（先于任何帧；与握手 Info 同值幂等，
        // 切屏/分辨率迁移后为权威更新）
        send_info(&tx, w, h);
        let (enc, enc_open_ms, reused) = match enc {
            Some(e) => (e, 0, true),
            None => {
                let t_enc = Instant::now();
                let bps = TIER_BPS
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .max(1);
                let (e, _tier) = encoder::open_auto_with_bitrate(w, h, bps)?;
                (SendEncoder(e), t_enc.elapsed().as_millis(), false)
            }
        };
        let enc_name = enc.0.name();
        // M4-T3：会话启动时控制循环会写入基础档；这里 0（竞态）则退基础档
        let init_bps = {
            let t = TIER_BPS.load(std::sync::atomic::Ordering::Relaxed);
            if t > 0 { t } else { encoder::bitrate() }
        };
        println!(
            "[capture] 启动分段: WGC 激活 {activation_ms}ms | 编码器打开 {enc_open_ms}ms{}（{enc_name}）@ {w}x{h}（{}）",
            if reused { "（缓存复用）" } else { "" },
            info.device_name,
        );
        // 通知主任务：WGC 会话已建立，可以推首帧了
        let _ = ready.send(());
        Ok(Self {
            enc: Some(enc),
            tx,
            w,
            h,
            bps: init_bps,
            last_tier_switch: Instant::now(),
            backlog,
            gpu: None,
            gpu_dead: !load_conf().gpu_convert.unwrap_or(true),
            start: Instant::now(),
            window_frames: 0,
            dropped: 0,
            encoded: 0,
            enc_us: Vec::new(),
            gpu_us: Vec::new(),
            last_report: Instant::now(),
            scratch: Vec::new(),
            enc_name,
            launch,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        ctrl: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let pts_us = epoch_us();
        let w = frame.width() as usize;

        // M4-T2.4：动态分辨率迁移。帧尺寸变化（用户改分辨率/拔插屏）→ 原地重建
        // 编码器 + GPU 转换器 + 下发新尺寸信令 + 修正输入映射矩形，全程不断流。
        // 旧实现：尺寸不匹配 → 编码失败 → 捕获线程死 → 12s 看门狗回收 → 重连。
        let (fw, fh) = (frame.width() as u32, frame.height() as u32);
        if fw != self.w || fh != self.h {
            println!(
                "[capture] 分辨率变化 {}x{} → {}x{}：原地重建编码器/GPU 转换器（不断流）",
                self.w, self.h, fw, fh
            );
            let t_rebuild = Instant::now();
            let (mut e, _tier) = encoder::open_auto_with_bitrate(fw, fh, self.bps)?;
            e.force_key(); // 尺寸切换后的首帧必须 IDR（新 SPS）
            self.enc = Some(SendEncoder(e));
            self.gpu = None; // 下一帧按新尺寸惰性重建
            self.w = fw;
            self.h = fh;
            send_info(&self.tx, fw, fh);
            crate::monitors::update_active_size(fw, fh);
            println!("[capture] 重建完成 {}ms，新尺寸已下发", t_rebuild.elapsed().as_millis());
        }

        // M4-T3：码率档位热切（控制循环写 TIER_BPS）。变了就按新码率原地重建
        // 编码器（ffmpeg 硬编打开后改 bit_rate 不生效，重建是最可靠路径；
        // ~200ms + IDR，配合 ≥2s 防抖与档位滞回，频率很低）。
        {
            let want = TIER_BPS.load(std::sync::atomic::Ordering::Relaxed);
            if want > 0 && want != self.bps && self.last_tier_switch.elapsed().as_secs() >= 2 {
                println!(
                    "[capture] 码率档位切换 {}→{} Mbps：原地重建编码器",
                    self.bps / 1_000_000,
                    want / 1_000_000,
                );
                let t = Instant::now();
                match encoder::open_auto_with_bitrate(self.w, self.h, want) {
                    Ok((mut e, _)) => {
                        e.force_key();
                        self.enc = Some(SendEncoder(e));
                        self.bps = want;
                        println!("[capture] 档位重建完成 {}ms", t.elapsed().as_millis());
                    }
                    Err(err) => eprintln!("⚠️ [capture] 档位重建失败({err})，维持原档"),
                }
                self.last_tier_switch = Instant::now();
            }
        }

        {
            let Some(enc) = self.enc.as_mut() else {
                ctrl.stop();
                return Ok(());
            };
            // T3a：优先 GPU 转换（VideoProcessor BGRA→NV12 + 3MB 回读，替代 swscale 10ms）。
            // 转换器在首个帧纹理上惰性初始化（设备从纹理 GetDevice，保证与 WGC 同设备）；
            // 初始化或运行失败 → 永久回退 BGRA 直读 + 编码器内 swscale 旧路径。
            let mut t0: Option<Instant> = None;
            let packets = {
                let mut gpu_out: Option<Vec<encoder::EncodedPacket>> = None;
                if !self.gpu_dead {
                    if self.gpu.is_none() {
                        let tex = frame.as_raw_texture();
                        let dev = unsafe { tex.GetDevice() }.ok();
                        let conv_result = match dev {
                            Some(d) => crate::gpu::GpuConverter::new(
                                &d,
                                frame.width() as u32,
                                frame.height() as u32,
                            ),
                            None => Err(windows::core::Error::from_hresult(
                                windows::core::HRESULT(-1),
                            )),
                        };
                        match conv_result {
                            Ok(g) => {
                                println!("[gpu] 着色器 NV12 转换器就绪（GPU 路径生效）");
                                self.gpu = Some(g);
                            }
                            Err(e) => {
                                eprintln!("⚠️ [gpu] GPU 转换器初始化失败({e}) → 回退 BGRA+swscale 旧路径");
                                self.gpu_dead = true;
                            }
                        }
                    }
                    if let Some(gpu) = self.gpu.as_mut() {
                        let tex = frame.as_raw_texture();
                        let tg = Instant::now();
                        match gpu.convert_and_readback(tex) {
                            Ok(nv) => {
                                self.gpu_us.push(tg.elapsed().as_micros() as u64);
                                t0 = Some(Instant::now());
                                gpu_out = Some(enc.0.encode(
                                    encoder::FrameSrc::Nv12 { buf: nv.buf, pitch: nv.pitch },
                                    pts_us,
                                )?);
                            }
                            Err(e) => {
                                eprintln!("⚠️ [gpu] 转换/回读失败({e}) → 永久回退 BGRA+swscale 旧路径");
                                self.gpu = None;
                                self.gpu_dead = true;
                            }
                        }
                    }
                }
                match gpu_out {
                    Some(p) => p,
                    None => {
                        // 旧路径：WGC 直读 BGRA
                        let fb = frame.buffer()?;
                        let bgra = fb.as_nopadding_buffer(&mut self.scratch);
                        t0 = Some(Instant::now());
                        enc.0.encode(encoder::FrameSrc::Bgra { buf: bgra, pitch: w * 4 }, pts_us)?
                    }
                }
            };
            let t0 = t0.expect("编码计时起点必然被赋值");
            let e = t0.elapsed().as_micros() as u64;
            self.enc_us.push(e); // 窗口统计（p50/p95）
            let enc_us = e as u32; // 随帧下发（协议 v2，client 侧分段打点）
            for p in packets {
                self.encoded += 1;
                if self.encoded == 1 {
                    // M2-1a：首帧分段——线程启动→首帧编码完成入队（含 WGC 激活+编码器打开+等脏区）
                    println!(
                        "[capture] 首帧就绪: 线程启动→首帧入队 {}ms（本帧编码 {}ms）",
                        self.launch.elapsed().as_millis(),
                        enc_us / 1000,
                    );
                }
                match self.tx.try_send(VideoItem::Frame(VideoFrame {
                    capture_pts_us: p.pts_us,
                    key: p.key,
                    encode_us: enc_us,
                    data: p.data,
                })) {
                    Ok(()) => {
                        self.backlog.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // 背压兜底（T10）：发送跟不上 → 整帧丢弃（不破坏参考链）
                        self.dropped += 1;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        // 发送端已关闭（会话结束）→ 停止捕获（编码器由会话侧经 callback() 收回）
                        ctrl.stop();
                        return Ok(());
                    }
                }
            }
        }

        self.window_frames += 1;
        if self.last_report.elapsed().as_secs_f32() >= 5.0 {
            let window_s = self.last_report.elapsed().as_secs_f32();
            let (p50, p95) = pct2(&mut self.enc_us, 0.50, 0.95);
            let (g50, _) = pct2(&mut self.gpu_us, 0.50, 0.95);
            let gpu_label = if self.gpu_us.is_empty() { "（BGRA回退）" } else { "（NV12 via GPU）" };
            println!(
                "[stats] {:.1}s: {:.1}fps | GPU转+读 p50={:.1}ms{} | 编码 p50={:.1}ms p95={:.1}ms | 待发队列 {} | 丢帧 {} | 累计 {} 包（{}）",
                self.start.elapsed().as_secs_f32(),
                self.window_frames as f32 / window_s,
                g50 as f64 / 1000.0,
                gpu_label,
                p50 as f64 / 1000.0,
                p95 as f64 / 1000.0,
                self.backlog.load(std::sync::atomic::Ordering::Relaxed),
                self.dropped,
                self.encoded,
                self.enc_name,
            );
            self.window_frames = 0;
            self.enc_us.clear();
            self.gpu_us.clear();
            self.last_report = Instant::now();
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("[capture] 捕获会话关闭");
        Ok(())
    }
}

/// 窗口样本的 p50/p95（µs）；样本耗尽返回 (0,0)
fn pct2(samples: &mut [u64], a: f64, b: f64) -> (u64, u64) {
    if samples.is_empty() {
        return (0, 0);
    }
    samples.sort_unstable();
    let at = |p: f64| samples[((samples.len() as f64 - 1.0) * p).round() as usize];
    (at(a), at(b))
}

/// 1px 光标微推：制造脏区逼 WGC 出帧（首帧加速的最小实现）
fn nudge_cursor() {
    let (x, y) = crate::input::cursor_pos();
    let (x, y) = (x.max(0) as u32, y.max(0) as u32);
    let _ = crate::input::inject(&rdlink_proto::InputEvent::MouseMove { x: x + 1, y });
    let _ = crate::input::inject(&rdlink_proto::InputEvent::MouseMove { x, y });
}

/// 启动捕获（自由线程）：返回外部控制句柄，会话结束用 `stop()` 主动回收。
/// M4-T2：目标由调用方解析传入（下标选择/设备名重建共用）。
fn start_capture(
    tx: mpsc::Sender<VideoItem>,
    ready: std::sync::mpsc::Sender<()>,
    backlog: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    enc: Option<SendEncoder>,
    monitor: windows_capture::monitor::Monitor,
    info: rdlink_proto::MonitorInfo,
) -> ServeCaptureControl {
    let launch = Instant::now(); // M2-1a：捕获线程启动计时原点
    let boot = CaptureBoot { tx, ready, backlog, launch, enc, info };
    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        boot,
    );
    ServeCapture::start_free_threaded(settings).expect("捕获启动失败")
}

/// 会话内重建捕获（M4-T2：切屏与分辨率变化共用路径）。
/// stop 旧捕获 → 收回编码器（尺寸匹配则复用）→ 同通道起新捕获
/// （new() 内更新 ACTIVE 并先下发新尺寸 Info）→ 等 ready → nudge 逼首帧。
/// 视频流只短暂断供（百毫秒级），会话/控制流不断。
async fn rebuild_capture(
    monitor: windows_capture::monitor::Monitor,
    info: rdlink_proto::MonitorInfo,
    capture_control: &mut Option<ServeCaptureControl>,
    tx: &mpsc::Sender<VideoItem>,
    backlog: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let Some(old_cc) = capture_control.take() else { return };
    let handler = old_cc.callback();
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        tokio::task::spawn_blocking(move || old_cc.stop()),
    )
    .await;
    let mut reuse: Option<SendEncoder> = None;
    if let Some(mut h) = handler.try_lock() {
        if let Some(mut e) = h.enc.take() {
            if h.w == info.width && h.h == info.height {
                e.0.force_key();
                reuse = Some(e);
            }
        }
    }
    let (rtx, rrx) = std::sync::mpsc::channel::<()>();
    *capture_control = Some(start_capture(
        tx.clone(),
        rtx,
        backlog.clone(),
        reuse,
        monitor,
        info,
    ));
    let _ = tokio::task::spawn_blocking(move || {
        rrx.recv_timeout(std::time::Duration::from_secs(3))
    })
    .await;
    nudge_cursor();
}
