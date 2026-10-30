//! OpenPGP 分离签名与验证。
//!
//! 为了避免引入难以编译的 OpenPGP crate，本模块通过系统 `gpg` 命令行执行
//! detached signature；签名者和验证者都使用标准 OpenPGP 文件格式。

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 返回 `file.sig` 的路径。
pub fn signature_path(file: &Path) -> PathBuf {
    let mut name = OsString::from(file.as_os_str());
    name.push(".sig");
    PathBuf::from(name)
}

/// 使用指定私钥生成 `file.sig` 分离签名。
///
/// 私钥位置由调用者的 `GNUPGHOME` 决定，与 gpg 命令行行为一致。
pub fn sign_detached(key_id: &str, file: &Path) -> Result<PathBuf, String> {
    if !file.is_file() {
        return Err(format!("待签名文件不存在: {}", file.display()));
    }
    if key_id.trim().is_empty() {
        return Err("签名 key id 不能为空".to_string());
    }
    let signature = signature_path(file);
    let _ = fs::remove_file(&signature);
    let output = Command::new("gpg")
        .args([
            "--batch",
            "--no-tty",
            "--yes",
            "--detach-sign",
            "--local-user",
            key_id,
            "--output",
        ])
        .arg(&signature)
        .arg(file)
        .env("LC_ALL", "C")
        .output()
        .map_err(|error| format!("执行 gpg 签名失败: {error}"))?;
    if !output.status.success() {
        let _ = fs::remove_file(&signature);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("签名 `{}` 失败: {}", file.display(), stderr.trim()));
    }
    Ok(signature)
}

/// 用指定公钥环验证 detached signature。
///
/// 只有 gpg 同时输出 GOODSIG 和 VALIDSIG 才认为通过；这样即使进程退出码
/// 因环境差异为 0，也不会把“没有有效签名”误判为成功。
pub fn verify_detached(keyring: &Path, signature: &Path, file: &Path) -> Result<(), String> {
    if !keyring.is_file() {
        return Err(format!("信任公钥环不存在: {}", keyring.display()));
    }
    if !signature.is_file() {
        return Err(format!("签名文件不存在: {}", signature.display()));
    }
    if !file.is_file() {
        return Err(format!("待验证文件不存在: {}", file.display()));
    }
    let home = std::env::temp_dir().join(format!(
        "xrpkg-gpg-verify-{}-{}",
        std::process::id(),
        nonce()
    ));
    fs::create_dir(&home).map_err(|error| format!("创建临时 GPG 验证目录失败: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&home, fs::Permissions::from_mode(0o700));
    }
    let output = Command::new("gpg")
        .args(["--homedir"])
        .arg(&home)
        .args(["--batch", "--no-tty", "--no-default-keyring", "--keyring"])
        .arg(keyring)
        .args(["--status-fd", "1", "--verify"])
        .arg(signature)
        .arg(file)
        .env("LC_ALL", "C")
        .output();
    let _ = fs::remove_dir_all(&home);
    let output = output.map_err(|error| format!("执行 gpg 验证失败: {error}"))?;
    let status = String::from_utf8_lossy(&output.stdout);
    let good = status.lines().any(|line| line.contains("GOODSIG"));
    let valid = status.lines().any(|line| line.contains("VALIDSIG"));
    if output.status.success() && good && valid {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "签名验证失败 `{}`: {}",
            file.display(),
            stderr.trim()
        ))
    }
}
fn nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}
