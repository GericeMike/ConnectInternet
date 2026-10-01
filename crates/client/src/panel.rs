//! M3-0：控制面板（UI 地基）。
//!
//! 独立原生窗口方案（而非 egui 叠层）：零新依赖、Win32 原生控件输入法开箱即用、
//! 输入路由天然隔离（面板窗口有独立焦点，rdlink 窗口保持焦点时按键照常透传被控端，
//! 面板可以常驻屏幕边角随时操作——对齐商业远控的悬浮工具条体验）。
//!
//! 结构：一个专用线程 = 热键 + 面板窗口 + 消息循环。
//! - `RegisterHotKey(Ctrl+Alt+U)` 全局热键：任意焦点下呼出/隐藏面板；
//! - 面板窗口隐藏创建，热键切换显示；点 X = 隐藏（不是销毁，控件状态保留）；
//! - M3-2/3/4/5 的控件（文件列表/进程表/电源按钮/密码框）作为子控件挂进本窗口。
//!
//! 后续任务通过 `PanelCmd` 通道与面板交互（设置文本/列表等），M3-0 先立骨架。

use std::sync::atomic::{AtomicBool, Ordering};

/// 面板当前是否可见（热键线程与面板线程共享）
static PANEL_VISIBLE: AtomicBool = AtomicBool::new(false);

const HOTKEY_TOGGLE: i32 = 1;
/// Ctrl+Alt+U
const HOTKEY_MODS: u32 = 0x0002 | 0x0001; // MOD_CONTROL | MOD_ALT
const VK_U: u32 = 0x55;
/// 自定义：跨线程关闭/隐藏面板（WM_APP_HIDE_PANEL）
const WM_APP_HIDE_PANEL: u32 = 0x8000;

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
    unsafe {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::*;

        let hinstance: windows::Win32::Foundation::HINSTANCE =
            GetModuleHandleW(None).map_err(|e| e.to_string())?.into();

        // 面板窗口类（普通顶层窗口，不注册显示——热键切换可见性）
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

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0x0000_0100), // WS_EX_TOOLWINDOW：不进任务栏
            class_name,
            windows::core::w!("rdlink 控制面板"),
            WS_OVERLAPPEDWINDOW, // 隐藏创建，Ctrl+Alt+U 切换显示
            60,
            60,
            420,
            300,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .map_err(|e| format!("CreateWindowExW: {e}"))?;

        // 骨架内容（M3-2/3/4/5 接入时替换为各自控件）
        let static_style = WS_CHILD | WS_VISIBLE;
        let mk_static = |text: windows::core::PCWSTR, y: i32, h: i32| -> Result<(), String> {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                windows::core::w!("STATIC"),
                text,
                static_style,
                16,
                y,
                380,
                h,
                Some(hwnd),
                None,
                Some(hinstance.into()),
                None,
            )
            .map(|_| ())
            .map_err(|e| format!("STATIC: {e}"))
        };
        mk_static(windows::core::w!("剪贴板同步：已启用（双向，纯文本）"), 20, 22)?;
        mk_static(windows::core::w!("传输 / 进程 / 电源 / 设置：随 M3-2~M3-5 接入"), 52, 44)?;

        // 全局热键：Ctrl+Alt+U 呼出/隐藏（任意焦点下生效）
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
                let visible = PANEL_VISIBLE.load(Ordering::SeqCst);
                if visible {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    PANEL_VISIBLE.store(false, Ordering::SeqCst);
                } else {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    PANEL_VISIBLE.store(true, Ordering::SeqCst);
                }
                windows::Win32::Foundation::LRESULT(0)
            }
            WM_APP_HIDE_PANEL => {
                let _ = ShowWindow(hwnd, SW_HIDE);
                PANEL_VISIBLE.store(false, Ordering::SeqCst);
                windows::Win32::Foundation::LRESULT(0)
            }
            // 点 X = 隐藏（控件状态保留），不销毁窗口
            WM_CLOSE => {
                let _ = ShowWindow(hwnd, SW_HIDE);
                PANEL_VISIBLE.store(false, Ordering::SeqCst);
                windows::Win32::Foundation::LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
