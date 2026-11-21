mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use tempfile::TempDir;
use xrpkg::builder::{build_package, build_package_with_options, BuildOptions};
use xrpkg::capability::Version;
use xrpkg::solver::SelectedPackage;
use xrpkg::xpkg::{install_package, read_metadata, Rootfs};

fn source_program() -> &'static str {
    "#include <stdio.h>\nint main(void) { puts(\"hello from test build\"); return 0; }\n"
}

#[test]
fn builds_source_and_packages_only_pkgdir() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("src");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("hello.c"), source_program()).unwrap();
    fs::write(source.join("junk.tmp"), b"not packaged\n").unwrap();

    let recipe = temp.path().join("hello.toml");
    fs::write(
        &recipe,
        r#"
name = "test-hello"
version = "1.0"
source = "src"
requires = []
build = [
  "cc -O2 -c -o hello.o hello.c",
  "cc -o hello hello.o",
]
package = [
  "install -Dm755 hello \"$PKGDIR/usr/bin/test-hello\"",
]
"#,
    )
    .unwrap();
    let output_dir = temp.path().join("out");

    let archive = build_package(&recipe, &output_dir).unwrap();

    assert_eq!(archive, output_dir.join("test-hello-1.0.xpkg"));
    assert!(archive.is_file());
    let metadata = read_metadata(&archive).unwrap();
    assert_eq!(metadata.name, "test-hello");
    assert_eq!(metadata.version, Version::parse("1.0").unwrap());
    assert!(metadata.requires.is_empty());

    let archive_file = fs::File::open(&archive).unwrap();
    let decoder = zstd::stream::read::Decoder::new(archive_file).unwrap();
    let mut tar = tar::Archive::new(decoder);
    let entries = tar
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().path().unwrap().into_owned())
        .collect::<Vec<_>>();
    assert!(entries
        .iter()
        .any(|path| path == std::path::Path::new("metadata.toml")));
    assert!(entries
        .iter()
        .any(|path| path == std::path::Path::new("files/usr/bin/test-hello")));
    assert!(!entries
        .iter()
        .any(|path| path.to_string_lossy().contains("hello.c")));
    assert!(!entries
        .iter()
        .any(|path| path.to_string_lossy().contains("hello.o")));
    assert!(!entries
        .iter()
        .any(|path| path.to_string_lossy().contains("junk.tmp")));

    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    let expected = SelectedPackage {
        name: "test-hello".to_string(),
        version: Version::parse("1.0").unwrap(),
    };
    install_package(&archive, &expected, &rootfs).unwrap();
    let installed_binary = rootfs.path().join("usr/bin/test-hello");
    let mode = fs::metadata(&installed_binary)
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0);
    let output = Command::new(&installed_binary).output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "hello from test build"
    );
}

#[test]
fn failed_build_does_not_emit_an_xpkg() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("src");
    fs::create_dir(&source).unwrap();
    let recipe = temp.path().join("fail.toml");
    fs::write(
        &recipe,
        r#"
name = "broken"
version = "1.0"
source = "src"
build = [
  "echo build-stdout",
  "echo build-stderr >&2; exit 7",
]
package = []
"#,
    )
    .unwrap();
    let output_dir = temp.path().join("out");

    let error = build_package(&recipe, &output_dir).unwrap_err();

    assert!(error.contains("退出码 7"), "{error}");
    assert!(!output_dir.join("broken-1.0.xpkg").exists());
}

#[test]
fn packages_and_installs_symlinks_and_hardlinks() {
    common::isolate_cache();
    use std::os::unix::fs::MetadataExt;
    use xrpkg::xpkg::publish_files;

    let temp = TempDir::new().unwrap();
    let source = temp.path().join("src");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("data.txt"), b"linked data\n").unwrap();
    let recipe = temp.path().join("links.toml");
    fs::write(
        &recipe,
        r#"
name = "link-pkg"
version = "1.0"
source = "src"
requires = []
build = []
package = [
  "install -Dm644 data.txt \"$PKGDIR/usr/share/link-pkg/data.txt\"",
  "ln -s data.txt \"$PKGDIR/usr/share/link-pkg/data-link\"",
  "ln \"$PKGDIR/usr/share/link-pkg/data.txt\" \"$PKGDIR/usr/share/link-pkg/data-hard\"",
]
"#,
    )
    .unwrap();
    let output_dir = temp.path().join("out");
    let archive = build_package(&recipe, &output_dir).unwrap();

    let staging = Rootfs::prepare(temp.path().join("staging")).unwrap();
    let installed = xrpkg::xpkg::install_package(
        &archive,
        &SelectedPackage {
            name: "link-pkg".to_string(),
            version: Version::parse("1.0").unwrap(),
        },
        &staging,
    )
    .unwrap();
    assert_eq!(
        fs::read_link(staging.path().join("usr/share/link-pkg/data-link")).unwrap(),
        std::path::Path::new("data.txt")
    );
    assert_eq!(
        fs::metadata(staging.path().join("usr/share/link-pkg/data.txt"))
            .unwrap()
            .ino(),
        fs::metadata(staging.path().join("usr/share/link-pkg/data-hard"))
            .unwrap()
            .ino()
    );

    let target = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    publish_files(&staging, &target, &installed).unwrap();
    assert_eq!(
        fs::read_link(target.path().join("usr/share/link-pkg/data-link")).unwrap(),
        std::path::Path::new("data.txt")
    );
    assert_eq!(
        fs::metadata(target.path().join("usr/share/link-pkg/data.txt"))
            .unwrap()
            .ino(),
        fs::metadata(target.path().join("usr/share/link-pkg/data-hard"))
            .unwrap()
            .ino()
    );
}

#[test]
fn passes_custom_environment_to_build_commands() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("src");
    fs::create_dir(&source).unwrap();
    let recipe = temp.path().join("env.toml");
    fs::write(
        &recipe,
        r#"
name = "env-pkg"
version = "1.0"
source = "src"
requires = []
build = []
package = [
  "mkdir -p \"$PKGDIR/usr/share\" && printf '%s' \"$XRPKG_TEST_VALUE\" > \"$PKGDIR/usr/share/env.txt\"",
]
[env]
XRPKG_TEST_VALUE = "from-recipe-env"
"#,
    )
    .unwrap();
    let output_dir = temp.path().join("out");
    let archive = build_package_with_options(
        &recipe,
        &output_dir,
        BuildOptions {
            sandbox: false,
            jobs: None,
        },
    )
    .unwrap();
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    install_package(
        &archive,
        &SelectedPackage {
            name: "env-pkg".to_string(),
            version: Version::parse("1.0").unwrap(),
        },
        &rootfs,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(rootfs.path().join("usr/share/env.txt")).unwrap(),
        "from-recipe-env"
    );
}

#[test]
fn packaging_preserves_suid_bits() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("src");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("tool"), b"#!/bin/sh\necho tool\n").unwrap();
    let recipe = temp.path().join("suid.toml");
    fs::write(
        &recipe,
        r#"
name = "suid-tool"
version = "1.0"
source = "src"
requires = []
package = [
  "install -Dm4755 tool \"$PKGDIR/usr/bin/suid-tool\"",
]
"#,
    )
    .unwrap();
    let output_dir = temp.path().join("out");
    let archive = build_package(&recipe, &output_dir).unwrap();

    let file = fs::File::open(&archive).unwrap();
    let decoder = zstd::stream::read::Decoder::new(file).unwrap();
    let mut tar = tar::Archive::new(decoder);
    let entry = tar
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap())
        .find(|entry| entry.path().unwrap() == std::path::Path::new("files/usr/bin/suid-tool"))
        .unwrap();
    assert_eq!(entry.header().uid().unwrap(), 0);
    assert_eq!(entry.header().gid().unwrap(), 0);

    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    let expected = SelectedPackage {
        name: "suid-tool".to_string(),
        version: Version::parse("1.0").unwrap(),
    };
    install_package(&archive, &expected, &rootfs).unwrap();
    let installed = rootfs.path().join("usr/bin/suid-tool");
    assert_eq!(
        fs::metadata(&installed).unwrap().permissions().mode() & 0o7777,
        0o4755
    );
}
