//! M3-4：进程管理（host 侧）。
//!
//! 列表：sysinfo 双采样（间隔 300ms）取 CPU%——首次采样无历史值，第二次
//! 刷新后 cpu_usage() 才有意义。按 CPU 降序返回。
//! 结束：Windows kill（TerminateProcess），系统级进程会被拒绝并回原因。

use sysinfo::System;

/// 双采样取进程列表（阻塞 ~300ms，控制循环内调用可接受）
pub fn list() -> Vec<rdlink_proto::ProcEntry> {
    let mut sys = System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    std::thread::sleep(std::time::Duration::from_millis(300));
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, false);

    let mut out: Vec<rdlink_proto::ProcEntry> = sys
        .processes()
        .values()
        .map(|p| rdlink_proto::ProcEntry {
            pid: p.pid().as_u32(),
            name: p.name().to_string_lossy().to_string(),
            cpu: p.cpu_usage(),
            mem_mb: p.memory() as f64 / 1048576.0,
        })
        .collect();
    out.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// 结束进程。Ok(()) = 已发送终止；Err = 失败原因。
pub fn kill(pid: u32) -> Result<(), String> {
    use sysinfo::Pid;
    let mut sys = System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]), true);
    match sys.process(Pid::from_u32(pid)) {
        Some(p) => {
            if p.kill() {
                Ok(())
            } else {
                Err("系统拒绝终止（权限不足或受保护进程）".into())
            }
        }
        None => Err(format!("进程 {pid} 不存在或已退出")),
    }
}
