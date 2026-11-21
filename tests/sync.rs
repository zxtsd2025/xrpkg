mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};
use tar::{Builder, Header};
use tempfile::TempDir;
use xrpkg::repo::RepoIndex;
use xrpkg::sync;

const BASE_ELF: &[u8] = include_bytes!("fixtures/libevil.so");

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn append<W: std::io::Write>(builder: &mut Builder<W>, path: &str, contents: &[u8]) {
    let mut header = Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o644);
    builder.append_data(&mut header, path, contents).unwrap();
}

/// 构造一个最小 Arch 包：根目录 `.PKGINFO` 加上若干数据文件。
fn arch_package(name: &str, version: &str, depends: &[&str], files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut pkginfo = format!("pkgname = {name}\npkgver = {version}\narch = x86_64\n");
    for dependency in depends {
        pkginfo.push_str(&format!("depend = {dependency}\n"));
    }

    let mut builder = Builder::new(Vec::new());
    append(&mut builder, ".PKGINFO", pkginfo.as_bytes());
    for (path, contents) in files {
        append(&mut builder, path, contents);
    }
    let tar = builder.into_inner().unwrap();
    zstd::stream::encode_all(&tar[..], 3).unwrap()
}

fn desc(name: &str, version: &str, filename: &str, sha256: &str, depends: &[&str]) -> String {
    let mut text = format!("%FILENAME%\n{filename}\n\n%NAME%\n{name}\n\n%VERSION%\n{version}\n\n");
    if !depends.is_empty() {
        text.push_str("%DEPENDS%\n");
        for dependency in depends {
            text.push_str(dependency);
            text.push('\n');
        }
        text.push('\n');
    }
    text.push_str(&format!("%SHA256SUM%\n{sha256}\n"));
    text
}

fn write_core_db(path: &Path, entries: &[(&str, String)]) {
    let file = fs::File::create(path).unwrap();
    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = Builder::new(encoder);
    for (directory, desc) in entries {
        append(&mut builder, &format!("{directory}/desc"), desc.as_bytes());
    }
    let encoder = builder.into_inner().unwrap();
    encoder.finish().unwrap();
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
fn syncs_good_packages_and_filters_baseline_violations() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let mirror = temp.path().join("mirror");
    fs::create_dir_all(&mirror).unwrap();
    let repo_dir = temp.path().join("repo");
    let deps_map = temp.path().join("deps_map.toml");
    fs::write(
        &deps_map,
        "[mappings]\n\"libc6\" = \"glibc >= 2.44\"\n\"libc6-new\" = \"glibc >= 2.45\"\n",
    )
    .unwrap();

    let good_name = "hello-1.0-1-x86_64.pkg.tar.zst";
    let filtered_name = "needs-new-glibc-2.0-1-x86_64.pkg.tar.zst";
    let abi_name = "bad-abi-3.0-1-x86_64.pkg.tar.zst";

    let good_bytes = arch_package(
        "hello",
        "1.0-1",
        &["libc6", "libfoo"],
        &[("usr/bin/hello", b"#!/bin/sh\n")],
    );
    let filtered_bytes = arch_package("needs-new-glibc", "2.0-1", &["libc6-new"], &[]);
    let bad_elf_bytes = bad_elf();
    let abi_bytes = arch_package(
        "bad-abi",
        "3.0-1",
        &["glibc"],
        &[("usr/lib/libevil.so", bad_elf_bytes.as_slice())],
    );

    fs::write(mirror.join(good_name), &good_bytes).unwrap();
    fs::write(mirror.join(filtered_name), &filtered_bytes).unwrap();
    fs::write(mirror.join(abi_name), &abi_bytes).unwrap();

    write_core_db(
        &mirror.join("core.db"),
        &[
            (
                "hello-1.0-1",
                desc(
                    "hello",
                    "1.0-1",
                    good_name,
                    &sha256(&good_bytes),
                    &["libc6", "libfoo"],
                ),
            ),
            (
                "needs-new-glibc-2.0-1",
                desc(
                    "needs-new-glibc",
                    "2.0-1",
                    filtered_name,
                    &sha256(&filtered_bytes),
                    &["libc6-new"],
                ),
            ),
            (
                "bad-abi-3.0-1",
                desc(
                    "bad-abi",
                    "3.0-1",
                    abi_name,
                    &sha256(&abi_bytes),
                    &["glibc"],
                ),
            ),
        ],
    );

    let report = sync::sync_mirror_with_options(
        mirror.to_str().unwrap(),
        &repo_dir,
        &deps_map,
        &sync::SyncOptions { jobs: 2 },
    )
    .unwrap();

    assert_eq!(report.synced, 1);
    assert_eq!(report.skipped_dependencies, 1);
    assert_eq!(report.skipped_abi, 1);
    assert_eq!(report.failed, 0);
    assert!(repo_dir.join(good_name).is_file());
    assert!(!repo_dir.join(filtered_name).exists());
    assert!(!repo_dir.join(abi_name).exists());

    let text = fs::read_to_string(repo_dir.join("xrpkg-repo.toml")).unwrap();
    let index: RepoIndex = toml::from_str(&text).unwrap();
    assert_eq!(index.packages.len(), 1);
    assert_eq!(index.packages[0].name, "hello");
    assert_eq!(index.packages[0].version, "1.0-1");
    assert_eq!(index.packages[0].filename, good_name);
    assert_eq!(index.packages[0].origin.as_deref(), Some("arch"));
    assert_eq!(index.packages[0].requires, ["glibc >= 2.44", "libfoo"]);

    for entry in fs::read_dir(&repo_dir).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.contains(".part-"), "残留临时文件: {name}");
    }
}

#[test]
fn abi_guard_blocks_bad_elf_from_entering_repo() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let mirror = temp.path().join("mirror");
    fs::create_dir_all(&mirror).unwrap();
    let repo_dir = temp.path().join("repo");
    let deps_map = temp.path().join("deps_map.toml");
    fs::write(&deps_map, "[mappings]\n").unwrap();

    let filename = "bad-abi-3.0-1-x86_64.pkg.tar.zst";
    let bad_elf_bytes = bad_elf();
    let package = arch_package(
        "bad-abi",
        "3.0-1",
        &[],
        &[("usr/lib/libevil.so", bad_elf_bytes.as_slice())],
    );
    fs::write(mirror.join(filename), &package).unwrap();
    // 故意省略 FILENAME/SHA256SUM，验证同步器能按 name/version 推导文件名，
    // 并在 ABI 阶段拒绝后才没有机会写入索引。
    write_core_db(
        &mirror.join("core.db"),
        &[(
            "bad-abi-3.0-1",
            "%NAME%\nbad-abi\n\n%VERSION%\n3.0-1\n\n%DEPENDS%\n".to_string(),
        )],
    );

    let report = sync::sync_mirror(mirror.to_str().unwrap(), &repo_dir, &deps_map).unwrap();

    assert_eq!(report.synced, 0);
    assert_eq!(report.skipped_abi, 1);
    assert!(!repo_dir.join(filename).exists());
    assert!(!repo_dir.join("xrpkg-repo.toml").exists());
}
#[test]
fn cli_sync_all_reads_mirrors_config_and_stays_quiet_without_tty() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let mirror = temp.path().join("mirror");
    fs::create_dir_all(&mirror).unwrap();
    let repo_dir = temp.path().join("repo");
    let mirrors_path = temp.path().join("mirrors.toml");

    let filename = "cli-hello-1.0-1-x86_64.pkg.tar.zst";
    let package = arch_package(
        "cli-hello",
        "1.0-1",
        &["glibc"],
        &[("usr/bin/cli-hello", b"#!/bin/sh\necho cli hello\n")],
    );
    fs::write(mirror.join(filename), &package).unwrap();
    write_core_db(
        &mirror.join("core.db"),
        &[(
            "cli-hello-1.0-1",
            desc(
                "cli-hello",
                "1.0-1",
                filename,
                &sha256(&package),
                &["glibc"],
            ),
        )],
    );
    fs::write(
        &mirrors_path,
        format!(
            "[[mirrors]]\nname = \"local\"\nurl = \"{}\"\npriority = 1\n",
            mirror.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_xrpkg"))
        .args([
            "sync",
            "--all",
            "--mirrors",
            mirrors_path.to_str().unwrap(),
            "--repo-dir",
            repo_dir.to_str().unwrap(),
            "--jobs",
            "2",
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout={stdout}\nstderr={stderr}");
    assert!(stdout.contains("同步镜像 local"), "缺少镜像提示: {stdout}");
    assert!(
        !stdout.contains("\x1b[") && !stderr.contains("\x1b["),
        "非 TTY 不应输出 ANSI 控制序列"
    );
    assert!(repo_dir.join(filename).is_file());
    assert!(repo_dir.join("xrpkg-repo.toml").is_file());
}
