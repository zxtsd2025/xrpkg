//! RPM `%caps` 到 `security.capability` 的端到端测试。

use std::fs;
use std::path::Path;
use std::process::Command;

use rpm::{FileOptions, PackageBuilder};
use tempfile::TempDir;
use xrpkg::compat_rpm::extract_rpm_data;

/// 生成一个带 `%caps` 的最小 RPM。
fn write_rpm_with_caps(path: &Path, destination: &str, caps: &str) {
    let mut builder = PackageBuilder::new(
        "caps-rpm",
        "1.0",
        "MIT",
        "x86_64",
        "fake RPM with file capabilities",
    );
    builder
        .release("1")
        .with_file_contents(
            b"#!/bin/sh\n".to_vec(),
            FileOptions::new(destination)
                .permissions(0o755)
                .caps(caps)
                .unwrap(),
        )
        .unwrap();
    builder.build().unwrap().write_file(path).unwrap();
}

/// `cap_net_raw=ep` 的期望二进制：REV2 + effective，permitted 第 13 位。
const EXPECTED_NET_RAW: [u8; 20] = [
    0x01, 0x00, 0x00, 0x02, // magic_etc
    0x00, 0x20, 0x00, 0x00, // permitted 低 32 位
    0x00, 0x00, 0x00, 0x00, // inheritable 低 32 位
    0x00, 0x00, 0x00, 0x00, // permitted 高 32 位
    0x00, 0x00, 0x00, 0x00, // inheritable 高 32 位
];

#[test]
fn extracts_rpm_caps_into_security_capability_when_root() {
    if !xrpkg::fs_meta::is_root() {
        eprintln!("skipped: security.capability 需要 CAP_SETFCAP/root");
        return;
    }
    if Command::new("getcap").arg("--version").output().is_err() {
        eprintln!("skipped: 未安装 getcap");
        return;
    }

    let temp = TempDir::new().unwrap();
    let rpm = temp.path().join("caps.rpm");
    write_rpm_with_caps(&rpm, "/usr/bin/ping", "cap_net_raw=ep");
    let rootfs = temp.path().join("rootfs");

    let files = extract_rpm_data(&rpm, &rootfs).unwrap();

    assert_eq!(files, [std::path::PathBuf::from("usr/bin/ping")]);
    let ping = rootfs.join("usr/bin/ping");
    let value = xattr::get(&ping, "security.capability").unwrap();
    assert_eq!(value, Some(EXPECTED_NET_RAW.to_vec()));

    let output = Command::new("getcap").arg(&ping).output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("cap_net_raw=ep"), "{text}");
}

#[test]
fn fails_with_a_clear_error_without_cap_setfcap() {
    if xrpkg::fs_meta::is_root() {
        eprintln!("skipped: root 可以写入 security.capability");
        return;
    }

    let temp = TempDir::new().unwrap();
    let rpm = temp.path().join("caps.rpm");
    write_rpm_with_caps(&rpm, "/usr/bin/ping", "cap_net_raw=ep");
    let rootfs = temp.path().join("rootfs");

    let error = extract_rpm_data(&rpm, &rootfs).unwrap_err();

    assert!(
        error.contains("security.capability"),
        "错误应明确指出失败的扩展属性: {error}"
    );
    assert!(
        error.contains("扩展属性"),
        "错误应来自 apply_metadata: {error}"
    );
    // 提取发生在暂存目录；调用方在失败后不会发布任何文件到目标 Rootfs。
    // 这里只验证错误足够明确，让安装流程能够中止并回滚。
}

#[test]
fn files_without_caps_have_no_xattr() {
    let temp = TempDir::new().unwrap();
    let rpm = temp.path().join("plain.rpm");
    let mut builder = PackageBuilder::new("plain-rpm", "1.0", "MIT", "x86_64", "plain");
    builder
        .with_file_contents(
            b"plain\n".to_vec(),
            FileOptions::new("/usr/share/plain.txt").permissions(0o644),
        )
        .unwrap();
    builder.build().unwrap().write_file(&rpm).unwrap();
    let rootfs = temp.path().join("rootfs");

    extract_rpm_data(&rpm, &rootfs).unwrap();

    assert_eq!(
        xattr::get(rootfs.join("usr/share/plain.txt"), "security.capability").unwrap(),
        None
    );
    assert!(fs::metadata(rootfs.join("usr/share/plain.txt")).is_ok());
}
