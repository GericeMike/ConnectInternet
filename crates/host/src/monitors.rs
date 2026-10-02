//! M4-T2：显示器枚举、虚拟桌面矩形与捕获目标解析。
//!
//! 坐标系：虚拟桌面（主屏左上角 = (0,0)，副屏可为负坐标）。
//! 枚举用 `EnumDisplayMonitors`——与 windows-capture 的 `Monitor::enumerate()`
//! 走同一回调，顺序一致；捕获目标最终按 `device_name`（\\.\DISPLAY1 风格）
//! 精确匹配，避免依赖枚举顺序的稳定性。

use std::sync::Mutex;

use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW,
    HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// 虚拟桌面矩形（物理像素）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// 当前被捕获显示器的矩形（输入映射用）。会话启动/切屏/分辨率变化时更新。
/// None = 尚未初始化（回退主屏矩形）。
static ACTIVE: Mutex<Option<(String, MonRect)>> = Mutex::new(None);

/// 虚拟桌面全屏矩形（SM_*VIRTUALSCREEN 四元组）
fn virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

fn primary_size() -> (i64, i64) {
    unsafe {
        (
            GetSystemMetrics(SM_CXSCREEN).max(1) as i64,
            GetSystemMetrics(SM_CYSCREEN).max(1) as i64,
        )
    }
}

/// 捕获显示器-local 像素 → MOUSEEVENTF_ABSOLUTE|VIRTUALDESK 的 0..=65535。
/// 公式 (v - v0) * 65535 / (span - 1)：端点精确映射到像素中心，无半像素偏移。
/// local 坐标先夹紧在捕获屏内（client 端已按 host_size 归一化，这里是双保险）。
pub fn to_virtual_abs(x: u32, y: u32) -> (i32, i32) {
    let (vx0, vy0, vw, vh) = virtual_screen();
    let (vw, vh) = (vw.max(1) as i64, vh.max(1) as i64);
    let (ox, oy, mw, mh) = match ACTIVE.lock().expect("ACTIVE 锁").as_ref() {
        Some((_, r)) => (r.x as i64, r.y as i64, r.w.max(1) as i64, r.h.max(1) as i64),
        None => (0, 0, primary_size().0, primary_size().1),
    };
    let ax = ox + (x.min((mw - 1).max(0) as u32) as i64);
    let ay = oy + (y.min((mh - 1).max(0) as u32) as i64);
    let span_x = (vw - 1).max(1);
    let span_y = (vh - 1).max(1);
    let nx = ((ax - vx0 as i64) * 65535) / span_x;
    let ny = ((ay - vy0 as i64) * 65535) / span_y;
    (nx.clamp(0, 65535) as i32, ny.clamp(0, 65535) as i32)
}

/// 枚举全部显示器（顺序与 windows-capture 一致）。
pub fn enumerate() -> Vec<rdlink_proto::MonitorInfo> {
    let mut out: Vec<rdlink_proto::MonitorInfo> = Vec::new();
    let ctx = &mut out as *mut _ as isize;
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(enum_cb), LPARAM(ctx));
    }
    out
}

unsafe extern "system" fn enum_cb(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut Vec<rdlink_proto::MonitorInfo>) };
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            rcMonitor: RECT::default(),
            rcWork: RECT::default(),
            dwFlags: 0,
        },
        szDevice: [0; 32],
    };
    if unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast()) }.as_bool() {
        let dev = String::from_utf16_lossy(
            &info.szDevice[..info.szDevice.iter().position(|&c| c == 0).unwrap_or(32)],
        );
        let r = info.monitorInfo.rcMonitor;
        out.push(rdlink_proto::MonitorInfo {
            index: out.len() as u32,
            device_name: dev,
            x: r.left,
            y: r.top,
            width: (r.right - r.left).max(0) as u32,
            height: (r.bottom - r.top).max(0) as u32,
        });
    }
    BOOL(1)
}

/// 更新当前捕获矩形（捕获会话建立/切屏时调用；分辨率变化时调 update_active_size）
pub fn set_active(device_name: &str, rect: MonRect) {
    *ACTIVE.lock().expect("ACTIVE 锁") = Some((device_name.to_string(), rect));
}

/// 分辨率变化：保持设备与原点，仅改尺寸
pub fn update_active_size(w: u32, h: u32) {
    let mut g = ACTIVE.lock().expect("ACTIVE 锁");
    if let Some((_, r)) = g.as_mut() {
        r.w = w;
        r.h = h;
    }
}

/// 当前 active 的设备名（无则空串）
pub fn active_device() -> String {
    ACTIVE
        .lock()
        .expect("ACTIVE 锁")
        .as_ref()
        .map(|(d, _)| d.clone())
        .unwrap_or_default()
}

/// `host --set-res W H`：改主显示器分辨率（M4-T2.4 E2E 驱动 / 运维工具）。
/// 须在交互会话运行（SSH 直跑也可——本机 sshd 会话可注入输入说明窗口站可达；
/// 若失败走 schtasks /IT）。返回 (原分辨率, CDS 结果码)。
pub fn set_resolution(w: u32, h: u32) -> Result<((u32, u32), i32), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        DEVMODEW, DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS,
    };
    unsafe {
        let mut dm = DEVMODEW::default();
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        if EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut dm).0 == 0 {
            return Err("EnumDisplaySettingsW 失败（非交互会话？）".into());
        }
        let orig = (dm.dmPelsWidth, dm.dmPelsHeight);
        dm.dmPelsWidth = w;
        dm.dmPelsHeight = h;
        dm.dmFields |= DM_PELSWIDTH | DM_PELSHEIGHT;
        let r = ChangeDisplaySettingsExW(
            PCWSTR::null(),
            Some(&dm as *const _),
            None,
            Default::default(),
            None,
        );
        Ok((orig, r.0))
    }
}

/// 解析捕获目标：优先 prefer 下标（我的枚举顺序），按 device_name 与
/// windows-capture 对齐；全部失败回退主屏。
pub fn resolve_capture_target(
    prefer: Option<u32>,
) -> Result<
    (
        windows_capture::monitor::Monitor,
        MonRect,
        rdlink_proto::MonitorInfo,
    ),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let mine = enumerate();
    let pick = prefer
        .and_then(|i| mine.get(i as usize))
        .or_else(|| mine.first())
        .cloned();
    match pick {
        Some(p) => {
            let wc = windows_capture::monitor::Monitor::enumerate()?;
            let m = wc
                .iter()
                .find(|m| m.device_name().ok().as_deref() == Some(p.device_name.as_str()))
                .or_else(|| wc.get(p.index as usize))
                .ok_or("windows-capture 枚举中找不到目标显示器")?;
            let rect = MonRect { x: p.x, y: p.y, w: p.width, h: p.height };
            Ok((m.clone(), rect, p))
        }
        None => {
            // 一块屏都枚举不到（异常环境）——回退 windows-capture 主屏
            let m = windows_capture::monitor::Monitor::primary()?;
            let (w, h) = (m.width()?, m.height()?);
            let info = rdlink_proto::MonitorInfo {
                index: 0,
                device_name: m.device_name().unwrap_or_default(),
                x: 0,
                y: 0,
                width: w,
                height: h,
            };
            Ok((m, MonRect { x: 0, y: 0, w, h }, info))
        }
    }
}
