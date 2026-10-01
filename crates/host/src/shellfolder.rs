//! M3-2：检测被控端**前台 Explorer 窗口正在显示的文件夹**（拖拽落点跟随）。
//!
//! 前台窗口分类：
//! - 桌面（Progman/WorkerW）→ 桌面路径；
//! - Explorer（CabinetWClass）→ 用 Shell.Application COM 按 HWND 匹配取
//!   LocationURL（经 PowerShell，控制台会话上下文可靠；Rust 内联 COM 在
//!   windows 0.62 绑定上摩擦过大，不划算）；
//! - 其他 → None（调用方落 Downloads 兜底）。

use std::path::PathBuf;

use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow};

/// 返回前台 Explorer/桌面正在显示的文件夹路径。检测不到 → None。
pub fn foreground_view_folder() -> Option<PathBuf> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut buf = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut buf);
        let class = String::from_utf16_lossy(&buf[..n as usize]);

        if class == "Progman" || class == "WorkerW" {
            return known_desktop();
        }
        if class != "CabinetWClass" {
            return None;
        }
        let hwnd_dec = hwnd.0 as i64;
        // PowerShell：Shell.Application 按 HWND 匹配 Explorer 实例取 LocationURL
        let script = format!(
            "(New-Object -ComObject Shell.Application).Windows() | \
             Where-Object {{ $_.HWND -eq {hwnd_dec} }} | \
             Select-Object -First 1 -ExpandProperty LocationURL"
        );
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-STA", "-Command", &script])
            .output()
            .ok()?;
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        location_url_to_path(&url)
    }
}

/// "file:///D:/test" → "D:\test"；无法解析（特殊视图如"此电脑"）→ None
fn location_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file:///")?;
    if rest.is_empty() {
        return None;
    }
    // URL 解码（%20 空格等）
    let decoded = url_decode(rest)?;
    // file:///D:/test → D:/test → D:\test
    let path = decoded.replace('/', "\\");
    if path.len() < 2 || !path.as_bytes()[1].is_ascii_alphabetic() {
        return None; // 不是盘符路径（网络位置等）
    }
    Some(PathBuf::from(path))
}

fn url_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() + 1 && i + 2 <= bytes.len() - 1 + 1 {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if i + 2 < bytes.len() {
                if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn known_desktop() -> Option<PathBuf> {
    use windows::Win32::UI::Shell::KNOWN_FOLDER_FLAG;
    unsafe {
        use windows::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_Desktop};
        let path = SHGetKnownFolderPath(&FOLDERID_Desktop, KNOWN_FOLDER_FLAG(0), None).ok()?;
        let s = windows::core::PWSTR(path.0).to_string().ok()?;
        Some(PathBuf::from(s))
    }
}