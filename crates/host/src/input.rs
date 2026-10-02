//! T8:被控端输入注入。消费 `rdlink_proto::InputEvent` → Win32 `SendInput`。
//!
//! M1 范围:绝对坐标鼠标 + VK 键盘 + Unicode 兜底;坐标像素 → 0..=65535 归一化(主屏)。
//! 已知限制:UIPI——焦点在提权(管理员)窗口时 SendInput 被静默丢弃,返回错误。

use rdlink_proto::{InputEvent, MouseButton};
use windows::core::{Error, HRESULT};
use windows::Win32::Foundation::{GetLastError, POINT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, WHEEL_DELTA,
};

/// 扩展键集合:注入时必须带 KEYEVENTF_EXTENDEDKEY(E0 前缀),否则对端收到的是别的键
/// (经典坑:方向键注入成小键盘;任务卡 T8 专门标注)。
/// 页面导航/编辑键区、数字小键盘除号、右 Ctrl/右 Alt、Apps、PrintScreen、NumLock。
const EXTENDED_VKS: &[u16] = &[
    0x21, 0x22, 0x23, 0x24, // Prior(PgUp) Next(PgDn) End Home
    0x25, 0x26, 0x27, 0x28, // Left Up Right Down
    0x2C, 0x2D, 0x2E, // Snapshot(PrintScreen) Insert Delete
    0x5D, // Apps
    0x6F, // 数字小键盘除号
    0x90, // NumLock
    0xA3, 0xA5, // RControl RMenu(右 Alt)
];

const VK_SHIFT: u16 = 0x10;

/// 屏幕像素 → MOUSEEVENTF_ABSOLUTE 的 0..=65535（M4-T2：虚拟桌面全屏归一化，
/// 支持捕获非主屏/负坐标副屏；ACTIVE 矩形由 monitors.rs 维护）
fn normalize(x: u32, y: u32) -> (i32, i32) {
    crate::monitors::to_virtual_abs(x, y)
}

fn send(inputs: &[INPUT]) -> windows::core::Result<()> {
    let ok = unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32)
    };
    if ok == 0 {
        // 常见原因:UIPI(焦点在提权窗口时 SendInput 被静默丢弃)
        Err(Error::from_hresult(HRESULT::from_win32(unsafe { GetLastError() }.0)))
    } else {
        Ok(())
    }
}

fn mouse_input(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_MOUSE;
    input.Anonymous.mi = MOUSEINPUT {
        dx,
        dy,
        mouseData: data,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    input
}

fn key_input(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: VIRTUAL_KEY(vk),
        wScan: scan,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    input
}

/// 注入一条输入事件。T7 将由 Input stream 消费循环调用。
pub fn inject(event: &InputEvent) -> windows::core::Result<()> {
    match event {
        InputEvent::MouseMove { x, y } => {
            let (nx, ny) = normalize(*x, *y);
            let inputs =
                [mouse_input(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)];
            send(&inputs)
        }
        InputEvent::MouseButton { button, down } => {
            let (flags, data): (MOUSE_EVENT_FLAGS, u32) = match button {
                MouseButton::Left => {
                    (if *down { MOUSEEVENTF_LEFTDOWN } else { MOUSEEVENTF_LEFTUP }, 0)
                }
                MouseButton::Right => {
                    (if *down { MOUSEEVENTF_RIGHTDOWN } else { MOUSEEVENTF_RIGHTUP }, 0)
                }
                MouseButton::Middle => {
                    (if *down { MOUSEEVENTF_MIDDLEDOWN } else { MOUSEEVENTF_MIDDLEUP }, 0)
                }
                // X 按钮:XDOWN/XUP 的 mouseData 指明 XBUTTON1/XBUTTON2
                MouseButton::X1 => (if *down { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, 1),
                MouseButton::X2 => (if *down { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, 2),
            };
            let inputs = [mouse_input(0, 0, data, flags)];
            send(&inputs)
        }
        InputEvent::MouseWheel { dx, dy } => {
            // 垂直/水平各一条,滚动单位 = WHEEL_DELTA(120)/格
            let mut inputs = Vec::new();
            if *dy != 0 {
                inputs.push(mouse_input(0, 0, (*dy * WHEEL_DELTA as i32) as u32, MOUSEEVENTF_WHEEL));
            }
            if *dx != 0 {
                inputs.push(mouse_input(0, 0, (*dx * WHEEL_DELTA as i32) as u32, MOUSEEVENTF_HWHEEL));
            }
            if inputs.is_empty() {
                return Ok(());
            }
            send(&inputs)
        }
        InputEvent::Key { vk, down } => {
            let mut flags = KEYBD_EVENT_FLAGS(0);
            if EXTENDED_VKS.contains(vk) {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !*down {
                flags |= KEYEVENTF_KEYUP;
            }
            let inputs = [key_input(*vk, 0, flags)];
            send(&inputs)
        }
        InputEvent::UnicodeChar { ch } => {
            // KEYEVENTF_UNICODE 必须成对(down+up),wVk=0,字符放 wScan。
            // BMP 外字符（emoji 等，U+10000+）拆 UTF-16 代理对逐个注入。
            let cp = *ch & 0x1F_FFFF;
            let units: Vec<u16> = if cp < 0x1_0000 {
                vec![cp as u16]
            } else {
                let c = cp - 0x1_0000;
                vec![0xD800 | (c >> 10) as u16, 0xDC00 | (c & 0x3FF) as u16]
            };
            let mut inputs = Vec::with_capacity(units.len() * 2);
            for u in units {
                inputs.push(key_input(0, u, KEYEVENTF_UNICODE));
                inputs.push(key_input(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
            send(&inputs)
        }
    }
}

/// 当前光标位置(像素),demo 验证与 T9 打点用。
pub fn cursor_pos() -> (i32, i32) {
    let mut pt = POINT::default();
    unsafe { GetCursorPos(&mut pt) }.ok().expect("GetCursorPos 失败");
    (pt.x, pt.y)
}

/// M3 运维工具：注入一次鼠标左键点击（绝对坐标）。`host --click x y`
/// 供主控端经 SSH 把远端焦点点到位（如测试时把记事本点成前台）。
pub fn click(x: u32, y: u32) {
    let _ = inject(&rdlink_proto::InputEvent::MouseMove { x, y });
    let _ = inject(&rdlink_proto::InputEvent::MouseButton {
        button: rdlink_proto::MouseButton::Left,
        down: true,
    });
    let _ = inject(&rdlink_proto::InputEvent::MouseButton {
        button: rdlink_proto::MouseButton::Left,
        down: false,
    });
    println!("已点击 ({x},{y})");
}

fn key_down(vk: u16) -> bool {
    (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
}

/// `host --input-demo`:注入自测,全部可程序化验证,不产生破坏性输入。
/// (不打字、不点击——那些留给 T7 两机联调时在记事本里人工验收)
pub fn input_demo() {
    println!("── T8 输入注入自测 ──");
    // 主屏尺寸（M4-T2 起经 monitors 取，保持虚拟桌面语义一致）
    let (w, h) = {
        let m = crate::monitors::enumerate()
            .first()
            .cloned()
            .unwrap_or(rdlink_proto::MonitorInfo {
                index: 0,
                device_name: String::new(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            });
        (m.width as i32, m.height as i32)
    };
    println!("主屏: {w}x{h}");

    // 1) 鼠标绝对移动:注入 → 回读比对(容差 ±2px,归一化取整误差)
    let cx = w / 2;
    let cy = h / 2;
    let mut pass = 0;
    let mut fail = 0;
    for (tx, ty) in [(cx + 150, cy), (cx - 150, cy), (cx, cy + 100), (cx, cy - 100)] {
        inject(&InputEvent::MouseMove { x: tx as u32, y: ty as u32 })
            .expect("SendInput 失败(焦点在提权窗口?UIPI)");
        std::thread::sleep(std::time::Duration::from_millis(30));
        let (ax, ay) = cursor_pos();
        let ok = (ax - tx).abs() <= 2 && (ay - ty).abs() <= 2;
        if ok { pass += 1 } else { fail += 1 }
        println!(
            "[{}] MouseMove → ({tx},{ty}),回读 ({ax},{ay})",
            if ok { "PASS" } else { "FAIL" }
        );
    }

    // 2) 普通键:Shift 按下 → GetAsyncKeyState 应为按下态 → 抬起 → 清零
    inject(&InputEvent::Key { vk: VK_SHIFT, down: true }).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let s_down = key_down(VK_SHIFT);
    inject(&InputEvent::Key { vk: VK_SHIFT, down: false }).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let s_up = !key_down(VK_SHIFT);
    let shift_ok = s_down && s_up;
    if shift_ok { pass += 1 } else { fail += 1 }
    println!("[{}] Key Shift: 按下态={s_down} 抬起态清零={s_up}", if shift_ok { "PASS" } else { "FAIL" });

    // 3) 扩展键(方向键,带 EXTENDEDKEY):VK_DOWN 按下/抬起状态验证
    const VK_DOWN: u16 = 0x28;
    inject(&InputEvent::Key { vk: VK_DOWN, down: true }).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let d_down = key_down(VK_DOWN);
    inject(&InputEvent::Key { vk: VK_DOWN, down: false }).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let d_up = !key_down(VK_DOWN);
    let ext_ok = d_down && d_up;
    if ext_ok { pass += 1 } else { fail += 1 }
    println!("[{}] 扩展键 VK_DOWN(EXTENDEDKEY): 按下={d_down} 抬起清零={d_up}", if ext_ok { "PASS" } else { "FAIL" });

    // 4) 滚轮与 Unicode 注入:无法无副作用验证,只演示不判分
    inject(&InputEvent::MouseWheel { dx: 0, dy: -1 }).unwrap();
    inject(&InputEvent::UnicodeChar { ch: 'r' as u32 }).unwrap();
    println!("[演示] 滚轮 -1 格 / Unicode 'r' 已注入(效果取决于当前焦点窗口,人工验收在 T7)");

    println!("── 结果: {pass} PASS / {fail} FAIL ──");
    if fail > 0 {
        std::process::exit(1);
    }
}
