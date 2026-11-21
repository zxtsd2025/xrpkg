use std::fs::{self, File};
use std::io::{Cursor, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;
use tar::{Builder, EntryType, Header};
use tempfile::TempDir;
use xrpkg::abi::{AbiGuard, MAX_GLIBC_VERSION};
use xrpkg::compat_deb::{extract_deb_data, parse_deb_metadata, read_deb_hooks};

const BASE_ELF: &[u8] = include_bytes!("fixtures/libevil.so");
const CONTROL: &str = "Package: hello-deb
Version: 1.0-1
Architecture: amd64
Maintainer: Test <test@example.com>
Depends: libc6 (>= 2.34) | libc6.1, libfoo
Description: fake package for xrpkg
";

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

fn append_ar(builder: &mut ar::Builder<File>, name: &[u8], data: &[u8]) {
    let header = ar::Header::new(name.to_vec(), data.len() as u64);
    builder.append(&header, Cursor::new(data)).unwrap();
}

fn write_deb(path: &Path, data_tar: &[u8]) -> PathBuf {
    let control_tar = tar_bytes(&[
        ("./control", CONTROL.as_bytes(), 0o644),
        ("./postinst", b"#!/bin/sh\\necho deb-postinst\\n", 0o755),
    ]);
    let file = File::create(path).unwrap();
    let mut builder = ar::Builder::new(file);
    append_ar(&mut builder, b"debian-binary", b"2.0\n");
    append_ar(&mut builder, b"control.tar.gz", &gzip(&control_tar));
    append_ar(&mut builder, b"data.tar.xz", &xz(data_tar));
    builder.into_inner().unwrap();
    path.to_path_buf()
}

fn good_data() -> Vec<u8> {
    tar_bytes(&[
        ("./usr/bin/hello-deb", b"#!/bin/sh\necho hello deb\n", 0o755),
        (
            "./usr/share/doc/hello-deb/readme.txt",
            b"hello deb docs\n",
            0o644,
        ),
    ])
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
fn parses_package_version_and_depends() {
    let temp = TempDir::new().unwrap();
    let deb = write_deb(&temp.path().join("hello.deb"), &good_data());

    let metadata = parse_deb_metadata(&deb).unwrap();

    assert_eq!(metadata.package, "hello-deb");
    assert_eq!(metadata.version, "1.0-1");
    assert_eq!(
        metadata.depends.as_deref(),
        Some("libc6 (>= 2.34) | libc6.1, libfoo")
    );
}

#[test]
fn extracts_data_files_and_preserves_modes() {
    let temp = TempDir::new().unwrap();
    let deb = write_deb(&temp.path().join("hello.deb"), &good_data());
    let rootfs = temp.path().join("rootfs");

    let files = extract_deb_data(&deb, &rootfs).unwrap();

    assert_eq!(
        files,
        [
            PathBuf::from("usr/bin/hello-deb"),
            PathBuf::from("usr/share/doc/hello-deb/readme.txt")
        ]
    );
    assert_eq!(
        fs::read_to_string(rootfs.join("usr/bin/hello-deb")).unwrap(),
        "#!/bin/sh\necho hello deb\n"
    );
    let mode = fs::metadata(rootfs.join("usr/bin/hello-deb"))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0);
    assert_eq!(
        fs::read_to_string(rootfs.join("usr/share/doc/hello-deb/readme.txt")).unwrap(),
        "hello deb docs\n"
    );
}

#[test]
fn rejects_parent_directory_traversal_in_data_tar() {
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

    let deb = write_deb(&temp.path().join("evil.deb"), &data_tar);
    let rootfs = temp.path().join("rootfs");

    let error = extract_deb_data(&deb, &rootfs).unwrap_err();

    assert!(error.contains("非法"), "{error}");
    assert!(!temp.path().join("escape.txt").exists());
}

#[test]
fn abi_guard_rejects_elf_from_deb_above_the_baseline() {
    let temp = TempDir::new().unwrap();
    let data = tar_bytes(&[
        ("./usr/lib/libevil.so", &bad_elf(), 0o644),
        ("./usr/share/doc/hello-deb/readme.txt", b"docs\n", 0o644),
    ]);
    let deb = write_deb(&temp.path().join("bad.deb"), &data);
    let rootfs = temp.path().join("rootfs");

    let files = extract_deb_data(&deb, &rootfs).unwrap();
    let guard = AbiGuard::new(MAX_GLIBC_VERSION).unwrap();
    let error = guard
        .check_file(
            "hello-deb",
            "usr/lib/libevil.so",
            &rootfs.join("usr/lib/libevil.so"),
        )
        .unwrap_err();

    assert!(files.contains(&PathBuf::from("usr/lib/libevil.so")));
    assert_eq!(
        error,
        "包 hello-deb 中的 /usr/lib/libevil.so 需要 GLIBC_2.45，但系统基线为 2.44"
    );
}

#[test]
fn rejects_missing_control_member() {
    let temp = TempDir::new().unwrap();
    let deb = temp.path().join("bad.deb");
    let file = File::create(&deb).unwrap();
    let mut builder = ar::Builder::new(file);
    append_ar(&mut builder, b"debian-binary", b"2.0\n");
    append_ar(&mut builder, b"data.tar.xz", &xz(&good_data()));
    builder.into_inner().unwrap();

    let error = parse_deb_metadata(&deb).unwrap_err();

    assert!(error.contains("control.tar"), "{error}");
}

#[test]
fn reads_debian_maintainer_scripts() {
    let temp = TempDir::new().unwrap();
    let deb = write_deb(&temp.path().join("hello.deb"), &good_data());

    let hooks = read_deb_hooks(&deb).unwrap();

    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].phase, "post_install");
    assert!(String::from_utf8_lossy(&hooks[0].script).contains("deb-postinst"));
}

#[test]
fn extracts_relative_symlinks_with_rootfs_validation() {
    let temp = TempDir::new().unwrap();
    let mut builder = Builder::new(Vec::new());
    let mut file = Header::new_gnu();
    file.set_size(11);
    file.set_mode(0o755);
    builder
        .append_data(&mut file, "./usr/lib/libhello.so.1", &b"hello-link\n"[..])
        .unwrap();
    let mut link = Header::new_gnu();
    link.set_entry_type(EntryType::Symlink);
    link.set_size(0);
    link.set_mode(0o777);
    builder
        .append_link(&mut link, "./usr/lib/libhello.so", "libhello.so.1")
        .unwrap();
    let data = builder.into_inner().unwrap();
    let deb = write_deb(&temp.path().join("link.deb"), &data);
    let rootfs = temp.path().join("rootfs");

    let files = extract_deb_data(&deb, &rootfs).unwrap();

    assert!(files.contains(&PathBuf::from("usr/lib/libhello.so")));
    assert_eq!(
        fs::read_link(rootfs.join("usr/lib/libhello.so")).unwrap(),
        PathBuf::from("libhello.so.1")
    );
}

#[test]
fn rejects_absolute_symlink_targets() {
    let temp = TempDir::new().unwrap();
    let mut builder = Builder::new(Vec::new());
    let mut link = Header::new_gnu();
    link.set_entry_type(EntryType::Symlink);
    link.set_size(0);
    link.set_mode(0o777);
    builder
        .append_link(&mut link, "./usr/lib/libhello.so", "/usr/lib/libhello.so.1")
        .unwrap();
    let data = builder.into_inner().unwrap();
    let deb = write_deb(&temp.path().join("absolute.deb"), &data);

    let error = extract_deb_data(&deb, &temp.path().join("rootfs")).unwrap_err();

    assert!(error.contains("绝对路径"), "{error}");
}
