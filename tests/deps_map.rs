use std::fs::{self, File};
use std::io::{Cursor, Write};
use std::path::Path;

use flate2::write::GzEncoder;
use flate2::Compression;
use tempfile::TempDir;
use xrpkg::capability::{Capability, Version};
use xrpkg::compat_deb::parse_deb_metadata;
use xrpkg::deps_map::DependencyMapper;
use xrpkg::resolvo_solver::resolve_requirements;
use xrpkg::solver::Package;

fn package(name: &str, version_input: &str) -> Package {
    Package::new(name, Version::parse(version_input).unwrap(), Vec::new())
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn tar_bytes(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, contents, mode) in entries {
        let mut header = tar::Header::new_gnu();
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

fn write_fake_deb_with_libssl3(path: &Path) {
    let control = "Package: fake-libssl3\nVersion: 1.0-1\nArchitecture: amd64\nDepends: libssl3 (>= 1.1)\nDescription: fake\n";
    let control_tar = tar_bytes(&[("./control", control.as_bytes(), 0o644)]);
    let data_tar = tar_bytes(&[("./usr/share/fake-libssl3/readme", b"fake\n", 0o644)]);

    let file = File::create(path).unwrap();
    let mut builder = ar::Builder::new(file);
    append_ar(&mut builder, b"debian-binary", b"2.0\n");
    append_ar(&mut builder, b"control.tar.gz", &gzip(&control_tar));
    append_ar(&mut builder, b"data.tar.gz", &gzip(&data_tar));
    builder.into_inner().unwrap();
}

#[test]
fn mapper_generates_template_when_missing() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("config/deps_map.toml");

    let mapper = DependencyMapper::load_or_create(&path).unwrap();

    assert!(path.is_file());
    assert_eq!(mapper.map_dependency("libc6 (>= 2.34)"), "glibc >= 2.44");
}

#[test]
fn mapper_strips_debian_rpm_and_arch_constraint_forms() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("deps_map.toml");
    fs::write(
        &path,
        r#"
[mappings]
"libssl3" = "openssl >= 3.0"
"glibc" = "glibc >= 2.44"
"libc.so.6" = "glibc >= 2.44"
"libfoo" = "libfoo >= 2.0"
"#,
    )
    .unwrap();
    let mapper = DependencyMapper::load_or_create(&path).unwrap();

    assert_eq!(mapper.map_dependency("libssl3 (>= 1.1)"), "openssl >= 3.0");
    assert_eq!(mapper.map_dependency("glibc >= 2.17"), "glibc >= 2.44");
    assert_eq!(mapper.map_dependency("libc.so.6()(64bit)"), "glibc >= 2.44");
    assert_eq!(mapper.map_dependency("libfoo>=1.0"), "libfoo >= 2.0");
}

#[test]
fn invalid_mapping_target_falls_back_to_source_name() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("deps_map.toml");
    fs::write(&path, "[mappings]\n\"libssl3\" = \"openssl >= \"\n").unwrap();
    let mapper = DependencyMapper::load_or_create(&path).unwrap();

    let capability = mapper.map_capability("libssl3 (>= 1.1)").unwrap();

    assert_eq!(capability, Capability::parse("libssl3").unwrap());
}

#[test]
fn fake_deb_with_libssl3_fails_without_mapping_and_succeeds_with_mapping() {
    let temp = TempDir::new().unwrap();
    let deb_path = temp.path().join("fake.deb");
    write_fake_deb_with_libssl3(&deb_path);
    let metadata = parse_deb_metadata(&deb_path).unwrap();
    let repository = vec![package("openssl", "3.1")];

    let empty_map = temp.path().join("empty-deps-map.toml");
    fs::write(&empty_map, "[mappings]\n").unwrap();
    let empty_mapper = DependencyMapper::load_or_create(&empty_map).unwrap();
    let unmapped = metadata.mapped_requirements(&empty_mapper).unwrap();
    assert_eq!(unmapped, [Capability::parse("libssl3").unwrap()]);
    assert!(resolve_requirements(&repository, &unmapped, &[]).is_err());

    let mapped_path = temp.path().join("deps_map.toml");
    fs::write(
        &mapped_path,
        "[mappings]\n\"libssl3\" = \"openssl >= 3.0\"\n",
    )
    .unwrap();
    let mapper = DependencyMapper::load_or_create(&mapped_path).unwrap();
    let mapped = metadata.mapped_requirements(&mapper).unwrap();
    assert_eq!(mapped, [Capability::parse("openssl >= 3.0").unwrap()]);

    let selected = resolve_requirements(&repository, &mapped, &[]).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].name, "openssl");
    assert_eq!(selected[0].version, Version::parse("3.1").unwrap());
}
