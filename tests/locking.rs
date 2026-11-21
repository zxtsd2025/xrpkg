//! 并发进程锁的端到端测试：父进程持锁时，第二个 xrpkg 必须快速失败。

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;
use xrpkg::lock::ProcessLock;

/// 在固定中文环境下运行 xrpkg，避免宿主 locale 影响错误文案。
fn run_init(rootfs: &Path, database: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xrpkg"));
    for key in ["LC_ALL", "LC_MESSAGES", "LC_CTYPE", "LANG", "XRPKG_LANG"] {
        command.env_remove(key);
    }
    command
        .env("XRPKG_LANG", "zh_CN.UTF-8")
        .env("TERM", "dumb")
        .args(["init", "--root"])
        .arg(rootfs)
        .arg("--db")
        .arg(database)
        .output()
        .unwrap()
}

#[test]
fn second_process_fails_fast_while_lock_is_held() {
    let temp = TempDir::new().unwrap();
    let rootfs = temp.path().join("root");
    let database = temp.path().join("local.db");

    let lock = ProcessLock::acquire(&rootfs, &database).unwrap();
    let blocked = run_init(&rootfs, &database);

    assert!(!blocked.status.success(), "持锁时第二个进程不应成功");
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert!(stderr.contains("另一个 xrpkg 进程"), "{stderr}");
    // 获取锁在任何写入之前，失败时连 Rootfs 目录都不应该创建。
    assert!(!rootfs.exists(), "失败进程不得产生部分写入");

    drop(lock);
    let allowed = run_init(&rootfs, &database);
    assert!(
        allowed.status.success(),
        "释放锁后应成功: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert!(rootfs.join("etc/passwd").is_file());
}

#[test]
fn different_targets_can_proceed_in_parallel() {
    let temp = TempDir::new().unwrap();
    let first_root = temp.path().join("root-a");
    let second_root = temp.path().join("root-b");

    let _first = ProcessLock::acquire(&first_root, &temp.path().join("a.db")).unwrap();
    let second = run_init(&second_root, &temp.path().join("b.db"));

    assert!(
        second.status.success(),
        "不同目标不应互相阻塞: {}",
        String::from_utf8_lossy(&second.stderr)
    );
}
