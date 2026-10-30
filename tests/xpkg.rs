use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tar::{Builder, EntryType, Header};
use tempfile::TempDir;
use xrpkg::capability::Version;
use xrpkg::solver::SelectedPackage;
use xrpkg::xpkg::{install_package, read_metadata, Rootfs};

const APP_METADATA: &str = r#"
name = "app"
version = "1.0"
requires = ["libbar >= 1.0"]
"#;

fn selected(name: &str, version_input: &str) -> SelectedPackage {
    SelectedPackage {
        name: name.to_string(),
        version: Version::parse(version_input).unwrap(),
    }
}

fn finish_archive(builder: Builder<zstd::stream::write::Encoder<'static, fs::File>>) {
    let encoder = builder.into_inner().unwrap();
    encoder.finish().unwrap();
}

fn write_xpkg(path: &Path, metadata: &str, files: &[(&str, &[u8], u32)]) {
    let file = fs::File::create(path).unwrap();
    let encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let mut builder = Builder::new(encoder);

    let mut header = Header::new_gnu();
    header.set_size(metadata.len() as u64);
    header.set_mode(0o644);
    builder
        .append_data(&mut header, "metadata.toml", metadata.as_bytes())
        .unwrap();

    for (relative_path, contents, mode) in files {
        let mut header = Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(*mode);
        builder
            .append_data(&mut header, format!("files/{relative_path}"), *contents)
            .unwrap();
    }

    finish_archive(builder);
}

fn write_traversal_xpkg(path: &Path, archive_path: &[u8]) {
    let file = fs::File::create(path).unwrap();
    let encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let mut builder = Builder::new(encoder);

    let mut metadata_header = Header::new_gnu();
    metadata_header.set_size(APP_METADATA.len() as u64);
    metadata_header.set_mode(0o644);
    builder
        .append_data(
            &mut metadata_header,
            "metadata.toml",
            APP_METADATA.as_bytes(),
        )
        .unwrap();

    let payload = b"escaped\n";
    let mut header = Header::new_gnu();
    header.set_size(payload.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(EntryType::Regular);
    header.as_mut_bytes()[..archive_path.len()].copy_from_slice(archive_path);
    header.set_cksum();
    builder.append(&header, &payload[..]).unwrap();

    finish_archive(builder);
}

fn write_symlink_xpkg(path: &Path, target: &str) {
    let file = fs::File::create(path).unwrap();
    let encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let mut builder = Builder::new(encoder);

    let mut metadata_header = Header::new_gnu();
    metadata_header.set_size(APP_METADATA.len() as u64);
    metadata_header.set_mode(0o644);
    builder
        .append_data(
            &mut metadata_header,
            "metadata.toml",
            APP_METADATA.as_bytes(),
        )
        .unwrap();

    let mut header = Header::new_gnu();
    header.set_size(0);
    header.set_mode(0o777);
    header.set_entry_type(EntryType::Symlink);
    header.set_link_name(target).unwrap();
    builder
        .append_data(&mut header, "files/link", &[][..])
        .unwrap();

    finish_archive(builder);
}

fn write_links_xpkg(path: &Path) {
    let file = fs::File::create(path).unwrap();
    let encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let mut builder = Builder::new(encoder);

    let mut metadata_header = Header::new_gnu();
    metadata_header.set_size(APP_METADATA.len() as u64);
    metadata_header.set_mode(0o644);
    builder
        .append_data(
            &mut metadata_header,
            "metadata.toml",
            APP_METADATA.as_bytes(),
        )
        .unwrap();

    let mut regular = Header::new_gnu();
    regular.set_size(8);
    regular.set_mode(0o755);
    builder
        .append_data(&mut regular, "files/usr/bin/foo", &b"foo-data"[..])
        .unwrap();

    let mut hardlink = Header::new_gnu();
    hardlink.set_size(0);
    hardlink.set_mode(0o755);
    hardlink.set_entry_type(EntryType::Link);
    builder
        .append_link(&mut hardlink, "files/usr/bin/foo-hard", "files/usr/bin/foo")
        .unwrap();

    let mut library = Header::new_gnu();
    library.set_size(8);
    library.set_mode(0o644);
    builder
        .append_data(
            &mut library,
            "files/usr/lib/libfoo.so.1.0",
            &b"lib-data"[..],
        )
        .unwrap();

    let mut soname = Header::new_gnu();
    soname.set_size(0);
    soname.set_mode(0o777);
    soname.set_entry_type(EntryType::Symlink);
    builder
        .append_link(&mut soname, "files/usr/lib/libfoo.so.1", "libfoo.so.1.0")
        .unwrap();

    let mut devlink = Header::new_gnu();
    devlink.set_size(0);
    devlink.set_mode(0o777);
    devlink.set_entry_type(EntryType::Symlink);
    builder
        .append_link(&mut devlink, "files/usr/lib/libfoo.so", "libfoo.so.1")
        .unwrap();

    finish_archive(builder);
}

#[test]
fn reads_metadata_and_installs_files_below_the_rootfs() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("app-1.0.xpkg");
    write_xpkg(
        &archive,
        APP_METADATA,
        &[
            ("bin/app", b"#!/bin/sh\necho app\n", 0o755),
            ("lib/libfoo.so", b"fake library\n", 0o644),
        ],
    );

    let metadata = read_metadata(&archive).unwrap();
    assert_eq!(metadata.name, "app");
    assert_eq!(metadata.version, Version::parse("1.0").unwrap());
    assert_eq!(metadata.requires[0].to_string(), "libbar >= 1.0");

    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    let installed = install_package(&archive, &selected("app", "1.0"), &rootfs).unwrap();

    assert_eq!(installed, ["bin/app", "lib/libfoo.so"]);
    assert_eq!(
        fs::read_to_string(rootfs.path().join("bin/app")).unwrap(),
        "#!/bin/sh\necho app\n"
    );
    let mode = fs::metadata(rootfs.path().join("bin/app"))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0);
    assert_eq!(
        fs::read_to_string(rootfs.path().join("lib/libfoo.so")).unwrap(),
        "fake library\n"
    );
}

#[test]
fn rejects_parent_directory_path_traversal() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("evil.xpkg");
    write_traversal_xpkg(&archive, b"files/../../escape.txt");
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();

    let error = install_package(&archive, &selected("app", "1.0"), &rootfs).unwrap_err();

    assert!(error.contains("非法路径"), "{error}");
    assert!(!temp.path().join("escape.txt").exists());
}

#[test]
fn rejects_absolute_paths() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("absolute.xpkg");
    write_traversal_xpkg(&archive, b"/tmp/xrpkg-escape.txt");
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();

    let error = install_package(&archive, &selected("app", "1.0"), &rootfs).unwrap_err();

    assert!(error.contains("非法路径"), "{error}");
    assert!(!PathBuf::from("/tmp/xrpkg-escape.txt").exists());
}

#[test]
fn rejects_symlink_target_escape() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("symlink.xpkg");
    write_symlink_xpkg(&archive, "../../outside");
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();

    let error = install_package(&archive, &selected("app", "1.0"), &rootfs).unwrap_err();

    assert!(error.contains("逃出 Rootfs"), "{error}");
}

#[test]
fn rejects_absolute_symlink_target() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("symlink-abs.xpkg");
    write_symlink_xpkg(&archive, "/etc/passwd");
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();

    let error = install_package(&archive, &selected("app", "1.0"), &rootfs).unwrap_err();

    assert!(error.contains("绝对路径"), "{error}");
}

#[test]
fn rejects_hardlink_target_escape() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("hardlink-escape.xpkg");
    let file = fs::File::create(&archive).unwrap();
    let encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let mut builder = Builder::new(encoder);

    let mut metadata_header = Header::new_gnu();
    metadata_header.set_size(APP_METADATA.len() as u64);
    metadata_header.set_mode(0o644);
    builder
        .append_data(
            &mut metadata_header,
            "metadata.toml",
            APP_METADATA.as_bytes(),
        )
        .unwrap();

    let mut header = Header::new_gnu();
    header.set_size(0);
    header.set_mode(0o644);
    header.set_entry_type(EntryType::Link);
    builder
        .append_link(&mut header, "files/link", "files/../../outside")
        .unwrap();

    finish_archive(builder);
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();

    let error = install_package(&archive, &selected("app", "1.0"), &rootfs).unwrap_err();

    assert!(error.contains("非法"), "{error}");
}

#[test]
fn installs_and_publishes_symlinks_and_hardlinks() {
    use std::os::unix::fs::MetadataExt;
    use xrpkg::xpkg::publish_files;

    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("links.xpkg");
    write_links_xpkg(&archive);
    let staging = Rootfs::prepare(temp.path().join("staging")).unwrap();

    let installed = install_package(&archive, &selected("app", "1.0"), &staging).unwrap();

    assert_eq!(
        installed,
        [
            "usr/bin/foo",
            "usr/bin/foo-hard",
            "usr/lib/libfoo.so",
            "usr/lib/libfoo.so.1",
            "usr/lib/libfoo.so.1.0"
        ]
    );
    assert_eq!(
        fs::read_link(staging.path().join("usr/lib/libfoo.so")).unwrap(),
        Path::new("libfoo.so.1")
    );
    assert_eq!(
        fs::read_link(staging.path().join("usr/lib/libfoo.so.1")).unwrap(),
        Path::new("libfoo.so.1.0")
    );
    assert_eq!(
        fs::metadata(staging.path().join("usr/bin/foo"))
            .unwrap()
            .ino(),
        fs::metadata(staging.path().join("usr/bin/foo-hard"))
            .unwrap()
            .ino()
    );

    let target = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    publish_files(&staging, &target, &installed).unwrap();

    assert_eq!(
        fs::read_link(target.path().join("usr/lib/libfoo.so")).unwrap(),
        Path::new("libfoo.so.1")
    );
    assert_eq!(
        fs::read_to_string(target.path().join("usr/bin/foo-hard")).unwrap(),
        "foo-data"
    );
    assert_eq!(
        fs::metadata(target.path().join("usr/bin/foo"))
            .unwrap()
            .ino(),
        fs::metadata(target.path().join("usr/bin/foo-hard"))
            .unwrap()
            .ino()
    );
}

#[test]
fn rejects_metadata_version_mismatch() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("app-1.0.xpkg");
    write_xpkg(&archive, APP_METADATA, &[("bin/app", b"app\n", 0o755)]);
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();

    let error = install_package(&archive, &selected("app", "2.0"), &rootfs).unwrap_err();

    assert!(error.contains("版本"), "{error}");
}

#[test]
fn refuses_to_use_the_host_root_as_rootfs() {
    assert!(Rootfs::prepare("/").is_err());
}
