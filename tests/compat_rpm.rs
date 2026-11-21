use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rpm::{Dependency, FileOptions, PackageBuilder};
use tempfile::TempDir;
use xrpkg::abi::{AbiGuard, MAX_GLIBC_VERSION};
use xrpkg::compat_rpm::{extract_rpm_data, parse_rpm_metadata, read_rpm_hooks};

const BASE_ELF: &[u8] = include_bytes!("fixtures/libevil.so");

fn write_rpm(path: &Path, files: &[(&str, &[u8], u32)]) {
    let mut builder = PackageBuilder::new(
        "hello-rpm",
        "1.0",
        "MIT",
        "x86_64",
        "fake RPM for xrpkg compatibility testing",
    );
    builder
        .release("1")
        .epoch(1)
        .requires(Dependency::any("libc.so.6()(64bit)"))
        .requires(Dependency::rpmlib("CompressedFileNames", "3.0.4-1"))
        .requires(Dependency::greater_eq("glibc", "2.17"))
        .post_install_script("echo rpm-postinst");

    for (destination, contents, mode) in files {
        builder
            .with_file_contents(
                *contents,
                FileOptions::new(*destination).permissions(*mode as u16),
            )
            .unwrap();
    }

    let package = builder.build().unwrap();
    package.write_file(path).unwrap();
}

fn good_files() -> Vec<(&'static str, &'static [u8], u32)> {
    vec![
        ("/usr/bin/hello-rpm", b"#!/bin/sh\necho hello rpm\n", 0o755),
        (
            "/usr/share/doc/hello-rpm/readme.txt",
            b"hello rpm docs\n",
            0o644,
        ),
    ]
}

fn bad_elf() -> Vec<u8> {
    let mut elf = BASE_ELF.to_vec();
    let old = b"GLIBC_2.00";
    let new = b"GLIBC_2.45";
    let position = elf
        .windows(old.len())
        .position(|window| window == old)
        .unwrap();
    elf[position..position + new.len()].copy_from_slice(new);
    elf
}

#[test]
fn parses_name_version_and_raw_requires() {
    let temp = TempDir::new().unwrap();
    let rpm = temp.path().join("hello.rpm");
    write_rpm(&rpm, &good_files());

    let metadata = parse_rpm_metadata(&rpm).unwrap();

    assert_eq!(metadata.name, "hello-rpm");
    assert_eq!(metadata.version, "1.0");
    assert_eq!(metadata.release, "1");
    assert_eq!(metadata.epoch, Some(1));
    assert!(
        metadata
            .requires
            .contains(&"libc.so.6()(64bit)".to_string()),
        "{:?}",
        metadata.requires
    );
    assert!(
        metadata
            .requires
            .contains(&"rpmlib(CompressedFileNames) <= 3.0.4-1".to_string()),
        "{:?}",
        metadata.requires
    );
    assert!(
        metadata.requires.contains(&"glibc >= 2.17".to_string()),
        "{:?}",
        metadata.requires
    );
}

#[test]
fn extracts_payload_files_and_preserves_modes() {
    let temp = TempDir::new().unwrap();
    let rpm = temp.path().join("hello.rpm");
    write_rpm(&rpm, &good_files());
    let rootfs = temp.path().join("rootfs");

    let files = extract_rpm_data(&rpm, &rootfs).unwrap();

    assert_eq!(
        files,
        [
            PathBuf::from("usr/bin/hello-rpm"),
            PathBuf::from("usr/share/doc/hello-rpm/readme.txt")
        ]
    );
    assert_eq!(
        fs::read_to_string(rootfs.join("usr/bin/hello-rpm")).unwrap(),
        "#!/bin/sh\necho hello rpm\n"
    );
    let mode = fs::metadata(rootfs.join("usr/bin/hello-rpm"))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0);
    assert_eq!(
        fs::read_to_string(rootfs.join("usr/share/doc/hello-rpm/readme.txt")).unwrap(),
        "hello rpm docs\n"
    );
}

#[test]
fn abi_guard_rejects_elf_from_rpm_above_the_baseline() {
    let temp = TempDir::new().unwrap();
    let rpm = temp.path().join("bad.rpm");
    let elf = bad_elf();
    write_rpm(
        &rpm,
        &[
            ("/usr/lib/libevil.so", &elf, 0o644),
            ("/usr/share/doc/hello-rpm/readme.txt", b"docs\n", 0o644),
        ],
    );
    let rootfs = temp.path().join("rootfs");

    let files = extract_rpm_data(&rpm, &rootfs).unwrap();
    let guard = AbiGuard::new(MAX_GLIBC_VERSION).unwrap();
    let error = guard
        .check_file(
            "hello-rpm",
            "usr/lib/libevil.so",
            &rootfs.join("usr/lib/libevil.so"),
        )
        .unwrap_err();

    assert!(files.contains(&PathBuf::from("usr/lib/libevil.so")));
    assert_eq!(
        error,
        "包 hello-rpm 中的 /usr/lib/libevil.so 需要 GLIBC_2.45，但系统基线为 2.44"
    );
}

#[test]
fn rejects_parent_directory_traversal_in_rpm_payload() {
    let temp = TempDir::new().unwrap();
    let rpm = temp.path().join("evil.rpm");
    write_rpm(&rpm, &[("/usr/../../escape.txt", b"escaped\n", 0o644)]);
    let rootfs = temp.path().join("rootfs");

    let error = extract_rpm_data(&rpm, &rootfs).unwrap_err();

    assert!(error.contains("非法"), "{error}");
    assert!(!temp.path().join("escape.txt").exists());
}

#[test]
fn reads_rpm_scriptlets() {
    let temp = TempDir::new().unwrap();
    let rpm = temp.path().join("hello.rpm");
    write_rpm(&rpm, &good_files());

    let hooks = read_rpm_hooks(&rpm).unwrap();

    assert!(hooks.iter().any(|hook| hook.phase == "post_install"));
    assert!(hooks
        .iter()
        .find(|hook| hook.phase == "post_install")
        .unwrap()
        .script
        .starts_with(b"echo rpm-postinst"));
}

#[test]
fn extracts_relative_symlinks_with_rootfs_validation() {
    let temp = TempDir::new().unwrap();
    let rpm = temp.path().join("link.rpm");
    let mut builder = PackageBuilder::new("link-rpm", "1.0", "MIT", "x86_64", "symlink");
    builder
        .with_symlink(FileOptions::symlink(
            "/usr/lib/libhello.so",
            "libhello.so.1",
        ))
        .unwrap();
    builder.build().unwrap().write_file(&rpm).unwrap();
    let rootfs = temp.path().join("rootfs");

    let files = extract_rpm_data(&rpm, &rootfs).unwrap();

    assert!(files.contains(&PathBuf::from("usr/lib/libhello.so")));
    assert_eq!(
        fs::read_link(rootfs.join("usr/lib/libhello.so")).unwrap(),
        PathBuf::from("libhello.so.1")
    );
}

#[test]
fn rejects_absolute_symlink_targets() {
    let temp = TempDir::new().unwrap();
    let rpm = temp.path().join("absolute.rpm");
    let mut builder = PackageBuilder::new("absolute-rpm", "1.0", "MIT", "x86_64", "symlink");
    builder
        .with_symlink(FileOptions::symlink(
            "/usr/lib/libhello.so",
            "/usr/lib/libhello.so.1",
        ))
        .unwrap();
    builder.build().unwrap().write_file(&rpm).unwrap();

    let error = extract_rpm_data(&rpm, &temp.path().join("rootfs")).unwrap_err();

    assert!(error.contains("绝对路径"), "{error}");
}
