//! 验证包解包和发布阶段不会丢失 SUID/SGID 与扩展属性。

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use base64::Engine;
use tar::{Builder, Header};
use tempfile::TempDir;
use xrpkg::capability::Version;
use xrpkg::compat_arch::{extract_arch_data, parse_arch_metadata};
use xrpkg::solver::SelectedPackage;
use xrpkg::xpkg::{install_package, publish_files, Rootfs};

const METADATA: &str = "name = \"meta-test\"\nversion = \"1.0\"\nrequires = []\n";
const PKGINFO: &str = "pkgname = meta-arch\npkgver = 1.0-1\n";

fn append(builder: &mut Builder<Vec<u8>>, path: &str, bytes: &[u8], mode: u32) {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(mode);
    builder.append_data(&mut header, path, bytes).unwrap();
}

fn xattr_pax(name: &str, value: &[u8]) -> (String, Vec<u8>) {
    (
        format!(
            "LIBARCHIVE.xattr.{}",
            name.replace('.', "%2E").replace('-', "%2D")
        ),
        base64::engine::general_purpose::STANDARD
            .encode(value)
            .into_bytes(),
    )
}

type EntrySpec<'a> = (&'a str, &'a [u8], u32, Option<(String, Vec<u8>)>);

fn write_xpkg(path: &Path, entries: &[EntrySpec<'_>]) {
    let mut builder = Builder::new(Vec::new());
    append(&mut builder, "metadata.toml", METADATA.as_bytes(), 0o644);
    for (name, contents, mode, pax) in entries {
        if let Some((key, value)) = pax {
            builder
                .append_pax_extensions([(key.as_str(), value.as_slice())])
                .unwrap();
        }
        append(&mut builder, name, contents, *mode);
    }
    let tar = builder.into_inner().unwrap();
    let file = fs::File::create(path).unwrap();
    let mut encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    encoder.write_all(&tar).unwrap();
    encoder.finish().unwrap();
}

fn selected() -> SelectedPackage {
    SelectedPackage {
        name: "meta-test".to_string(),
        version: Version::parse("1.0").unwrap(),
    }
}

#[test]
fn xpkg_install_and_publish_keep_suid_sgid_and_xattrs() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("meta-test-1.0.xpkg");
    let pax = Some(xattr_pax("user.xrpkg.meta", b"blue"));
    write_xpkg(
        &archive,
        &[
            ("files/usr/bin/suid-test", b"#!/bin/sh\nid\n", 0o4755, None),
            ("files/usr/bin/sgid-test", b"#!/bin/sh\nid\n", 0o2755, None),
            ("files/usr/bin/xattr-test", b"payload", 0o755, pax),
            (
                "files/usr/bin/schily-test",
                b"payload",
                0o755,
                Some((
                    "SCHILY.xattr.user.xrpkg.schily".to_string(),
                    b"raw-value".to_vec(),
                )),
            ),
        ],
    );

    let staging = Rootfs::prepare(temp.path().join("staging")).unwrap();
    let installed = install_package(&archive, &selected(), &staging).unwrap();
    assert_eq!(installed.len(), 4);

    let suid = staging.path().join("usr/bin/suid-test");
    let sgid = staging.path().join("usr/bin/sgid-test");
    let xattr_file = staging.path().join("usr/bin/xattr-test");
    assert_eq!(
        fs::metadata(&suid).unwrap().permissions().mode() & 0o7777,
        0o4755
    );
    assert_eq!(
        fs::metadata(&sgid).unwrap().permissions().mode() & 0o7777,
        0o2755
    );
    assert_eq!(
        xattr::get(&xattr_file, "user.xrpkg.meta").unwrap(),
        Some(b"blue".to_vec())
    );
    assert_eq!(
        xattr::get(
            staging.path().join("usr/bin/schily-test"),
            "user.xrpkg.schily"
        )
        .unwrap(),
        Some(b"raw-value".to_vec())
    );

    let target = Rootfs::prepare(temp.path().join("target")).unwrap();
    publish_files(&staging, &target, &installed).unwrap();
    let published_suid = target.path().join("usr/bin/suid-test");
    let published_sgid = target.path().join("usr/bin/sgid-test");
    let published_xattr = target.path().join("usr/bin/xattr-test");
    assert_eq!(
        fs::metadata(&published_suid).unwrap().permissions().mode() & 0o7777,
        0o4755
    );
    assert_eq!(
        fs::metadata(&published_sgid).unwrap().permissions().mode() & 0o7777,
        0o2755
    );
    assert_eq!(
        xattr::get(&published_xattr, "user.xrpkg.meta").unwrap(),
        Some(b"blue".to_vec())
    );
}

#[test]
fn arch_extraction_keeps_suid_and_libarchive_xattrs() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("meta-arch.pkg.tar.zst");
    let mut builder = Builder::new(Vec::new());
    append(&mut builder, ".PKGINFO", PKGINFO.as_bytes(), 0o644);
    let (key, value) = xattr_pax("user.xrpkg.arch", b"green");
    builder
        .append_pax_extensions([(key.as_str(), value.as_slice())])
        .unwrap();
    append(
        &mut builder,
        "usr/bin/ping-test",
        b"\x7fELF-not-really\n",
        0o4755,
    );
    let tar = builder.into_inner().unwrap();
    let file = fs::File::create(&archive).unwrap();
    let mut encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    encoder.write_all(&tar).unwrap();
    encoder.finish().unwrap();

    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    let files = extract_arch_data(&archive, rootfs.path()).unwrap();
    assert_eq!(files, [PathBuf::from("usr/bin/ping-test")]);
    let ping = rootfs.path().join("usr/bin/ping-test");
    assert_eq!(
        fs::metadata(&ping).unwrap().permissions().mode() & 0o7777,
        0o4755
    );
    assert_eq!(
        xattr::get(&ping, "user.xrpkg.arch").unwrap(),
        Some(b"green".to_vec())
    );

    let metadata = parse_arch_metadata(&archive).unwrap();
    assert_eq!(metadata.pkgname, "meta-arch");
}

/// 该测试只在 root 下运行：`security.capability` 需要 CAP_SETFCAP。
#[test]
fn security_capability_survives_install_and_publish_when_root() {
    if !xrpkg::fs_meta::is_root() {
        eprintln!("skipped: security.capability 需要 root");
        return;
    }

    let temp = TempDir::new().unwrap();
    let capability =
        b"\x01\x00\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
    let source = temp.path().join("ping");
    fs::write(&source, b"fake ping").unwrap();
    xattr::set(&source, "security.capability", capability).unwrap();

    let archive = temp.path().join("meta-test-1.0.xpkg");
    let encoded = base64::engine::general_purpose::STANDARD.encode(capability);
    write_xpkg(
        &archive,
        &[(
            "files/usr/bin/ping",
            b"fake ping",
            0o755,
            Some((
                "LIBARCHIVE.xattr.security.capability".to_string(),
                encoded.into_bytes(),
            )),
        )],
    );

    let staging = Rootfs::prepare(temp.path().join("staging")).unwrap();
    let installed = install_package(&archive, &selected(), &staging).unwrap();
    let target = Rootfs::prepare(temp.path().join("target")).unwrap();
    publish_files(&staging, &target, &installed).unwrap();

    assert_eq!(
        xattr::get(target.path().join("usr/bin/ping"), "security.capability").unwrap(),
        Some(capability.to_vec())
    );
}
