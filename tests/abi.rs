use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use xrpkg::abi::{inspect_elf, AbiGuard, MAX_GLIBC_VERSION};
use xrpkg::capability::Version;

const BASE_ELF: &[u8] = include_bytes!("fixtures/libevil.so");

fn fake_elf(temp: &TempDir, version: &str) -> PathBuf {
    let mut bytes = BASE_ELF.to_vec();
    let old = b"GLIBC_2.00";
    let new = format!("GLIBC_{version}");
    assert_eq!(old.len(), new.len());

    let position = bytes
        .windows(old.len())
        .position(|window| window == old)
        .unwrap();
    bytes[position..position + new.len()].copy_from_slice(new.as_bytes());

    let path = temp.path().join(format!("libevil-{version}.so"));
    fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn reads_the_highest_glibc_requirement_from_version_r() {
    let temp = TempDir::new().unwrap();
    let path = fake_elf(&temp, "2.44");

    let required = inspect_elf(&path).unwrap().unwrap();

    assert_eq!(required, Version::parse("2.44").unwrap());
}

#[test]
fn accepts_a_file_at_the_baseline() {
    let temp = TempDir::new().unwrap();
    let path = fake_elf(&temp, "2.44");
    let guard = AbiGuard::new(MAX_GLIBC_VERSION).unwrap();

    assert!(guard.check_file("app", "lib/libevil.so", &path).is_ok());
}

#[test]
fn rejects_a_file_above_the_baseline() {
    let temp = TempDir::new().unwrap();
    let path = fake_elf(&temp, "2.45");
    let guard = AbiGuard::new(MAX_GLIBC_VERSION).unwrap();

    let error = guard
        .check_file("app", "lib/libevil.so", &path)
        .unwrap_err();

    assert_eq!(
        error,
        "包 app 中的 /lib/libevil.so 需要 GLIBC_2.45，但系统基线为 2.44"
    );
}

#[test]
fn skips_non_elf_files() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("readme.txt");
    fs::write(&path, b"not an elf\n").unwrap();
    let guard = AbiGuard::new(MAX_GLIBC_VERSION).unwrap();

    assert_eq!(inspect_elf(&path).unwrap(), None);
    assert!(guard.check_file("app", "readme.txt", &path).is_ok());
}

#[test]
fn rejects_a_malformed_file_with_elf_magic() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("broken-elf");
    fs::write(&path, b"\x7fELFbroken").unwrap();

    let error = inspect_elf(Path::new(&path)).unwrap_err();

    assert!(error.contains("解析 ELF"), "{error}");
}
