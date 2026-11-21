mod common;

use std::fs;
use std::io::Write;
use std::process::Command;

use flate2::write::GzEncoder;
use flate2::Compression;
use liblzma::write::XzEncoder;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use xrpkg::builder::build_package;
use xrpkg::solver::SelectedPackage;
use xrpkg::xpkg::{install_package, read_metadata, Rootfs};

fn hello_source_tar() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let source =
        b"#include <stdio.h>\nint main(void) { puts(\"hello from fetched source\"); return 0; }\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(source.len() as u64);
    header.set_mode(0o644);
    builder
        .append_data(&mut header, "src/hello.c", &source[..])
        .unwrap();
    builder.into_inner().unwrap()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn xz(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = XzEncoder::new(Vec::new(), 6);
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write_recipe(path: &std::path::Path, name: &str, url: &str, hash: &str) {
    fs::write(
        path,
        format!(
            r#"
name = "{name}"
version = "1.0"
source = [{{ url = "{url}", sha256 = "{hash}" }}]
requires = []
build = [
  "cd src && cc -O2 -o hello hello.c",
]
package = [
  "cd src && install -Dm755 hello \"$PKGDIR/usr/bin/{name}\"",
]
"#
        ),
    )
    .unwrap();
}

fn build_and_install(recipe: &std::path::Path, output_dir: &std::path::Path, temp: &TempDir) {
    let archive = build_package(recipe, output_dir).unwrap();
    let metadata = read_metadata(&archive).unwrap();
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    let expected = SelectedPackage {
        name: metadata.name.clone(),
        version: metadata.version.clone(),
    };
    install_package(&archive, &expected, &rootfs).unwrap();
    let output = Command::new(rootfs.path().join(format!("usr/bin/{}", metadata.name)))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "hello from fetched source"
    );
}

#[test]
fn fetches_and_extracts_tar_gz_from_file_url() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let tar = hello_source_tar();
    let archive = temp.path().join("fetched-gzip.tar.gz");
    let bytes = gzip(&tar);
    fs::write(&archive, &bytes).unwrap();
    let recipe = temp.path().join("recipe.toml");
    let url = format!("file://{}", archive.display());
    write_recipe(&recipe, "fetched-gzip", &url, &sha256(&bytes));
    let output_dir = temp.path().join("out");

    let out = build_package(&recipe, &output_dir).unwrap();

    assert_eq!(out, output_dir.join("fetched-gzip-1.0.xpkg"));
    assert!(out.is_file());
    build_and_install(&recipe, &output_dir, &temp);
}

#[test]
fn detects_and_extracts_tar_xz_from_local_path() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let tar = hello_source_tar();
    let archive = temp.path().join("fetched-xz.tar.xz");
    let bytes = xz(&tar);
    fs::write(&archive, &bytes).unwrap();
    let recipe = temp.path().join("recipe.toml");
    write_recipe(
        &recipe,
        "fetched-xz",
        &archive.display().to_string(),
        &sha256(&bytes),
    );
    let output_dir = temp.path().join("out");

    let out = build_package(&recipe, &output_dir).unwrap();

    assert_eq!(out, output_dir.join("fetched-xz-1.0.xpkg"));
    assert!(out.is_file());
    build_and_install(&recipe, &output_dir, &temp);
}

#[test]
fn hash_mismatch_aborts_before_build_or_packaging() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("hello.tar.gz");
    fs::write(&archive, gzip(&hello_source_tar())).unwrap();
    let recipe = temp.path().join("recipe.toml");
    write_recipe(
        &recipe,
        "bad-hash",
        &archive.display().to_string(),
        &"0".repeat(64),
    );
    let output_dir = temp.path().join("out");

    let error = build_package(&recipe, &output_dir).unwrap_err();

    assert!(error.contains("SHA256 不匹配"), "{error}");
    assert!(!output_dir.join("bad-hash-1.0.xpkg").exists());
}
