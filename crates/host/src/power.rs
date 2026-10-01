//! M3-3：电源动作执行（host 侧）。
//!
//! Lock = LockWorkStation（同会话交互进程，直接有效）；
//! Sleep = SetSuspendState（睡眠；若系统开了休眠且睡眠不可用可能转休眠）；
//! Shutdown/Restart = InitiateSystemShutdownExW（需 SeShutdownPrivilege，
//! 先在自身令牌上启用——常规交互令牌里该特权默认存在但禁用）。
//!
//! Sleep/Shutdown/Restart 后连接随之断开：Restart 依赖 M2.5 的登录自启
//! + client 自动重连完成自愈闭环。

use rdlink_proto::PowerActionKind;

/// 启用当前进程令牌的 SeShutdownPrivilege（SetSuspendState/InitiateSystemShutdown 需要）
fn enable_shutdown_privilege() -> Result<(), String> {
    use windows::Win32::Foundation::LUID;
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_SHUTDOWN_NAME,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(|e| format!("OpenProcessToken: {e}"))?;

        let mut luid = LUID::default();
        LookupPrivilegeValueW(None, SE_SHUTDOWN_NAME, &mut luid)
            .map_err(|e| format!("LookupPrivilegeValueW: {e}"))?;

        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: windows::Win32::Security::SE_PRIVILEGE_ENABLED,
            }],
        };
        let r = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None);
        let _ = windows::Win32::Foundation::CloseHandle(token);
        // ERROR_NOT_ALL_ASSIGNED(1300) = 令牌里没有该特权 → 后续调用会失败，报出来
        r.map_err(|e| format!("AdjustTokenPrivileges: {e}"))?;
        Ok(())
    }
}

/// 执行电源动作。Err = 执行失败（连接/协议层不受影响）。
pub fn execute(action: PowerActionKind) -> Result<(), String> {
    use windows::Win32::System::Power::SetSuspendState;
    use windows::Win32::System::Shutdown::{
        InitiateSystemShutdownExW, LockWorkStation, SHTDN_REASON_FLAG_PLANNED,
    };
    match action {
        PowerActionKind::Lock => unsafe {
            LockWorkStation().map_err(|e| format!("LockWorkStation: {e}"))
        },
        PowerActionKind::Sleep => {
            enable_shutdown_privilege()?;
            unsafe {
                // (休眠=false, 强制=false, 禁止唤醒事件=false)
                if SetSuspendState(false, false, false) {
                    Ok(())
                } else {
                    Err("SetSuspendState 返回失败（系统可能拒绝睡眠）".into())
                }
            }
        }
        PowerActionKind::Shutdown => {
            enable_shutdown_privilege()?;
            unsafe {
                InitiateSystemShutdownExW(
                    None,
                    windows::core::w!("rdlink 远程关机"),
                    0,
                    true,
                    false,
                    SHTDN_REASON_FLAG_PLANNED,
                )
                .map_err(|e| format!("InitiateSystemShutdownExW: {e}"))
            }
        }
        PowerActionKind::Restart => {
            enable_shutdown_privilege()?;
            unsafe {
                InitiateSystemShutdownExW(
                    None,
                    windows::core::w!("rdlink 远程重启"),
                    0,
                    true,
                    true,
                    SHTDN_REASON_FLAG_PLANNED,
                )
                .map_err(|e| format!("InitiateSystemShutdownExW: {e}"))
            }
        }
    }
}
