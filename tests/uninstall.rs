use std::fs;
use std::os::unix::fs::symlink;

use tempfile::TempDir;
use xrpkg::capability::Version;
use xrpkg::solver::SelectedPackage;
use xrpkg::state::{LocalDb, PackageInstall};
use xrpkg::uninstall::remove_package;
use xrpkg::xpkg::Rootfs;

fn install(name: &str, files: &[&str]) -> PackageInstall {
    PackageInstall {
        package: SelectedPackage {
            name: name.to_string(),
            version: Version::parse("1.0").unwrap(),
        },
        version_raw: "1.0".to_string(),
        origin: "native".to_string(),
        files: files.iter().map(|file| file.to_string()).collect(),
        hooks: Vec::new(),
    }
}

fn write_file(rootfs: &std::path::Path, relative: &str, contents: &[u8]) {
    let path = rootfs.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn create_rootfs(temp: &TempDir) -> Rootfs {
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    write_file(rootfs.path(), "usr/lib/libz.so.1.3.1", b"zlib");
    fs::create_dir_all(rootfs.path().join("usr/lib")).unwrap();
    symlink("libz.so.1.3.1", rootfs.path().join("usr/lib/libz.so.1")).unwrap();
    symlink("libz.so.1", rootfs.path().join("usr/lib/libz.so")).unwrap();
    write_file(rootfs.path(), "usr/share/shared/a.txt", b"a");
    write_file(rootfs.path(), "usr/share/shared/b.txt", b"b");
    write_file(rootfs.path(), "usr/share/shared/common.txt", b"common");
    write_file(rootfs.path(), "usr/share/only-a/a.txt", b"only-a");
    rootfs
}

#[test]
fn removes_files_symlinks_and_empty_dirs_but_preserves_shared_paths() {
    let temp = TempDir::new().unwrap();
    let rootfs = create_rootfs(&temp);
    let mut db = LocalDb::open(":memory:").unwrap();
    db.commit(&[
        install(
            "a",
            &[
                "usr/lib/libz.so.1.3.1",
                "usr/lib/libz.so.1",
                "usr/lib/libz.so",
                "usr/share/shared/a.txt",
                "usr/share/shared/common.txt",
                "usr/share/only-a/a.txt",
            ],
        ),
        install(
            "b",
            &["usr/share/shared/b.txt", "usr/share/shared/common.txt"],
        ),
    ])
    .unwrap();

    let report = remove_package(&mut db, &rootfs, "a", false).unwrap();

    assert_eq!(report.removed_files, 5);
    assert_eq!(report.skipped_shared, 1);
    assert!(!rootfs.path().join("usr/lib").exists());
    assert!(!rootfs.path().join("usr/share/only-a").exists());
    assert!(!rootfs.path().join("usr/share/shared/a.txt").exists());
    assert!(rootfs.path().join("usr/share/shared/b.txt").is_file());
    assert!(rootfs.path().join("usr/share/shared/common.txt").is_file());
    assert!(!db.package_exists("a").unwrap());
    assert!(db.package_exists("b").unwrap());

    let report = remove_package(&mut db, &rootfs, "b", false).unwrap();

    assert_eq!(report.removed_files, 2);
    assert!(!rootfs.path().join("usr/share/shared").exists());
    assert!(!db.package_exists("b").unwrap());
    assert!(db.list_installed().unwrap().is_empty());
}

#[test]
fn stops_before_database_cleanup_when_a_file_cannot_be_removed() {
    let temp = TempDir::new().unwrap();
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    // 数据库把目录记录成文件，remove_file 会失败。
    fs::create_dir_all(rootfs.path().join("protected")).unwrap();
    let mut db = LocalDb::open(":memory:").unwrap();
    db.commit(&[install("broken", &["protected"])]).unwrap();

    let error = remove_package(&mut db, &rootfs, "broken", false).unwrap_err();

    assert!(error.contains("目录"), "{error}");
    assert!(db.package_exists("broken").unwrap());
    assert_eq!(db.list_files("broken").unwrap(), ["protected"]);
    assert!(rootfs.path().join("protected").is_dir());
}
