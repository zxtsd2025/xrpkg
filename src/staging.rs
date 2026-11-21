//! 安装暂存目录：原生 `.xpkg` 与兼容轨道共用的临时解包根。
//!
//! 每次安装创建一个独立目录，解包、ABI 校验和 hook 都只写这里；提交阶段
//! 再从暂存区发布到目标 Rootfs。`Drop` 会自动删除目录，因此错误返回和
//! panic 都不会留下半成品。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 一次安装的暂存目录。
pub struct StagingDir {
    path: PathBuf,
}

impl StagingDir {
    /// 在系统临时目录下创建唯一子目录。
    pub fn create() -> Result<Self, String> {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        for attempt in 0u128..8 {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| format!("读取系统时间失败: {error}"))?
                .as_nanos()
                + attempt;
            let path = base.join(format!("xrpkg-stage-{pid}-{nonce}"));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!("创建暂存目录 `{}` 失败: {error}", path.display()))
                }
            }
        }
        Err("创建暂存目录失败：连续名称冲突".to_string())
    }

    /// 返回暂存目录路径。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagingDir {
    /// 删除暂存目录；ABI 校验或 hook 失败时这就是回滚。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
