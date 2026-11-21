//! CLI 端到端：`xrpkg install ./local.xpkg`。

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use tar::{Builder, Header};
use tempfile::TempDir;
use xrpkg::state::LocalDb;

fn build_local_xpkg(path: &Path) {
    let metadata = "name = \"local-hello\"\nversion = \"1.0\"\nrequires = []\n";
    let mut builder = Builder::new(Vec::new());
    let mut header = Header::new_gnu();
    header.set_size(metadata.len() as u64);
    header.set_mode(0o644);
    builder
        .append_data(&mut header, "metadata.toml", metadata.as_bytes())
        .unwrap();

    let mut header = Header::new_gnu();
    header.set_size(18);
    header.set_mode(0o755);
    builder
        .append_data(
            &mut header,
            "files/usr/bin/local-hello",
            &b"#!/bin/sh\necho hi\n"[..],
        )
        .unwrap();

    let tar = builder.into_inner().unwrap();
    let file = fs::File::create(path).unwrap();
    let mut encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    encoder.write_all(&tar).unwrap();
    encoder.finish().unwrap();
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_xrpkg"))
        .args(args)
        .env("XRPKG_ASCII", "1")
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

#[test]
fn installs_a_local_xpkg_and_is_idempotent() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("local-hello-1.0.xpkg");
    build_local_xpkg(&archive);
    let rootfs = temp.path().join("rootfs");
    let db_path = temp.path().join("local.db");

    let output = run(&[
        "install",
        archive.to_str().unwrap(),
        "--root",
        rootfs.to_str().unwrap(),
        "--db",
        db_path.to_str().unwrap(),
        "--no-hooks",
    ]);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(rootfs.join("usr/bin/local-hello")).unwrap(),
        b"#!/bin/sh\necho hi\n"
    );

    let db = LocalDb::open(&db_path).unwrap();
    let installed = db.list_installed().unwrap();
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].name, "local-hello");
    assert_eq!(installed[0].origin, "native");
    assert_eq!(
        db.list_files("local-hello").unwrap(),
        ["usr/bin/local-hello"]
    );

    let output = run(&[
        "install",
        archive.to_str().unwrap(),
        "--root",
        rootfs.to_str().unwrap(),
        "--db",
        db_path.to_str().unwrap(),
        "--no-hooks",
    ]);
    assert!(output.status.success());
    assert_eq!(
        LocalDb::open(&db_path)
            .unwrap()
            .list_installed()
            .unwrap()
            .len(),
        1
    );
}
