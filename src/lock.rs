//! 目标 Rootfs 与数据库的进程级排他锁。
//!
//! 锁文件放在运行时目录（`XRPKG_LOCK_DIR`、`XDG_RUNTIME_DIR/xrpkg/locks` 或
//! `/tmp/xrpkg-locks-<uid>`），文件名由 Rootfs 和数据库路径共同哈希得到。
//! 使用 `flock(2)` 而不是 `O_EXCL`：进程崩溃后内核会自动释放锁，不会留下
//! 永久阻塞的残留文件。
//!
//! 只有会写入目标 Rootfs 或数据库的命令才需要这把锁；`list`、`search`、
//! `help`、`version` 等只读命令不获取锁。

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::ui;

/// FNV-1a 64 位偏移基数；这里只用于生成稳定的锁文件名。
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64 位质数。
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 一个已获取的排他锁；离开作用域时释放。
#[derive(Debug)]
pub struct ProcessLock {
    file: File,
}

impl ProcessLock {
    /// 获取 `rootfs` + `database` 组合的排他锁。
    ///
    /// 同一组合已被其他进程持有时立即失败，不等待；不同组合互不影响。
    pub fn acquire(rootfs: &Path, database: &Path) -> Result<Self, String> {
        let directory = lock_directory();
        fs::create_dir_all(&directory)
            .map_err(|error| format!("创建锁目录 `{}` 失败: {error}", directory.display()))?;
        ensure_private_directory(&directory)?;

        let path = directory.join(format!("{}.lock", identity(rootfs, database)));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("打开锁文件 `{}` 失败: {error}", path.display()))?;
        // 锁文件只保存进程协调状态，不保存敏感信息；仍然限制为当前用户可读写。
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));

        // SAFETY: `file` 在调用期间保持打开，fd 有效；flock 只影响该 fd
        // 对应的打开文件描述，失败时立即返回错误码。
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(ui::pick(
                    format!(
                        "另一个 xrpkg 进程正在操作同一目标（Rootfs {}，数据库 {}）；请等待它结束后重试",
                        rootfs.display(),
                        database.display()
                    ),
                    format!(
                        "Another xrpkg process is already operating on this target (rootfs {}, database {}); wait for it to exit and retry",
                        rootfs.display(),
                        database.display()
                    ),
                ));
            }
            return Err(format!("获取目标锁 `{}` 失败: {error}", path.display()));
        }

        Ok(Self { file })
    }
}

impl Drop for ProcessLock {
    /// 显式解锁；即使这里失败，关闭 fd 也会释放 flock。
    fn drop(&mut self) {
        // SAFETY: fd 仍由 `self.file` 持有且有效。
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// 锁目录优先级：显式环境变量 > XDG 运行时目录 > 按 uid 隔离的临时目录。
fn lock_directory() -> PathBuf {
    if let Some(directory) = std::env::var_os("XRPKG_LOCK_DIR") {
        if !directory.is_empty() {
            return PathBuf::from(directory);
        }
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !runtime.is_empty() {
            return PathBuf::from(runtime).join("xrpkg/locks");
        }
    }
    // SAFETY: geteuid 只读取进程凭据，不修改任何状态。
    let uid = unsafe { libc::geteuid() };
    std::env::temp_dir().join(format!("xrpkg-locks-{uid}"))
}

/// 确认锁目录属于当前用户，避免在共享 /tmp 下误用他人预建的目录。
fn ensure_private_directory(directory: &Path) -> Result<(), String> {
    let metadata = fs::metadata(directory)
        .map_err(|error| format!("读取锁目录 `{}` 失败: {error}", directory.display()))?;
    if !metadata.is_dir() {
        return Err(format!("锁路径 `{}` 不是目录", directory.display()));
    }
    // SAFETY: geteuid 只读取进程凭据。
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid {
        return Err(format!(
            "锁目录 `{}` 的属主是 {}，当前用户是 {uid}；拒绝使用",
            directory.display(),
            metadata.uid()
        ));
    }
    Ok(())
}

/// 锁身份：Rootfs 与数据库的规范路径；任一变化都会得到不同的锁文件。
fn identity(rootfs: &Path, database: &Path) -> String {
    let mut hash = FNV_OFFSET_BASIS;
    for path in [normalize(rootfs), normalize(database)] {
        for byte in path.as_os_str().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        // 用 0 分隔两个路径，避免 ("/a","/b") 与 ("/ab","") 之类的拼接歧义。
        hash ^= 0;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

/// 优先使用解析过符号链接的规范路径；路径不存在时退回到绝对路径。
fn normalize(path: &Path) -> PathBuf {
    fs::canonicalize(path)
        .unwrap_or_else(|_| std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn identity_is_stable_and_distinguishes_targets() {
        let root_a = Path::new("/tmp/xrpkg-lock-a");
        let root_b = Path::new("/tmp/xrpkg-lock-b");
        let db = Path::new("/tmp/xrpkg-lock.db");

        assert_eq!(identity(root_a, db), identity(root_a, db));
        assert_ne!(identity(root_a, db), identity(root_b, db));
        assert_ne!(
            identity(root_a, db),
            identity(root_a, Path::new("/tmp/other.db"))
        );
    }

    #[test]
    fn same_target_is_exclusive_within_one_process() {
        let temp = TempDir::new().unwrap();
        let rootfs = temp.path().join("rootfs");
        let database = temp.path().join("local.db");

        let first = ProcessLock::acquire(&rootfs, &database).unwrap();
        let error = ProcessLock::acquire(&rootfs, &database).unwrap_err();
        assert!(error.contains("另一个 xrpkg 进程"), "{error}");

        drop(first);
        ProcessLock::acquire(&rootfs, &database).unwrap();
    }

    #[test]
    fn different_targets_do_not_block_each_other() {
        let temp = TempDir::new().unwrap();
        let _first =
            ProcessLock::acquire(&temp.path().join("root-a"), &temp.path().join("a.db")).unwrap();
        let _second =
            ProcessLock::acquire(&temp.path().join("root-b"), &temp.path().join("b.db")).unwrap();
    }
}
