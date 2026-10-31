use std::fs;
use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};
use tar::{Builder, EntryType, Header};
use tempfile::TempDir;
use xrpkg::repo::{self, RepoPackage};
use xrpkg::state::LocalDb;

mod common;

const BASE_ELF: &[u8] = include_bytes!("fixtures/libevil.so");
const PKGINFO: &str = "pkgname = arch-hello\npkgver = 1.0-1\narch = x86_64\ndepend = glibc\n";

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn append(builder: &mut Builder<Vec<u8>>, path: &str, contents: &[u8], mode: u32) {
    let mut header = Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(mode);
    builder.append_data(&mut header, path, contents).unwrap();
}

fn arch_package() -> Vec<u8> {
    let mut builder = Builder::new(Vec::new());
    append(&mut builder, ".PKGINFO", PKGINFO.as_bytes(), 0o644);
    append(
        &mut builder,
        "usr/bin/arch-hello",
        b"#!/bin/sh\necho arch hello\n",
        0o755,
    );
    append(&mut builder, "usr/lib/libevil.so", BASE_ELF, 0o644);

    let mut symlink = Header::new_gnu();
    symlink.set_size(0);
    symlink.set_mode(0o777);
    symlink.set_entry_type(EntryType::Symlink);
    builder
        .append_link(&mut symlink, "usr/lib/libevil.so.1", "libevil.so")
        .unwrap();

    let tar = builder.into_inner().unwrap();
    zstd::stream::encode_all(&tar[..], 3).unwrap()
}

#[test]
fn installs_arch_package_from_repository_and_runs_it() {
    let temp = TempDir::new().unwrap();
    let repo_dir = temp.path().join("repo");
    let mirror = temp.path().join("mirror");
    fs::create_dir_all(&repo_dir).unwrap();
    fs::create_dir_all(&mirror).unwrap();

    let gpg = common::TestGpg::new(temp.path());
    let config = temp.path().join("repos.toml");
    common::write_config(&config, &[("repo", &repo_dir)], &gpg.keyring);

    let filename = "arch-hello-1.0-1-x86_64.pkg.tar.zst";
    let package = arch_package();
    fs::write(repo_dir.join(filename), &package).unwrap();
    fs::write(mirror.join(filename), &package).unwrap();
    repo::add_index_entries(
        &repo_dir,
        &[RepoPackage {
            name: "arch-hello".to_string(),
            version: "1.0-1".to_string(),
            requires: vec!["glibc".to_string()],
            filename: filename.to_string(),
            sha256: sha256(&package),
            origin: Some("arch".to_string()),
        }],
    )
    .unwrap();
    gpg.sign_repo(&repo_dir);

    let mirrors = temp.path().join("mirrors.toml");
    fs::write(
        &mirrors,
        format!(
            "[[mirrors]]\nname = \"broken\"\nurl = \"{}\"\npriority = 1\n\n[[mirrors]]\nname = \"local\"\nurl = \"{}\"\npriority = 2\n",
            temp.path().join("missing").display(),
            mirror.display()
        ),
    )
    .unwrap();

    let db_path = temp.path().join("local.db");
    let rootfs = temp.path().join("rootfs");
    let cache = temp.path().join("cache");
    repo::update(&config, &cache).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_xrpkg"))
        .args([
            "install",
            "arch-hello",
            "--config",
            config.to_str().unwrap(),
            "--mirrors",
            mirrors.to_str().unwrap(),
            "--db",
            db_path.to_str().unwrap(),
            "--root",
            rootfs.to_str().unwrap(),
            "--cache",
            cache.to_str().unwrap(),
            "--prefer-arch",
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout={stdout}\nstderr={stderr}");
    assert!(stdout.contains("从镜像 local 下载 Arch 包"), "{stdout}");
    assert!(stdout.contains("ABI 校验通过"), "{stdout}");
    assert!(stdout.contains("兼容轨道：已写入"), "{stdout}");
    assert!(stdout.contains("[arch]"), "{stdout}");
    assert!(
        !stdout.contains("\x1b[") && !stderr.contains("\x1b["),
        "非 TTY 不应输出 ANSI 控制序列"
    );
    println!("--- install stdout ---\n{stdout}");
    println!("--- install stderr ---\n{stderr}");

    let run = Command::new(rootfs.join("usr/bin/arch-hello"))
        .output()
        .unwrap();
    assert!(run.status.success(), "运行安装后的脚本失败");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "arch hello\n");
    assert_eq!(
        fs::read_link(rootfs.join("usr/lib/libevil.so.1")).unwrap(),
        Path::new("libevil.so")
    );
    assert!(rootfs.join("usr/lib/libevil.so").is_file());

    let db = LocalDb::open(&db_path).unwrap();
    let installed = db.list_installed().unwrap();
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].name, "arch-hello");
    assert_eq!(installed[0].version_raw, "1.0-1");
    assert_eq!(installed[0].origin, "arch");
    let files = db.list_files("arch-hello").unwrap();
    assert!(files.iter().any(|file| file == "usr/lib/libevil.so.1"));
    assert!(files.iter().any(|file| file == "usr/bin/arch-hello"));
}
