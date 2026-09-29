//! winit 物理键 → Windows 虚拟键码（VK）映射。
//!
//! 用 `physical_key`（与键盘布局无关，等价扫描码语义）映射到标准美式布局 VK——
//! 远程控制的标准做法：被控端按 VK 注入，文字输入走 `UnicodeChar` 兜底（IME 是 M4 议题）。

use winit::keyboard::{KeyCode, PhysicalKey};

/// winit 物理键 → Windows VK。无法映射的键返回 None（如媒体键杂项）。
pub fn vk_from_key(physical: PhysicalKey) -> Option<u16> {
    let PhysicalKey::Code(pc) = physical else {
        return None; // Unidentified
    };
    Some(match pc {
        // 字母 A-Z
        KeyCode::KeyA => 0x41, KeyCode::KeyB => 0x42, KeyCode::KeyC => 0x43,
        KeyCode::KeyD => 0x44, KeyCode::KeyE => 0x45, KeyCode::KeyF => 0x46,
        KeyCode::KeyG => 0x47, KeyCode::KeyH => 0x48, KeyCode::KeyI => 0x49,
        KeyCode::KeyJ => 0x4A, KeyCode::KeyK => 0x4B, KeyCode::KeyL => 0x4C,
        KeyCode::KeyM => 0x4D, KeyCode::KeyN => 0x4E, KeyCode::KeyO => 0x4F,
        KeyCode::KeyP => 0x50, KeyCode::KeyQ => 0x51, KeyCode::KeyR => 0x52,
        KeyCode::KeyS => 0x53, KeyCode::KeyT => 0x54, KeyCode::KeyU => 0x55,
        KeyCode::KeyV => 0x56, KeyCode::KeyW => 0x57, KeyCode::KeyX => 0x58,
        KeyCode::KeyY => 0x59, KeyCode::KeyZ => 0x5A,
        // 数字 0-9
        KeyCode::Digit0 => 0x30, KeyCode::Digit1 => 0x31, KeyCode::Digit2 => 0x32,
        KeyCode::Digit3 => 0x33, KeyCode::Digit4 => 0x34, KeyCode::Digit5 => 0x35,
        KeyCode::Digit6 => 0x36, KeyCode::Digit7 => 0x37, KeyCode::Digit8 => 0x38,
        KeyCode::Digit9 => 0x39,
        // 功能键 F1-F24
        KeyCode::F1 => 0x70, KeyCode::F2 => 0x71, KeyCode::F3 => 0x72, KeyCode::F4 => 0x73,
        KeyCode::F5 => 0x74, KeyCode::F6 => 0x75, KeyCode::F7 => 0x76, KeyCode::F8 => 0x77,
        KeyCode::F9 => 0x78, KeyCode::F10 => 0x79, KeyCode::F11 => 0x7A, KeyCode::F12 => 0x7B,
        KeyCode::F13 => 0x7C, KeyCode::F14 => 0x7D, KeyCode::F15 => 0x7E, KeyCode::F16 => 0x7F,
        KeyCode::F17 => 0x80, KeyCode::F18 => 0x81, KeyCode::F19 => 0x82, KeyCode::F20 => 0x83,
        KeyCode::F21 => 0x84, KeyCode::F22 => 0x85, KeyCode::F23 => 0x86, KeyCode::F24 => 0x87,
        // 修饰键（右 Ctrl/Alt 用可区分 VK，配合被控端扩展键白名单；左右 Shift 合并 0x10——
        // SendInput 按 VK_RSHIFT(0xA1) 注入不可靠，M1 已知限制）
        KeyCode::ShiftLeft | KeyCode::ShiftRight => 0x10,
        KeyCode::ControlLeft => 0x11, KeyCode::ControlRight => 0xA3,
        KeyCode::AltLeft => 0x12, KeyCode::AltRight => 0xA5,
        KeyCode::SuperLeft => 0x5B, KeyCode::SuperRight => 0x5C,
        // 编辑/导航区
        KeyCode::Escape => 0x1B, KeyCode::Tab => 0x09, KeyCode::Space => 0x20,
        KeyCode::Backspace => 0x08, KeyCode::Enter => 0x0D,
        KeyCode::Insert => 0x2D, KeyCode::Delete => 0x2E,
        KeyCode::Home => 0x24, KeyCode::End => 0x23,
        KeyCode::PageUp => 0x21, KeyCode::PageDown => 0x22,
        KeyCode::ArrowUp => 0x26, KeyCode::ArrowDown => 0x28,
        KeyCode::ArrowLeft => 0x25, KeyCode::ArrowRight => 0x27,
        KeyCode::CapsLock => 0x14, KeyCode::PrintScreen => 0x2C,
        KeyCode::ScrollLock => 0x91, KeyCode::Pause => 0x13,
        // 主区标点
        KeyCode::Backquote => 0xC0, KeyCode::Minus => 0xBD, KeyCode::Equal => 0xBB,
        KeyCode::BracketLeft => 0xDB, KeyCode::BracketRight => 0xDD, KeyCode::Backslash => 0xDC,
        KeyCode::Semicolon => 0xBA, KeyCode::Quote => 0xDE,
        KeyCode::Comma => 0xBC, KeyCode::Period => 0xBE, KeyCode::Slash => 0xBF,
        KeyCode::IntlBackslash => 0xE2, KeyCode::IntlRo => 0xE1,
        // 小键盘
        KeyCode::NumLock => 0x90,
        KeyCode::NumpadDivide => 0x6F, KeyCode::NumpadMultiply => 0x6A,
        KeyCode::NumpadSubtract => 0x6D, KeyCode::NumpadAdd => 0x6B,
        KeyCode::NumpadEnter => 0x0D, KeyCode::NumpadDecimal => 0x2E,
        KeyCode::Numpad0 => 0x60, KeyCode::Numpad1 => 0x61, KeyCode::Numpad2 => 0x62,
        KeyCode::Numpad3 => 0x63, KeyCode::Numpad4 => 0x64, KeyCode::Numpad5 => 0x65,
        KeyCode::Numpad6 => 0x66, KeyCode::Numpad7 => 0x67, KeyCode::Numpad8 => 0x68,
        KeyCode::Numpad9 => 0x69,
        // 未列举键（媒体键/输入法专用键等杂项）不注入，走兜底
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::PhysicalKey;

    #[test]
    fn spot_checks() {
        assert_eq!(vk_from_key(PhysicalKey::Code(KeyCode::KeyA)), Some(0x41));
        assert_eq!(vk_from_key(PhysicalKey::Code(KeyCode::KeyZ)), Some(0x5A));
        assert_eq!(vk_from_key(PhysicalKey::Code(KeyCode::Digit0)), Some(0x30));
        assert_eq!(vk_from_key(PhysicalKey::Code(KeyCode::F12)), Some(0x7B));
        assert_eq!(vk_from_key(PhysicalKey::Code(KeyCode::ArrowLeft)), Some(0x25));
        // 右 Ctrl/Alt 走可区分 VK（被控端 EXTENDED_VKS 含 0xA3/0xA5）
        assert_eq!(vk_from_key(PhysicalKey::Code(KeyCode::ControlRight)), Some(0xA3));
        assert_eq!(vk_from_key(PhysicalKey::Code(KeyCode::AltRight)), Some(0xA5));
        assert_eq!(vk_from_key(PhysicalKey::Code(KeyCode::Numpad5)), Some(0x65));
        // 未知
        assert_eq!(vk_from_key(PhysicalKey::Code(KeyCode::MediaPlayPause)), None);
    }
}
