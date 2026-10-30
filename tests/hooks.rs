use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use xrpkg::builder::build_package;
use xrpkg::capability::Version;
use xrpkg::installer::install_changes;
use xrpkg::solver::SelectedPackage;
use xrpkg::state::LocalDb;

fn build_hooked_package(temp: &TempDir, name: &str, post_install: Option<&str>) -> PathBuf {
    let source = temp.path().join(format!("{name}-src"));
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("payload.txt"), b"hook payload\n").unwrap();
    if let Some(script) = post_install {
        fs::write(temp.path().join(format!("{name}-post.sh")), script).unwrap();
    }
    let recipe = temp.path().join(format!("{name}.toml"));
    let hooks = if post_install.is_some() {
        format!("[hooks]\npost_install = \"{name}-post.sh\"\n")
    } else {
        String::new()
    };
    fs::write(
        &recipe,
        format!(
            r#"
name = "{name}"
version = "1.0"
source = "{name}-src"
requires = []
build = []
package = [
  "install -Dm644 payload.txt \"$PKGDIR/usr/share/hook-test/{name}.txt\"",
]
{hooks}
"#
        ),
    )
    .unwrap();
    let output = temp.path().join(format!("out-{name}"));
    build_package(&recipe, &output).unwrap()
}

fn install_one(temp: &TempDir, name: &str, archive: &Path, no_hooks: bool) -> Result<(), String> {
    let mut db = LocalDb::open(":memory:").unwrap();
    let mut archives = HashMap::new();
    archives.insert(
        (name.to_string(), Version::parse("1.0").unwrap()),
        archive.to_path_buf(),
    );
    let changes = vec![SelectedPackage {
        name: name.to_string(),
        version: Version::parse("1.0").unwrap(),
    }];
    let rootfs = temp.path().join(format!("rootfs-{name}"));
    let result = install_changes(&mut db, &changes, &archives, &rootfs, no_hooks, false);
    if result.is_ok() {
        assert!(db
            .list_files(name)
            .unwrap()
            .contains(&format!("usr/share/hook-test/{name}.txt")));
        assert!(!db.list_hooks(name).unwrap().is_empty());
    } else {
        assert!(db.list_installed().unwrap().is_empty());
    }
    result
}

#[test]
fn post_install_runs_inside_the_target_rootfs() {
    let temp = TempDir::new().unwrap();
    let script =
        "mkdir -p \"$XRPKG_ROOTFS/etc\"\necho configured > \"$XRPKG_ROOTFS/etc/marker.txt\"\n";
    let archive = build_hooked_package(&temp, "hook-ok", Some(script));

    install_one(&temp, "hook-ok", &archive, false).unwrap();

    assert_eq!(
        fs::read_to_string(temp.path().join("rootfs-hook-ok/etc/marker.txt")).unwrap(),
        "configured\n"
    );
}

#[test]
fn failed_post_install_rolls_back_files_and_database() {
    let temp = TempDir::new().unwrap();
    let archive = build_hooked_package(&temp, "hook-fail", Some("exit 1\n"));

    let error = install_one(&temp, "hook-fail", &archive, false).unwrap_err();

    assert!(error.contains("post_install"), "{error}");
    assert!(!temp
        .path()
        .join("rootfs-hook-fail/usr/share/hook-test/hook-fail.txt")
        .exists());
}

#[test]
fn no_hooks_skips_execution_but_keeps_scripts_in_database() {
    let temp = TempDir::new().unwrap();
    let script =
        "mkdir -p \"$XRPKG_ROOTFS/etc\"\necho configured > \"$XRPKG_ROOTFS/etc/marker.txt\"\n";
    let archive = build_hooked_package(&temp, "hook-skip", Some(script));

    install_one(&temp, "hook-skip", &archive, true).unwrap();

    assert!(!temp.path().join("rootfs-hook-skip/etc/marker.txt").exists());
}
#[test]
fn pre_and_post_remove_hooks_run_around_uninstall() {
    use xrpkg::uninstall::remove_package;

    let temp = TempDir::new().unwrap();
    let source = temp.path().join("hook-remove-src");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("payload.txt"), b"remove hook payload\n").unwrap();
    fs::write(
        temp.path().join("hook-remove-pre.sh"),
        "mkdir -p \"$XRPKG_ROOTFS/etc\"\necho pre > \"$XRPKG_ROOTFS/etc/pre-remove.txt\"\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("hook-remove-post.sh"),
        "echo post > \"$XRPKG_ROOTFS/etc/post-remove.txt\"\n",
    )
    .unwrap();
    let recipe = temp.path().join("hook-remove.toml");
    fs::write(
        &recipe,
        r#"
name = "hook-remove"
version = "1.0"
source = "hook-remove-src"
requires = []
build = []
package = [
  "install -Dm644 payload.txt \"$PKGDIR/usr/share/hook-test/hook-remove.txt\"",
]
[hooks]
pre_remove = "hook-remove-pre.sh"
post_remove = "hook-remove-post.sh"
"#,
    )
    .unwrap();
    let archive = build_package(&recipe, &temp.path().join("out-hook-remove")).unwrap();

    let mut db = LocalDb::open(":memory:").unwrap();
    let mut archives = HashMap::new();
    archives.insert(
        ("hook-remove".to_string(), Version::parse("1.0").unwrap()),
        archive,
    );
    let changes = vec![SelectedPackage {
        name: "hook-remove".to_string(),
        version: Version::parse("1.0").unwrap(),
    }];
    let rootfs_path = temp.path().join("rootfs-hook-remove");
    install_changes(&mut db, &changes, &archives, &rootfs_path, true, false).unwrap();
    assert!(db.list_hooks("hook-remove").unwrap().len() == 2);

    let rootfs = xrpkg::xpkg::Rootfs::prepare(&rootfs_path).unwrap();
    remove_package(&mut db, &rootfs, "hook-remove", false).unwrap();

    assert_eq!(
        fs::read_to_string(rootfs_path.join("etc/pre-remove.txt")).unwrap(),
        "pre\n"
    );
    assert_eq!(
        fs::read_to_string(rootfs_path.join("etc/post-remove.txt")).unwrap(),
        "post\n"
    );
    assert!(db.list_installed().unwrap().is_empty());
    assert!(!rootfs_path
        .join("usr/share/hook-test/hook-remove.txt")
        .exists());
}
#[test]
fn hook_sandbox_cannot_write_to_host_etc() {
    let temp = TempDir::new().unwrap();
    let host_marker = std::path::Path::new("/etc/xrpkg-hook-host-escape.txt");
    let _ = fs::remove_file(host_marker);
    let archive = build_hooked_package(
        &temp,
        "hook-sandbox",
        Some("echo escaped > /etc/xrpkg-hook-host-escape.txt\n"),
    );

    install_one(&temp, "hook-sandbox", &archive, false).unwrap();

    assert!(!host_marker.exists());
}
