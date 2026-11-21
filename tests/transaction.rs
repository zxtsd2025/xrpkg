//! 文件事务：原子替换、升级/卸载快照和断电恢复。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tempfile::TempDir;
use xrpkg::capability::Version;
use xrpkg::solver::SelectedPackage;
use xrpkg::state::{LocalDb, PackageInstall};
use xrpkg::transaction::Transaction;
use xrpkg::xpkg::Rootfs;

fn package_install(name: &str, version: &str) -> PackageInstall {
    PackageInstall {
        package: SelectedPackage {
            name: name.to_string(),
            version: Version::parse(version).unwrap(),
        },
        version_raw: version.to_string(),
        origin: "native".to_string(),
        files: vec!["bin/a".to_string()],
        hooks: Vec::new(),
    }
}

fn write(path: &Path, contents: &str, mode: u32) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn automatic_rollback_restores_content_mode_and_xattrs() {
    let temp = TempDir::new().unwrap();
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    fs::create_dir_all(rootfs.path().join("bin")).unwrap();
    let old = rootfs.path().join("bin/a");
    write(&old, "old\n", 0o4755);
    xattr::set(&old, "user.xrpkg.rollback", b"old-value").unwrap();

    let new = temp.path().join("new-a");
    write(&new, "new\n", 0o644);
    xattr::set(&new, "user.xrpkg.rollback", b"new-value").unwrap();

    let db = LocalDb::open(":memory:").unwrap();
    {
        let mut transaction = Transaction::begin(&rootfs, Some(&db)).unwrap();
        transaction
            .record_install("pkg-a", "2.0", "native")
            .unwrap();
        transaction.install_file(Path::new("bin/a"), &new).unwrap();
        // 未提交离开作用域 -> 自动回滚。
    }

    assert_eq!(fs::read_to_string(&old).unwrap(), "old\n");
    assert_eq!(
        fs::metadata(&old).unwrap().permissions().mode() & 0o7777,
        0o4755
    );
    assert_eq!(
        xattr::get(&old, "user.xrpkg.rollback").unwrap(),
        Some(b"old-value".to_vec())
    );
    assert!(db.list_installed().unwrap().is_empty());
}

#[test]
fn commit_keeps_new_file_and_removes_snapshot_directory() {
    let temp = TempDir::new().unwrap();
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    fs::create_dir_all(rootfs.path().join("bin")).unwrap();
    write(&rootfs.path().join("bin/a"), "old\n", 0o644);
    let new = temp.path().join("new-a");
    write(&new, "new\n", 0o644);

    let mut db = LocalDb::open(":memory:").unwrap();
    let mut transaction = Transaction::begin(&rootfs, Some(&db)).unwrap();
    transaction
        .record_install("pkg-a", "2.0", "native")
        .unwrap();
    transaction.install_file(Path::new("bin/a"), &new).unwrap();
    db.commit(&[package_install("pkg-a", "2.0")]).unwrap();
    transaction.commit();

    assert_eq!(
        fs::read_to_string(rootfs.path().join("bin/a")).unwrap(),
        "new\n"
    );
    let txn_root = rootfs.path().join(".xrpkg-txn");
    assert!(!txn_root.exists() || fs::read_dir(txn_root).unwrap().count() == 0);
}

#[test]
fn crash_before_database_commit_rolls_back_on_next_transaction() {
    let temp = TempDir::new().unwrap();
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    fs::create_dir_all(rootfs.path().join("bin")).unwrap();
    write(&rootfs.path().join("bin/a"), "old\n", 0o644);
    let new = temp.path().join("new-a");
    write(&new, "new\n", 0o644);

    let db = LocalDb::open(":memory:").unwrap();
    {
        let mut transaction = Transaction::begin(&rootfs, Some(&db)).unwrap();
        transaction
            .record_install("pkg-a", "2.0", "native")
            .unwrap();
        transaction.install_file(Path::new("bin/a"), &new).unwrap();
        std::mem::forget(transaction); // 模拟进程在 rename 后、数据库提交前崩溃
    }

    let transaction = Transaction::begin(&rootfs, Some(&db)).unwrap();
    assert_eq!(
        fs::read_to_string(rootfs.path().join("bin/a")).unwrap(),
        "old\n"
    );
    transaction.commit();
}

#[test]
fn crash_after_database_commit_keeps_new_file_and_cleans_up() {
    let temp = TempDir::new().unwrap();
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    fs::create_dir_all(rootfs.path().join("bin")).unwrap();
    write(&rootfs.path().join("bin/a"), "old\n", 0o644);
    let new = temp.path().join("new-a");
    write(&new, "new\n", 0o644);

    let mut db = LocalDb::open(":memory:").unwrap();
    {
        let mut transaction = Transaction::begin(&rootfs, Some(&db)).unwrap();
        transaction
            .record_install("pkg-a", "2.0", "native")
            .unwrap();
        transaction.install_file(Path::new("bin/a"), &new).unwrap();
        db.commit(&[package_install("pkg-a", "2.0")]).unwrap();
        std::mem::forget(transaction); // 模拟数据库已提交但清理前崩溃
    }

    let transaction = Transaction::begin(&rootfs, Some(&db)).unwrap();
    assert_eq!(
        fs::read_to_string(rootfs.path().join("bin/a")).unwrap(),
        "new\n"
    );
    transaction.commit();
    let txn_root = rootfs.path().join(".xrpkg-txn");
    assert!(!txn_root.exists() || fs::read_dir(txn_root).unwrap().count() == 0);
}

#[test]
fn removed_file_is_restored_when_transaction_drops() {
    let temp = TempDir::new().unwrap();
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    fs::create_dir_all(rootfs.path().join("bin")).unwrap();
    let file = rootfs.path().join("bin/a");
    write(&file, "keep\n", 0o2755);

    let db = LocalDb::open(":memory:").unwrap();
    {
        let mut transaction = Transaction::begin(&rootfs, Some(&db)).unwrap();
        transaction.record_remove("pkg-a").unwrap();
        assert!(transaction.remove_file(Path::new("bin/a")).unwrap());
        assert!(!file.exists());
    }

    assert_eq!(fs::read_to_string(&file).unwrap(), "keep\n");
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
        0o2755
    );
}
