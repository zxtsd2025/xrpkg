//! 安装后 ldconfig / fc-cache 触发器的集成测试。
//!
//! 测试 Rootfs 里放一个最小 bash 运行时和桩 `ldconfig`/`fc-cache`；触发器
//! 必须通过 chroot 沙箱执行目标工具，桩脚本把调用记录到 `/trigger-calls.log`。

mod common;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Cursor, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::write::GzEncoder;
use flate2::Compression;
use tar::{Builder, Header};
use tempfile::TempDir;
use xrpkg::builder::build_package;
use xrpkg::capability::Version;
use xrpkg::installer::install_changes;
use xrpkg::solver::SelectedPackage;
use xrpkg::state::LocalDb;

/// 当前环境是否支持 hooks 使用的 unshare 沙箱。
fn sandbox_available() -> bool {
    Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--mount",
            "--pid",
            "--fork",
            "/bin/true",
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// 把宿主 bash 及其动态库复制到目标 Rootfs，使 chroot 内能执行 shell 脚本。
fn install_shell_runtime(rootfs: &Path) {
    fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
    fs::create_dir_all(rootfs.join("usr/lib")).unwrap();
    fs::create_dir_all(rootfs.join("usr/sbin")).unwrap();
    fs::create_dir_all(rootfs.join("etc")).unwrap();
    symlink("usr/bin", rootfs.join("bin")).unwrap();
    symlink("usr/lib", rootfs.join("lib")).unwrap();
    symlink("usr/lib", rootfs.join("lib64")).unwrap();

    let bash = fs::canonicalize("/bin/bash").unwrap();
    fs::copy(&bash, rootfs.join("usr/bin/bash")).unwrap();
    fs::set_permissions(
        rootfs.join("usr/bin/bash"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("bash", rootfs.join("usr/bin/sh")).unwrap();

    let output = Command::new("ldd").arg(&bash).output().unwrap();
    assert!(output.status.success());
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        for token in line.split_whitespace() {
            if token.starts_with('/') && Path::new(token).is_file() {
                let relative = token.trim_start_matches('/');
                let destination = rootfs.join(relative);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::copy(token, &destination).unwrap();
                break;
            }
        }
    }
}

/// 写一个会把自身名字追加到 `/trigger-calls.log` 的桩工具。
fn write_stub_tool(rootfs: &Path, relative: &str, name: &str) {
    let path = rootfs.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        format!("#!/bin/sh\nprintf '%s\\n' '{name}' >> /trigger-calls.log\n"),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// 构建一个只安装单个文件的 `.xpkg`。
fn build_one_file_package(temp: &TempDir, name: &str, destination: &str) -> PathBuf {
    let source = temp.path().join(format!("{name}-src"));
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("payload"), format!("{name}\n")).unwrap();
    let recipe = temp.path().join(format!("{name}.toml"));
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
  "install -Dm644 payload \"$PKGDIR/{destination}\"",
]
"#
        ),
    )
    .unwrap();
    build_package(&recipe, &temp.path().join(format!("out-{name}"))).unwrap()
}

/// 安装单包并返回目标 Rootfs 路径。
fn install_package(
    name: &str,
    archive: &Path,
    rootfs: &Path,
    no_hooks: bool,
) -> Result<(), String> {
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
    install_changes(&mut db, &changes, &archives, rootfs, no_hooks, false)
}

fn calls(rootfs: &Path) -> String {
    match fs::read_to_string(rootfs.join("trigger-calls.log")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => panic!("读取触发器记录失败: {error}"),
    }
}

#[test]
fn library_file_triggers_ldconfig_exactly_once() {
    common::isolate_cache();
    if !sandbox_available() {
        eprintln!("skipped: 当前环境不支持 unshare user/mount namespace");
        return;
    }
    let temp = TempDir::new().unwrap();
    let rootfs = temp.path().join("rootfs");
    install_shell_runtime(&rootfs);
    write_stub_tool(&rootfs, "usr/sbin/ldconfig", "ldconfig");
    write_stub_tool(&rootfs, "usr/bin/fc-cache", "fc-cache");
    let archive = build_one_file_package(&temp, "trigger-lib", "usr/lib/libtrigger.so.1");

    install_package("trigger-lib", &archive, &rootfs, false).unwrap();

    assert_eq!(calls(&rootfs), "ldconfig\n");
}

#[test]
fn font_file_triggers_fc_cache_exactly_once() {
    common::isolate_cache();
    if !sandbox_available() {
        eprintln!("skipped: 当前环境不支持 unshare user/mount namespace");
        return;
    }
    let temp = TempDir::new().unwrap();
    let rootfs = temp.path().join("rootfs");
    install_shell_runtime(&rootfs);
    write_stub_tool(&rootfs, "usr/sbin/ldconfig", "ldconfig");
    write_stub_tool(&rootfs, "usr/bin/fc-cache", "fc-cache");
    let archive = build_one_file_package(&temp, "trigger-font", "usr/share/fonts/TTF/x.ttf");

    install_package("trigger-font", &archive, &rootfs, false).unwrap();

    assert_eq!(calls(&rootfs), "fc-cache\n");
}

#[test]
fn unrelated_file_triggers_nothing() {
    common::isolate_cache();
    if !sandbox_available() {
        eprintln!("skipped: 当前环境不支持 unshare user/mount namespace");
        return;
    }
    let temp = TempDir::new().unwrap();
    let rootfs = temp.path().join("rootfs");
    install_shell_runtime(&rootfs);
    write_stub_tool(&rootfs, "usr/sbin/ldconfig", "ldconfig");
    write_stub_tool(&rootfs, "usr/bin/fc-cache", "fc-cache");
    let archive = build_one_file_package(&temp, "trigger-doc", "usr/share/doc/x/readme");

    install_package("trigger-doc", &archive, &rootfs, false).unwrap();

    assert_eq!(calls(&rootfs), "");
}

#[test]
fn no_hooks_skips_triggers() {
    common::isolate_cache();
    if !sandbox_available() {
        eprintln!("skipped: 当前环境不支持 unshare user/mount namespace");
        return;
    }
    let temp = TempDir::new().unwrap();
    let rootfs = temp.path().join("rootfs");
    install_shell_runtime(&rootfs);
    write_stub_tool(&rootfs, "usr/sbin/ldconfig", "ldconfig");
    let archive = build_one_file_package(&temp, "trigger-skip", "usr/lib/libskip.so.1");

    install_package("trigger-skip", &archive, &rootfs, true).unwrap();

    assert_eq!(calls(&rootfs), "");
}

#[test]
fn missing_tool_warns_but_keeps_the_install() {
    common::isolate_cache();
    if !sandbox_available() {
        eprintln!("skipped: 当前环境不支持 unshare user/mount namespace");
        return;
    }
    let temp = TempDir::new().unwrap();
    let rootfs = temp.path().join("rootfs");
    install_shell_runtime(&rootfs);
    // 不创建桩工具：触发器应打印 warning，而不是让安装失败。
    let archive = build_one_file_package(&temp, "trigger-missing", "usr/lib/libmissing.so.1");

    install_package("trigger-missing", &archive, &rootfs, false).unwrap();

    assert!(rootfs.join("usr/lib/libmissing.so.1").is_file());
}

fn tar_bytes(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut builder = Builder::new(Vec::new());
    for (path, contents, mode) in entries {
        let mut header = Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(*mode);
        builder.append_data(&mut header, path, *contents).unwrap();
    }
    builder.into_inner().unwrap()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn xz(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = liblzma::write::XzEncoder::new(Vec::new(), 6);
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

/// 构造一个包含 `/usr/lib` 文件的 `.deb`，用于验证兼容轨道也触发 ldconfig。
fn write_deb_with_library(path: &Path) {
    let control =
        b"Package: trigger-deb\nVersion: 1.0-1\nArchitecture: amd64\nDescription: trigger test\n";
    let control_tar = tar_bytes(&[("./control", control, 0o644)]);
    let data_tar = tar_bytes(&[("./usr/lib/libdeb-trigger.so.1", b"stub\n", 0o644)]);
    let file = File::create(path).unwrap();
    let mut builder = ar::Builder::new(file);
    builder
        .append(
            &ar::Header::new(b"debian-binary".to_vec(), 4),
            Cursor::new(b"2.0\n"),
        )
        .unwrap();
    let control_gz = gzip(&control_tar);
    builder
        .append(
            &ar::Header::new(b"control.tar.gz".to_vec(), control_gz.len() as u64),
            Cursor::new(&control_gz),
        )
        .unwrap();
    let data_xz = xz(&data_tar);
    builder
        .append(
            &ar::Header::new(b"data.tar.xz".to_vec(), data_xz.len() as u64),
            Cursor::new(&data_xz),
        )
        .unwrap();
    builder.into_inner().unwrap();
}

#[test]
fn compatibility_deb_install_also_runs_ldconfig() {
    if !sandbox_available() {
        eprintln!("skipped: 当前环境不支持 unshare user/mount namespace");
        return;
    }
    common::isolate_cache();

    let temp = TempDir::new().unwrap();
    let rootfs = temp.path().join("rootfs");
    install_shell_runtime(&rootfs);
    write_stub_tool(&rootfs, "usr/sbin/ldconfig", "ldconfig");
    write_stub_tool(&rootfs, "usr/bin/fc-cache", "fc-cache");
    let deb = temp.path().join("trigger-deb.deb");
    write_deb_with_library(&deb);

    let output = Command::new(env!("CARGO_BIN_EXE_xrpkg"))
        .args(["install"])
        .arg(&deb)
        .arg("--root")
        .arg(&rootfs)
        .arg("--db")
        .arg(temp.path().join("local.db"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "兼容轨道安装失败: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(calls(&rootfs), "ldconfig\n");
}

#[test]
fn failing_tool_warns_but_keeps_the_install() {
    if !sandbox_available() {
        eprintln!("skipped: 当前环境不支持 unshare user/mount namespace");
        return;
    }
    let temp = TempDir::new().unwrap();
    let rootfs = temp.path().join("rootfs");
    install_shell_runtime(&rootfs);
    let path = rootfs.join("usr/sbin/ldconfig");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "#!/bin/sh\necho broken-ldconfig >&2\nexit 3\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    let archive = build_one_file_package(&temp, "trigger-fail", "usr/lib/libfail.so.1");

    install_package("trigger-fail", &archive, &rootfs, false).unwrap();

    assert!(rootfs.join("usr/lib/libfail.so.1").is_file());
}
