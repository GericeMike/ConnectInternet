//! M3-5：密码认证管理（host 侧）——`host --set-password <密码>`。
//!
//! K = argon2id(password, salt)（32B）；salt 随机 16B。两者以 hex 写入
//! rdlink.local.toml [host]（git 忽略的本机覆盖文件——**K 等效密码，勿外传**）。
//! 删除这两行即可关闭认证。

use argon2::Argon2;

/// 计算并写入 rdlink.local.toml。返回 (salt_hex, key_hex)。
pub fn set_password(password: &str) -> Result<(String, String), String> {
    // 盐走 OS CSPRNG（getrandom）
    let mut salt = [0u8; 16];
    getrandom::getrandom(&mut salt).map_err(|e| format!("随机数生成失败: {e}"))?;
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), &salt, &mut key)
        .map_err(|e| format!("KDF 失败: {e}"))?;
    let salt_hex = hex::encode(salt);
    let key_hex = hex::encode(key);
    write_local_config(&salt_hex, &key_hex)?;
    Ok((salt_hex, key_hex))
}

fn write_local_config(salt_hex: &str, key_hex: &str) -> Result<(), String> {
    let path = std::path::Path::new("rdlink.local.toml");
    let mut lines: Vec<String> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .filter(|l| !l.trim_start().starts_with("auth_salt") && !l.trim_start().starts_with("auth_key"))
        .collect();
    match lines.iter().position(|l| l.trim() == "[host]") {
        Some(i) => {
            // 插到 [host] 段第一行位置（段内追加）
            lines.insert(i + 1, format!("auth_salt = \"{salt_hex}\""));
            lines.insert(i + 2, format!("auth_key = \"{key_hex}\""));
        }
        None => {
            lines.push("[host]".into());
            lines.push(format!("auth_salt = \"{salt_hex}\""));
            lines.push(format!("auth_key = \"{key_hex}\""));
        }
    }
    std::fs::write(path, lines.join("\r\n") + "\r\n").map_err(|e| format!("写入失败: {e}"))
}
