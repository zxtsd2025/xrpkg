use std::fs::{self, File};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::write::GzEncoder;
use flate2::Compression;
use tar::{Builder, Header};
use tempfile::TempDir;
use xrpkg::state::LocalDb;

const CONTROL: &str = "Package: deb-hook-test\nVersion: 1.0-1\nArchitecture: amd64\nDepends: libssl3 (>= 1.1)\nDescription: hook and origin test\n";

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

fn append_ar(builder: &mut ar::Builder<File>, name: &[u8], data: &[u8]) {
    let header = ar::Header::new(name.to_vec(), data.len() as u64);
    builder.append(&header, Cursor::new(data)).unwrap();
}

fn write_deb(path: &Path) -> PathBuf {
    write_deb_with_control(path, CONTROL)
}

fn write_deb_with_control(path: &Path, control: &str) -> PathBuf {
    let control_tar = tar_bytes(&[
        ("./control", control.as_bytes(), 0o644),
        (
            "./postinst",
            b"#!/bin/sh\nmkdir -p \"$XRPKG_ROOTFS/etc\"\necho installed > \"$XRPKG_ROOTFS/etc/deb-hook.txt\"\n",
            0o755,
        ),
        (
            "./postrm",
            b"#!/bin/sh\nrm -f \"$XRPKG_ROOTFS/etc/deb-hook.txt\"\n",
            0o755,
        ),
    ]);
    let data_tar = tar_bytes(&[(
        "./usr/share/deb-hook-test/file.txt",
        b"deb payload\n",
        0o644,
    )]);
    let file = File::create(path).unwrap();
    let mut builder = ar::Builder::new(file);
    append_ar(&mut builder, b"debian-binary", b"2.0\n");
    append_ar(&mut builder, b"control.tar.gz", &gzip(&control_tar));
    append_ar(&mut builder, b"data.tar.xz", &xz(&data_tar));
    builder.into_inner().unwrap();
    path.to_path_buf()
}

fn write_deb_custom(
    path: &Path,
    package: &str,
    version: &str,
    data_file: &str,
    data_contents: &[u8],
    scripts: &[(&str, &[u8])],
) -> PathBuf {
    let control = format!(
        "Package: {package}\nVersion: {version}\nArchitecture: amd64\nDepends: libssl3 (>= 1.1)\nDescription: upgrade test\n"
    );
    let mut control_entries: Vec<(&str, &[u8], u32)> =
        vec![("./control", control.as_bytes(), 0o644)];
    for (name, contents) in scripts {
        control_entries.push((name, contents, 0o755));
    }
    let control_tar = tar_bytes(&control_entries);
    let data_tar = tar_bytes(&[(data_file, data_contents, 0o644)]);
    let file = File::create(path).unwrap();
    let mut builder = ar::Builder::new(file);
    append_ar(&mut builder, b"debian-binary", b"2.0\n");
    append_ar(&mut builder, b"control.tar.gz", &gzip(&control_tar));
    append_ar(&mut builder, b"data.tar.xz", &xz(&data_tar));
    builder.into_inner().unwrap();
    path.to_path_buf()
}

fn run(bin: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin)
        .args(args)
        .output()
        .expect("运行 xrpkg 失败")
}

#[test]
fn deb_install_records_origin_files_and_hooks_then_remove_runs_postrm() {
    let temp = TempDir::new().unwrap();
    let deb = write_deb(&temp.path().join("deb-hook-test.deb"));
    let rootfs = temp.path().join("rootfs");
    let db_path = temp.path().join("local.db");
    let bin = Path::new(env!("CARGO_BIN_EXE_xrpkg"));
    let deb_arg = deb.to_string_lossy().into_owned();
    let rootfs_arg = rootfs.to_string_lossy().into_owned();
    let db_arg = db_path.to_string_lossy().into_owned();

    let output = run(
        bin,
        &["install", &deb_arg, "--root", &rootfs_arg, "--db", &db_arg],
    );
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(rootfs.join("etc/deb-hook.txt")).unwrap(),
        "installed\n"
    );
    assert!(rootfs.join("usr/share/deb-hook-test/file.txt").is_file());

    let db = LocalDb::open(&db_path).unwrap();
    let installed = db.list_installed().unwrap();
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].name, "deb-hook-test");
    assert_eq!(installed[0].version_raw, "1.0-1");
    assert_eq!(installed[0].origin, "deb");
    assert_eq!(
        db.list_files("deb-hook-test").unwrap(),
        ["usr/share/deb-hook-test/file.txt"]
    );
    let hook_phases = db
        .list_hooks("deb-hook-test")
        .unwrap()
        .into_iter()
        .map(|hook| hook.phase)
        .collect::<Vec<_>>();
    assert!(hook_phases.contains(&"post_install".to_string()));
    assert!(hook_phases.contains(&"post_remove".to_string()));

    let output = run(
        bin,
        &[
            "--remove",
            "deb-hook-test",
            "--root",
            &rootfs_arg,
            "--db",
            &db_arg,
        ],
    );
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!rootfs.join("etc/deb-hook.txt").exists());
    assert!(!rootfs.join("usr/share/deb-hook-test/file.txt").exists());
    let db = LocalDb::open(&db_path).unwrap();
    assert!(db.list_installed().unwrap().is_empty());
}
#[test]
fn unsatisfied_dependency_aborts_before_touching_database() {
    let temp = TempDir::new().unwrap();
    let control = "Package: deb-fail\nVersion: 1.0-1\nArchitecture: amd64\nDepends: libdoesnotexist\nDescription: missing dependency\n";
    let deb = write_deb_with_control(&temp.path().join("deb-fail.deb"), control);
    let rootfs = temp.path().join("rootfs");
    let db_path = temp.path().join("local.db");
    let bin = Path::new(env!("CARGO_BIN_EXE_xrpkg"));

    let output = run(
        bin,
        &[
            "install",
            &deb.to_string_lossy(),
            "--root",
            &rootfs.to_string_lossy(),
            "--db",
            &db_path.to_string_lossy(),
        ],
    );

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("libdoesnotexist")
            || String::from_utf8_lossy(&output.stderr).contains("无解"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let db = LocalDb::open(&db_path).unwrap();
    assert!(db.list_installed().unwrap().is_empty());
    assert!(!rootfs.join("usr/share/deb-hook-test/file.txt").exists());
}

#[test]
fn deb_upgrade_runs_old_prerm_and_new_preinst_and_removes_obsolete_file() {
    let temp = TempDir::new().unwrap();
    let rootfs = temp.path().join("rootfs");
    let db_path = temp.path().join("local.db");
    let bin = Path::new(env!("CARGO_BIN_EXE_xrpkg"));
    let rootfs_arg = rootfs.to_string_lossy().into_owned();
    let db_arg = db_path.to_string_lossy().into_owned();

    let old = write_deb_custom(
        &temp.path().join("deb-upgrade-1.0.deb"),
        "deb-upgrade",
        "1.0-1",
        "./usr/share/deb-upgrade/old.txt",
        b"old\n",
        &[
            (
                "./prerm",
                b"#!/bin/sh\necho old-prerm > \"$XRPKG_ROOTFS/etc/prerm-ran.txt\"\n",
            ),
            (
                "./postinst",
                b"#!/bin/sh\nmkdir -p \"$XRPKG_ROOTFS/etc\"\necho v1 > \"$XRPKG_ROOTFS/etc/deb-version.txt\"\n",
            ),
        ],
    );
    let output = run(
        bin,
        &[
            "install",
            &old.to_string_lossy(),
            "--root",
            &rootfs_arg,
            "--db",
            &db_arg,
        ],
    );
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let new = write_deb_custom(
        &temp.path().join("deb-upgrade-2.0.deb"),
        "deb-upgrade",
        "2.0-1",
        "./usr/share/deb-upgrade/new.txt",
        b"new\n",
        &[
            (
                "./preinst",
                b"#!/bin/sh\necho new-preinst > \"$XRPKG_ROOTFS/etc/preinst-ran.txt\"\n",
            ),
            (
                "./postinst",
                b"#!/bin/sh\necho v2 > \"$XRPKG_ROOTFS/etc/deb-version.txt\"\n",
            ),
        ],
    );
    let output = run(
        bin,
        &[
            "install",
            &new.to_string_lossy(),
            "--root",
            &rootfs_arg,
            "--db",
            &db_arg,
        ],
    );
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!rootfs.join("usr/share/deb-upgrade/old.txt").exists());
    assert!(rootfs.join("usr/share/deb-upgrade/new.txt").is_file());
    assert_eq!(
        fs::read_to_string(rootfs.join("etc/prerm-ran.txt")).unwrap(),
        "old-prerm\n"
    );
    assert_eq!(
        fs::read_to_string(rootfs.join("etc/preinst-ran.txt")).unwrap(),
        "new-preinst\n"
    );
    assert_eq!(
        fs::read_to_string(rootfs.join("etc/deb-version.txt")).unwrap(),
        "v2\n"
    );
    let db = LocalDb::open(&db_path).unwrap();
    let installed = db.list_installed().unwrap();
    assert_eq!(installed[0].version_raw, "2.0-1");
    assert_eq!(installed[0].origin, "deb");
}

#[test]
fn failed_external_upgrade_restores_old_files_and_database() {
    let temp = TempDir::new().unwrap();
    let rootfs = temp.path().join("rootfs");
    let db_path = temp.path().join("local.db");
    let bin = Path::new(env!("CARGO_BIN_EXE_xrpkg"));
    let rootfs_arg = rootfs.to_string_lossy().into_owned();
    let db_arg = db_path.to_string_lossy().into_owned();

    let old = write_deb_custom(
        &temp.path().join("deb-rollback-1.0.deb"),
        "deb-rollback",
        "1.0-1",
        "./usr/share/deb-rollback/old.txt",
        b"old\n",
        &[],
    );
    let output = run(
        bin,
        &[
            "install",
            &old.to_string_lossy(),
            "--root",
            &rootfs_arg,
            "--db",
            &db_arg,
        ],
    );
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let new = write_deb_custom(
        &temp.path().join("deb-rollback-2.0.deb"),
        "deb-rollback",
        "2.0-1",
        "./usr/share/deb-rollback/new.txt",
        b"new\n",
        &[("./postinst", b"#!/bin/sh\nexit 1\n")],
    );
    let output = run(
        bin,
        &[
            "install",
            &new.to_string_lossy(),
            "--root",
            &rootfs_arg,
            "--db",
            &db_arg,
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("回滚"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        fs::read_to_string(rootfs.join("usr/share/deb-rollback/old.txt")).unwrap(),
        "old\n"
    );
    assert!(!rootfs.join("usr/share/deb-rollback/new.txt").exists());
    let db = LocalDb::open(&db_path).unwrap();
    let installed = db.list_installed().unwrap();
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].version_raw, "1.0-1");
    assert_eq!(
        db.list_files("deb-rollback").unwrap(),
        ["usr/share/deb-rollback/old.txt"]
    );
}
