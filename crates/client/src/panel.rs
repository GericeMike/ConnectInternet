//! M3-0/M3-2：控制面板。
//!
//! 独立原生窗口方案（而非 egui 叠层）：零新依赖、Win32 原生控件输入法开箱即用、
//! 输入路由天然隔离（面板窗口有独立焦点，rdlink 窗口保持焦点时按键照常透传被控端，
//! 面板可以常驻屏幕边角随时操作——对齐商业远控的悬浮工具条体验）。
//!
//! 结构：一个专用线程 = 热键 + 面板窗口 + 消息循环。
//! - `RegisterHotKey(Ctrl+Alt+U)` 全局热键：任意焦点下呼出/隐藏面板；
//! - 面板窗口隐藏创建，热键切换显示；点 X = 隐藏（不是销毁，控件状态保留）；
//! - 250ms 定时器排空文件传输事件（filex）刷新列表/进度/状态。
//! - M3-3/4/5 的控件（电源/进程/密码框）继续作为子控件挂进本窗口。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};

use windows::Win32::Foundation::{HWND, LRESULT};

/// 面板当前是否可见（热键线程与面板线程共享）
static PANEL_VISIBLE: AtomicBool = AtomicBool::new(false);

/// 面板事件接收端（panel 线程独占）+ 控件句柄
static EVENT_RX: Mutex<Option<std::sync::mpsc::Receiver<crate::filex::PanelEvent>>> =
    Mutex::new(None);
static PANEL_HWND: AtomicUsize = AtomicUsize::new(0);
static LIST_HWND: AtomicUsize = AtomicUsize::new(0);
static STATUS_HWND: AtomicUsize = AtomicUsize::new(0);
/// host Downloads 文件列表缓存（(名称, 大小)，下载按钮按选中行取）
static FILE_LIST: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());
/// 被控端进程列表缓存（(pid, 名称)，结束按钮按选中行取）
static PROC_LIST: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());
/// 进程请求出口（stream.rs 会话建立时注入，断开时清除）
static PROC_TX: RwLock<Option<tokio::sync::mpsc::UnboundedSender<ProcRequest>>> =
    RwLock::new(None);

/// 面板 → 会话的进程请求
#[derive(Debug, Clone, Copy)]
pub enum ProcRequest {
    List,
    Kill(u32),
}

/// stream.rs 会话建立时注入
pub fn set_proc_tx(tx: tokio::sync::mpsc::UnboundedSender<ProcRequest>) {
    *PROC_TX.write().unwrap() = Some(tx);
}

/// 其他模块（control 收任务自动刷新）获取请求出口
pub fn proc_tx() -> Option<tokio::sync::mpsc::UnboundedSender<ProcRequest>> {
    PROC_TX.read().unwrap().clone()
}

/// 会话结束清除
pub fn clear_proc_tx() {
    *PROC_TX.write().unwrap() = None;
}

fn submit_proc(req: ProcRequest) {
    match PROC_TX.read().unwrap().as_ref() {
        Some(tx) => {
            let _ = tx.send(req);
        }
        None => set_status("状态：未连接，进程管理不可用"),
    }
}
/// 电源动作出口（stream.rs 会话建立时注入，断开时清除）
static POWER_TX: RwLock<Option<tokio::sync::mpsc::UnboundedSender<rdlink_proto::PowerActionKind>>> =
    RwLock::new(None);
/// 重启/关机的两段式确认状态（按钮 id + 按下时刻）
static CONFIRM_ARMED: Mutex<Option<(i32, std::time::Instant)>> = Mutex::new(None);

/// stream.rs 会话建立时注入电源动作出口
pub fn set_power_tx(tx: tokio::sync::mpsc::UnboundedSender<rdlink_proto::PowerActionKind>) {
    *POWER_TX.write().unwrap() = Some(tx);
}

/// 会话结束清除
pub fn clear_power_tx() {
    *POWER_TX.write().unwrap() = None;
}

fn submit_power(action: rdlink_proto::PowerActionKind) {
    match POWER_TX.read().unwrap().as_ref() {
        Some(tx) => {
            let _ = tx.send(action);
        }
        None => set_status("状态：未连接，电源动作不可用"),
    }
}

const HOTKEY_TOGGLE: i32 = 1;
/// Ctrl+Alt+U
const HOTKEY_MODS: u32 = 0x0002 | 0x0001; // MOD_CONTROL | MOD_ALT
const VK_U: u32 = 0x55;
/// WM_APP_HIDE_PANEL
const WM_APP_HIDE_PANEL: u32 = 0x8000;

/// 子控件 ID
const IDC_FILE_LIST: i32 = 101;
const IDC_BTN_REFRESH: i32 = 102;
const IDC_BTN_DOWNLOAD: i32 = 103;
const IDC_STATUS: i32 = 104;
const IDC_PWR_LOCK: i32 = 110;
const IDC_PWR_SLEEP: i32 = 111;
const IDC_PWR_RESTART: i32 = 112;
const IDC_PWR_SHUTDOWN: i32 = 113;
const IDC_PROC_LIST: i32 = 201;
const IDC_PROC_REFRESH: i32 = 202;
const IDC_PROC_KILL: i32 = 203;
const TIMER_DRAIN: usize = 1;
const CONFIRM_WINDOW_SECS: u64 = 5;

/// 启动面板线程（热键 + 面板窗口 + 消息循环）。进程生命周期内常驻。
pub fn spawn() {
    let r = std::thread::Builder::new()
        .name("rdlink-panel".into())
        .spawn(|| {
            if let Err(e) = panel_thread() {
                eprintln!("[panel] 控制面板不可用: {e}");
            }
        });
    if let Err(e) = r {
        eprintln!("[panel] 面板线程创建失败: {e}");
    }
}

fn panel_thread() -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;

        let hinstance: windows::Win32::Foundation::HINSTANCE =
            GetModuleHandleW(None).map_err(|e| e.to_string())?.into();

        let class_name = windows::core::w!("rdlink_panel");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(panel_wndproc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err("RegisterClassW 失败".into());
        }

        // 隐藏创建，Ctrl+Alt+U 切换显示
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0x0000_0100), // WS_EX_TOOLWINDOW：不进任务栏
            class_name,
            windows::core::w!("rdlink 控制面板"),
            WS_OVERLAPPEDWINDOW,
            60,
            60,
            430,
            660,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .map_err(|e| format!("CreateWindowExW: {e}"))?;
        PANEL_HWND.store(hwnd.0 as usize, Ordering::SeqCst);

        let child = |text: windows::core::PCWSTR,
                     class: windows::core::PCWSTR,
                     style: WINDOW_STYLE,
                     x: i32,
                     y: i32,
                     w: i32,
                     h: i32,
                     id: i32|
         -> Result<(), String> {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                text,
                style,
                x,
                y,
                w,
                h,
                Some(hwnd),
                Some(HMENU(id as _)),
                Some(hinstance.into()),
                None,
            )
            .map(|_| ())
            .map_err(|e| format!("{:?}: {e}", unsafe { class.to_string() }))
        };

        const CHILD: WINDOW_STYLE = WINDOW_STYLE(0x4000_0000 | 0x1000_0000); // WS_CHILD|WS_VISIBLE
        child(
            windows::core::w!("剪贴板同步：已启用（双向：文本 + 截图）"),
            windows::core::w!("STATIC"),
            CHILD,
            16,
            14,
            390,
            22,
            0,
        )?;
        child(
            windows::core::w!("被控端 Downloads（拖文件进 rdlink 窗口 = 上传）："),
            windows::core::w!("STATIC"),
            CHILD,
            16,
            46,
            390,
            22,
            0,
        )?;
        child(
            windows::core::w!(""),
            windows::core::w!("LISTBOX"),
            CHILD | WS_BORDER | WS_VSCROLL | WINDOW_STYLE(LBS_NOTIFY as u32),
            16,
            74,
            398,
            170,
            IDC_FILE_LIST,
        )?;
        child(
            windows::core::w!("刷新列表"),
            windows::core::w!("BUTTON"),
            CHILD,
            16,
            252,
            100,
            28,
            IDC_BTN_REFRESH,
        )?;
        child(
            windows::core::w!("下载选中到主控端"),
            windows::core::w!("BUTTON"),
            CHILD,
            124,
            252,
            160,
            28,
            IDC_BTN_DOWNLOAD,
        )?;
        child(
            windows::core::w!("状态：-"),
            windows::core::w!("STATIC"),
            CHILD,
            16,
            288,
            398,
            24,
            IDC_STATUS,
        )?;
        // M3-4 进程区
        child(
            windows::core::w!("被控端进程（按 CPU 排序）："),
            windows::core::w!("STATIC"),
            CHILD,
            16,
            320,
            390,
            22,
            0,
        )?;
        child(
            windows::core::w!(""),
            windows::core::w!("LISTBOX"),
            CHILD | WS_BORDER | WS_VSCROLL | WINDOW_STYLE(LBS_NOTIFY as u32),
            16,
            348,
            398,
            150,
            IDC_PROC_LIST,
        )?;
        child(
            windows::core::w!("刷新进程"),
            windows::core::w!("BUTTON"),
            CHILD,
            16,
            506,
            100,
            28,
            IDC_PROC_REFRESH,
        )?;
        child(
            windows::core::w!("结束选中进程"),
            windows::core::w!("BUTTON"),
            CHILD,
            124,
            506,
            130,
            28,
            IDC_PROC_KILL,
        )?;
        // M3-3 电源区：锁屏/睡眠立即执行；重启/关机两段式确认
        child(
            windows::core::w!("电源（重启/关机需二次确认）："),
            windows::core::w!("STATIC"),
            CHILD,
            16,
            544,
            390,
            22,
            0,
        )?;
        child(
            windows::core::w!("锁屏"),
            windows::core::w!("BUTTON"),
            CHILD,
            16,
            570,
            92,
            28,
            IDC_PWR_LOCK,
        )?;
        child(
            windows::core::w!("睡眠"),
            windows::core::w!("BUTTON"),
            CHILD,
            114,
            570,
            92,
            28,
            IDC_PWR_SLEEP,
        )?;
        child(
            windows::core::w!("重启"),
            windows::core::w!("BUTTON"),
            CHILD,
            212,
            570,
            92,
            28,
            IDC_PWR_RESTART,
        )?;
        child(
            windows::core::w!("关机"),
            windows::core::w!("BUTTON"),
            CHILD,
            310,
            570,
            92,
            28,
            IDC_PWR_SHUTDOWN,
        )?;
        // 状态行句柄（进度/状态文本刷新用）
        let status = GetDlgItem(Some(hwnd), IDC_STATUS).unwrap_or_default();
        STATUS_HWND.store(status.0 as usize, Ordering::SeqCst);
        // 列表句柄
        let lb = GetDlgItem(Some(hwnd), IDC_FILE_LIST).unwrap_or_default();
        LIST_HWND.store(lb.0 as usize, Ordering::SeqCst);

        // 事件排水定时器
        let (tx, rx) = std::sync::mpsc::channel::<crate::filex::PanelEvent>();
        crate::filex::set_panel_events(tx);
        *EVENT_RX.lock().unwrap() = Some(rx);
        SetTimer(Some(hwnd), TIMER_DRAIN, 250, None);

        // 全局热键：Ctrl+Alt+U
        use windows::Win32::UI::Input::KeyboardAndMouse::{HOT_KEY_MODIFIERS, RegisterHotKey};
        RegisterHotKey(Some(hwnd), HOTKEY_TOGGLE, HOT_KEY_MODIFIERS(HOTKEY_MODS), VK_U)
            .map_err(|e| format!("RegisterHotKey(Ctrl+Alt+U): {e}"))?;
        println!("[panel] 控制面板就绪（Ctrl+Alt+U 呼出/隐藏）");

        let mut msg = std::mem::zeroed::<MSG>();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        Ok(())
    }
}

fn toggle_visible() {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::*;
        let hwnd = HWND(PANEL_HWND.load(Ordering::SeqCst) as _);
        if hwnd.is_invalid() {
            return;
        }
        if PANEL_VISIBLE.load(Ordering::SeqCst) {
            let _ = ShowWindow(hwnd, SW_HIDE);
            PANEL_VISIBLE.store(false, Ordering::SeqCst);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            PANEL_VISIBLE.store(true, Ordering::SeqCst);
        }
    }
}

fn set_status(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::SetWindowTextW;
    let hwnd: usize = STATUS_HWND.load(Ordering::SeqCst);
    if hwnd != 0 {
        let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
        unsafe {
            let _ = SetWindowTextW(HWND(hwnd as _), windows::core::PCWSTR(wide.as_ptr()));
        }
    }
}

fn set_button_text(id: i32, text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{GetDlgItem, SetWindowTextW};
    unsafe {
        let hwnd = HWND(PANEL_HWND.load(Ordering::SeqCst) as _);
        if hwnd.is_invalid() {
            return;
        }
        let ctl = GetDlgItem(Some(hwnd), id).unwrap_or_default();
        if !ctl.is_invalid() {
            let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
            let _ = SetWindowTextW(ctl, windows::core::PCWSTR(wide.as_ptr()));
        }
    }
}

/// 重启/关机两段式确认：第一次点进入待确认（按钮变"确认？再点一次"），
/// CONFIRM_WINDOW_SECS 内再点才真正发送；超时自动还原。
fn power_confirm(id: i32, confirm_text: &str, fire: impl FnOnce()) {
    let mut armed = CONFIRM_ARMED.lock().unwrap();
    match armed.as_ref() {
        Some((aid, _)) if *aid == id => {
            *armed = None;
            drop(armed);
            fire();
            let label = match id {
                IDC_PWR_RESTART => "重启",
                IDC_PWR_SHUTDOWN => "关机",
                _ => "?",
            };
            set_button_text(id, label);
        }
        _ => {
            *armed = Some((id, std::time::Instant::now()));
            set_button_text(id, confirm_text);
        }
    }
}

unsafe extern "system" fn panel_wndproc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        match msg {
            WM_HOTKEY if wparam.0 as i32 == HOTKEY_TOGGLE => {
                toggle_visible();
                LRESULT(0)
            }
            WM_APP_HIDE_PANEL => {
                toggle_visible_off();
                LRESULT(0)
            }
            WM_CLOSE => {
                toggle_visible_off();
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                match id {
                    IDC_BTN_REFRESH => {
                        crate::filex::submit(crate::filex::XferRequest::Refresh);
                    }
                    IDC_BTN_DOWNLOAD => {
                        if let Some((name, size)) = selected_download() {
                            crate::filex::submit(crate::filex::XferRequest::Download { name, size });
                        } else {
                            set_status("状态：请先在列表中选择要下载的文件");
                        }
                    }
                    IDC_PWR_LOCK => {
                        submit_power(rdlink_proto::PowerActionKind::Lock);
                        set_status("状态：已发送锁屏命令");
                    }
                    IDC_PWR_SLEEP => {
                        submit_power(rdlink_proto::PowerActionKind::Sleep);
                        set_status("状态：已发送睡眠命令（唤醒需在被控端按电源键）");
                    }
                    IDC_PWR_RESTART => {
                        power_confirm(IDC_PWR_RESTART, "确认重启？再点一次", || {
                            submit_power(rdlink_proto::PowerActionKind::Restart);
                            set_status("状态：已发送重启命令（被控端重启后自动恢复连接）");
                        });
                    }
                    IDC_PWR_SHUTDOWN => {
                        power_confirm(IDC_PWR_SHUTDOWN, "确认关机？再点一次", || {
                            submit_power(rdlink_proto::PowerActionKind::Shutdown);
                            set_status("状态：已发送关机命令");
                        });
                    }
                    IDC_PROC_REFRESH => {
                        submit_proc(ProcRequest::List);
                    }
                    IDC_PROC_KILL => {
                        if let Some((pid, name)) = selected_process() {
                            submit_proc(ProcRequest::Kill(pid));
                            set_status(&format!("状态：已发送结束进程命令（{pid} {name}）"));
                        } else {
                            set_status("状态：请先在进程列表中选择要结束的进程");
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 as usize == TIMER_DRAIN => {
                // 确认状态超时（5s）→ 按钮文案还原
                {
                    let mut armed = CONFIRM_ARMED.lock().unwrap();
                    if let Some((id, at)) = armed.as_ref() {
                        if at.elapsed().as_secs() >= CONFIRM_WINDOW_SECS {
                            let label = match *id {
                                IDC_PWR_RESTART => "重启",
                                IDC_PWR_SHUTDOWN => "关机",
                                _ => "?",
                            };
                            set_button_text(*id, label);
                            *armed = None;
                        }
                    }
                }
                drain_filex_events();
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn toggle_visible_off() {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::*;
        let hwnd = HWND(PANEL_HWND.load(Ordering::SeqCst) as _);
        if !hwnd.is_invalid() {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        PANEL_VISIBLE.store(false, Ordering::SeqCst);
    }
}

/// WM_TIMER：排空文件传输事件并刷新控件
fn drain_filex_events() {
    let mut events = Vec::new();
    {
        let mut rx = EVENT_RX.lock().unwrap();
        if let Some(rx) = rx.as_ref() {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
    }
    for ev in events {
        match ev {
            crate::filex::PanelEvent::List(entries) => {
                *FILE_LIST.lock().unwrap() =
                    entries.iter().map(|e| (e.name.clone(), e.size)).collect();
                unsafe {
                    use windows::Win32::Foundation::{LPARAM, WPARAM};
                    let lb: usize = LIST_HWND.load(Ordering::SeqCst);
                    if lb == 0 {
                        continue;
                    }
                    use windows::Win32::UI::WindowsAndMessaging::*;
                    let hwnd = windows::Win32::Foundation::HWND(lb as _);
                    let _ = SendMessageW(hwnd, LB_RESETCONTENT, Some(WPARAM(0)), Some(LPARAM(0)));
                    for e in &entries {
                        let text = format!(
                            "{}\t{}",
                            e.name,
                            if e.is_dir { "<目录>".into() } else { human_size(e.size) }
                        );
                        let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
                        let _ = SendMessageW(
                            hwnd,
                            LB_ADDSTRING,
                            Some(WPARAM(0)),
                            Some(LPARAM(wide.as_ptr() as _)),
                        );
                    }
                }
                set_status(&format!("状态：已获取被控端 Downloads（{} 项）", entries.len()));
            }
            crate::filex::PanelEvent::Status(s) => set_status(&format!("状态：{s}")),
            crate::filex::PanelEvent::ProcList(entries) => {
                *PROC_LIST.lock().unwrap() =
                    entries.iter().map(|e| (e.pid, e.name.clone())).collect();
                unsafe {
                    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
                    use windows::Win32::UI::WindowsAndMessaging::*;
                    let panel = HWND(PANEL_HWND.load(Ordering::SeqCst) as _);
                    let lb = GetDlgItem(Some(panel), IDC_PROC_LIST).unwrap_or_default();
                    if lb.is_invalid() {
                        return;
                    }
                    let _ = SendMessageW(lb, LB_RESETCONTENT, Some(WPARAM(0)), Some(LPARAM(0)));
                    for e in entries.iter().take(60) {
                        let text = format!(
                            "{}\t{}\tCPU {:.1}%\t{:.0} MB",
                            e.pid, e.name, e.cpu, e.mem_mb
                        );
                        let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
                        let _ = SendMessageW(
                            lb,
                            LB_ADDSTRING,
                            Some(WPARAM(0)),
                            Some(LPARAM(wide.as_ptr() as _)),
                        );
                    }
                }
                set_status(&format!("状态：已获取进程列表（{} 项）", entries.len()));
            }
            crate::filex::PanelEvent::Progress { label, current, total } => {
                let pct = if total > 0 { current * 100 / total } else { 0 };
                set_status(&format!(
                    "状态：{label} — {pct}%（{} / {}）",
                    human_size(current),
                    human_size(total)
                ));
            }
        }
    }
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / 1048576.0)
    } else if bytes >= 1024 {
        format!("{:.0} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// 结束进程按钮：取进程列表框选中项对应的 (pid, 名称)
fn selected_process() -> Option<(u32, String)> {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let panel = HWND(PANEL_HWND.load(Ordering::SeqCst) as _);
        let lb = GetDlgItem(Some(panel), IDC_PROC_LIST).unwrap_or_default();
        if lb.is_invalid() {
            return None;
        }
        let sel = SendMessageW(lb, LB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0;
        if sel < 0 {
            return None;
        }
        PROC_LIST.lock().unwrap().get(sel as usize).cloned()
    }
}

/// 下载按钮：取列表框选中项对应的 (名称, 大小)
fn selected_download() -> Option<(String, u64)> {
    let lb = LIST_HWND.load(Ordering::SeqCst);
    if lb == 0 {
        return None;
    }
    unsafe {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::*;
        let hwnd = HWND(lb as _);
        let sel = SendMessageW(hwnd, LB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0;
        if sel < 0 {
            return None;
        }
        let len = SendMessageW(hwnd, LB_GETTEXTLEN, Some(WPARAM(sel as _)), Some(LPARAM(0))).0 as usize;
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u16; len + 1];
        SendMessageW(
            hwnd,
            LB_GETTEXT,
            Some(WPARAM(sel as _)),
            Some(LPARAM(buf.as_mut_ptr() as _)),
        );
        let _display = String::from_utf16_lossy(&buf[..len]);
        // 按 FILE_LIST 索引取 (名称, 大小) 更可靠
        let list = FILE_LIST.lock().unwrap();
        list.get(sel as usize).cloned()
    }
}
