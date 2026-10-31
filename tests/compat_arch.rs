use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tar::{Builder, EntryType, Header};
use tempfile::TempDir;
use xrpkg::abi::{AbiGuard, MAX_GLIBC_VERSION};
use xrpkg::compat_arch::{extract_arch_data, parse_arch_metadata, read_arch_hooks};

const BASE_ELF: &[u8] = include_bytes!("fixtures/libevil.so");
const PKGINFO: &str = "pkgname = hello-arch
pkgver = 1.0-1
pkgdesc = fake Arch package for xrpkg
arch = x86_64
depend = glibc
depend = libfoo>=1.0
";

fn append(builder: &mut Builder<Vec<u8>>, path: &str, contents: &[u8]) {
    let mut header = Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o644);
    builder.append_data(&mut header, path, contents).unwrap();
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

fn zstd(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn write_arch(path: &Path, data_tar: &[u8]) {
    fs::write(path, zstd(data_tar)).unwrap();
}

fn good_entries() -> Vec<(&'static str, &'static [u8], u32)> {
    vec![
        (".PKGINFO", PKGINFO.as_bytes(), 0o644),
        (".INSTALL", b"post_install() { :; }\n", 0o644),
        (".MTREE", b"# fake mtree\n", 0o644),
        (
            "./usr/bin/hello-arch",
            b"#!/bin/sh\necho hello arch\n",
            0o755,
        ),
        (
            "./usr/share/doc/hello-arch/readme.txt",
            b"hello arch docs\n",
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
fn parses_pkgname_pkgver_and_depends() {
    let temp = TempDir::new().unwrap();
    let arch = temp.path().join("hello.pkg.tar.zst");
    write_arch(&arch, &tar_bytes(&good_entries()));

    let metadata = parse_arch_metadata(&arch).unwrap();

    assert_eq!(metadata.pkgname, "hello-arch");
    assert_eq!(metadata.pkgver, "1.0-1");
    assert_eq!(metadata.depends, ["glibc", "libfoo>=1.0"]);
}

#[test]
fn extracts_files_and_skips_root_dot_metadata() {
    let temp = TempDir::new().unwrap();
    let arch = temp.path().join("hello.pkg.tar.zst");
    write_arch(&arch, &tar_bytes(&good_entries()));
    let rootfs = temp.path().join("rootfs");

    let files = extract_arch_data(&arch, &rootfs).unwrap();

    assert_eq!(
        files,
        [
            PathBuf::from("usr/bin/hello-arch"),
            PathBuf::from("usr/share/doc/hello-arch/readme.txt")
        ]
    );
    assert!(!rootfs.join(".PKGINFO").exists());
    assert!(!rootfs.join(".INSTALL").exists());
    assert!(!rootfs.join(".MTREE").exists());
    assert_eq!(
        fs::read_to_string(rootfs.join("usr/bin/hello-arch")).unwrap(),
        "#!/bin/sh\necho hello arch\n"
    );
    let mode = fs::metadata(rootfs.join("usr/bin/hello-arch"))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0);
}

#[test]
fn rejects_parent_directory_traversal_in_arch_payload() {
    let temp = TempDir::new().unwrap();
    let mut builder = Builder::new(Vec::new());
    let payload = b"escaped\n";
    let path = b"./../../escape.txt";
    let mut header = Header::new_gnu();
    header.set_size(payload.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(EntryType::Regular);
    header.as_mut_bytes()[..path.len()].copy_from_slice(path);
    header.set_cksum();
    builder.append(&header, &payload[..]).unwrap();
    let data_tar = builder.into_inner().unwrap();

    let arch = temp.path().join("evil.pkg.tar.zst");
    write_arch(&arch, &data_tar);
    let rootfs = temp.path().join("rootfs");

    let error = extract_arch_data(&arch, &rootfs).unwrap_err();

    assert!(error.contains("非法"), "{error}");
    assert!(!temp.path().join("escape.txt").exists());
}

#[test]
fn abi_guard_rejects_elf_from_arch_above_the_baseline() {
    let temp = TempDir::new().unwrap();
    let data = tar_bytes(&[
        (".PKGINFO", PKGINFO.as_bytes(), 0o644),
        ("./usr/lib/libevil.so", &bad_elf(), 0o644),
    ]);
    let arch = temp.path().join("bad.pkg.tar.zst");
    write_arch(&arch, &data);
    let rootfs = temp.path().join("rootfs");

    let files = extract_arch_data(&arch, &rootfs).unwrap();
    let guard = AbiGuard::new(MAX_GLIBC_VERSION).unwrap();
    let error = guard
        .check_file(
            "hello-arch",
            "usr/lib/libevil.so",
            &rootfs.join("usr/lib/libevil.so"),
        )
        .unwrap_err();

    assert!(files.contains(&PathBuf::from("usr/lib/libevil.so")));
    assert_eq!(
        error,
        "包 hello-arch 中的 /usr/lib/libevil.so 需要 GLIBC_2.45，但系统基线为 2.44"
    );
}

#[test]
fn reads_arch_install_functions_as_hooks() {
    let temp = TempDir::new().unwrap();
    let arch = temp.path().join("hello.pkg.tar.zst");
    write_arch(&arch, &tar_bytes(&good_entries()));

    let hooks = read_arch_hooks(&arch).unwrap();

    assert_eq!(hooks.len(), 4);
    assert!(hooks.iter().any(|hook| hook.phase == "post_install"
        && String::from_utf8_lossy(&hook.script).contains("post_install")));
}
#[test]
fn extracts_symlinks_and_hardlinks_with_escape_checks() {
    let temp = TempDir::new().unwrap();
    let mut builder = Builder::new(Vec::new());
    append(&mut builder, ".PKGINFO", PKGINFO.as_bytes());
    append(&mut builder, "./usr/lib/libevil.so", BASE_ELF);

    let mut symlink = Header::new_gnu();
    symlink.set_size(0);
    symlink.set_mode(0o777);
    symlink.set_entry_type(EntryType::Symlink);
    builder
        .append_link(&mut symlink, "./usr/lib/libevil.so.1", "libevil.so")
        .unwrap();

    let mut hardlink = Header::new_gnu();
    hardlink.set_size(0);
    hardlink.set_mode(0o644);
    hardlink.set_entry_type(EntryType::Link);
    builder
        .append_link(
            &mut hardlink,
            "./usr/lib/libevil-hard.so",
            "usr/lib/libevil.so",
        )
        .unwrap();

    let arch = temp.path().join("links.pkg.tar.zst");
    write_arch(&arch, &builder.into_inner().unwrap());
    let rootfs = temp.path().join("rootfs");

    let files = extract_arch_data(&arch, &rootfs).unwrap();

    assert!(files.contains(&PathBuf::from("usr/lib/libevil.so.1")));
    assert!(files.contains(&PathBuf::from("usr/lib/libevil-hard.so")));
    assert_eq!(
        fs::read_link(rootfs.join("usr/lib/libevil.so.1")).unwrap(),
        PathBuf::from("libevil.so")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(rootfs.join("usr/lib/libevil-hard.so"))
                .unwrap()
                .nlink(),
            2
        );
    }
}

#[test]
fn rejects_absolute_symlink_targets_in_arch_payload() {
    let temp = TempDir::new().unwrap();
    let mut builder = Builder::new(Vec::new());
    append(&mut builder, ".PKGINFO", PKGINFO.as_bytes());

    let mut symlink = Header::new_gnu();
    symlink.set_size(0);
    symlink.set_mode(0o777);
    symlink.set_entry_type(EntryType::Symlink);
    builder
        .append_link(&mut symlink, "./usr/lib/escape.so", "/etc/passwd")
        .unwrap();

    let arch = temp.path().join("evil-link.pkg.tar.zst");
    write_arch(&arch, &builder.into_inner().unwrap());
    let rootfs = temp.path().join("rootfs");

    let error = extract_arch_data(&arch, &rootfs).unwrap_err();

    assert!(error.contains("绝对路径"), "{error}");
    assert!(!rootfs.join("usr/lib/escape.so").exists());
}
