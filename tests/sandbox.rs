use std::fs;
use std::path::Path;

use tempfile::TempDir;
use xrpkg::builder::{build_package, build_package_with_options, BuildOptions};
use xrpkg::capability::Version;
use xrpkg::solver::SelectedPackage;
use xrpkg::xpkg::{install_package, read_metadata, Rootfs};

fn write_recipe(temp: &TempDir, body: &str) -> std::path::PathBuf {
    let source = temp.path().join("src");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("data.txt"), "sandbox\n").unwrap();
    let recipe = temp.path().join("recipe.toml");
    fs::write(&recipe, body).unwrap();
    recipe
}

#[test]
fn sandbox_clears_host_cflags_and_ldflags() {
    let temp = TempDir::new().unwrap();
    let recipe = write_recipe(
        &temp,
        r#"
name = "sandbox-env"
version = "1.0"
source = "src"
requires = []
build = []
package = [
  "mkdir -p \"$PKGDIR/usr/share\" && printf '%s|%s' \"${CFLAGS:-}\" \"${LDFLAGS:-}\" > \"$PKGDIR/usr/share/env.txt\"",
]
"#,
    );
    let old_cflags = std::env::var_os("CFLAGS");
    let old_ldflags = std::env::var_os("LDFLAGS");
    std::env::set_var("CFLAGS", "-I/hacked");
    std::env::set_var("LDFLAGS", "-L/hacked");
    let result = build_package(&recipe, &temp.path().join("out"));
    match old_cflags {
        Some(value) => std::env::set_var("CFLAGS", value),
        None => std::env::remove_var("CFLAGS"),
    }
    match old_ldflags {
        Some(value) => std::env::set_var("LDFLAGS", value),
        None => std::env::remove_var("LDFLAGS"),
    }
    let archive = result.unwrap();
    let rootfs = Rootfs::prepare(temp.path().join("rootfs")).unwrap();
    install_package(
        &archive,
        &SelectedPackage {
            name: "sandbox-env".to_string(),
            version: Version::parse("1.0").unwrap(),
        },
        &rootfs,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(rootfs.path().join("usr/share/env.txt")).unwrap(),
        "|"
    );
}

#[test]
fn sandbox_rejects_writes_to_etc_and_host_is_untouched() {
    let temp = TempDir::new().unwrap();
    let recipe = write_recipe(
        &temp,
        r#"
name = "sandbox-etc"
version = "1.0"
source = "src"
requires = []
build = ["touch /etc/xrpkg-sandbox-test-file"]
package = ["touch \"$PKGDIR/should-not-exist\""]
"#,
    );
    let error = build_package(&recipe, &temp.path().join("out")).unwrap_err();
    assert!(error.contains("沙箱"), "{error}");
    assert!(!Path::new("/etc/xrpkg-sandbox-test-file").exists());
}

#[test]
fn real_hello_and_zlib_build_in_sandbox() {
    let temp = TempDir::new().unwrap();
    let project = Path::new(env!("CARGO_MANIFEST_DIR"));
    let hello = build_package(
        &project.join("recipes/hello.toml"),
        &temp.path().join("hello-out"),
    )
    .unwrap();
    let hello_meta = read_metadata(&hello).unwrap();
    assert_eq!(hello_meta.name, "hello");
    let hello_root = Rootfs::prepare(temp.path().join("hello-root")).unwrap();
    install_package(
        &hello,
        &SelectedPackage {
            name: "hello".to_string(),
            version: Version::parse("1.0").unwrap(),
        },
        &hello_root,
    )
    .unwrap();
    assert!(hello_root.path().join("usr/bin/hello").is_file());

    let zlib = build_package(
        &project.join("recipes/zlib.toml"),
        &temp.path().join("zlib-out"),
    )
    .unwrap();
    let zlib_meta = read_metadata(&zlib).unwrap();
    assert_eq!(zlib_meta.name, "zlib");
    let zlib_root = Rootfs::prepare(temp.path().join("zlib-root")).unwrap();
    install_package(
        &zlib,
        &SelectedPackage {
            name: "zlib".to_string(),
            version: Version::parse("1.3.1").unwrap(),
        },
        &zlib_root,
    )
    .unwrap();
    assert!(zlib_root.path().join("usr/lib/libz.so").exists());
}

fn install_and_read(archive: &std::path::Path, rootfs: &std::path::Path, file: &str) -> String {
    let rootfs = Rootfs::prepare(rootfs).unwrap();
    install_package(
        archive,
        &SelectedPackage {
            name: "sandbox-jobs".to_string(),
            version: Version::parse("1.0").unwrap(),
        },
        &rootfs,
    )
    .unwrap();
    fs::read_to_string(rootfs.path().join(file)).unwrap()
}

#[test]
fn makeflags_defaults_to_j8_and_can_be_overridden() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("src");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("data.txt"), "jobs\n").unwrap();
    let default_recipe = temp.path().join("jobs-default.toml");
    fs::write(
        &default_recipe,
        r#"
name = "sandbox-jobs"
version = "1.0"
source = "src"
requires = []
build = []
package = [
  "mkdir -p \"$PKGDIR/usr/share\" && printf '%s' \"$MAKEFLAGS\" > \"$PKGDIR/usr/share/makeflags.txt\"",
]
"#,
    )
    .unwrap();
    let default = build_package(&default_recipe, &temp.path().join("out-default")).unwrap();
    assert_eq!(
        install_and_read(
            &default,
            &temp.path().join("root-default"),
            "usr/share/makeflags.txt"
        ),
        "-j8"
    );

    let recipe = temp.path().join("jobs.toml");
    fs::write(
        &recipe,
        r#"
name = "sandbox-jobs"
version = "1.0"
source = "src"
requires = []
build = []
package = [
  "mkdir -p \"$PKGDIR/usr/share\" && printf '%s' \"$MAKEFLAGS\" > \"$PKGDIR/usr/share/makeflags.txt\"",
]
[env]
MAKEFLAGS = "-j2"
"#,
    )
    .unwrap();
    let env_override = build_package(&recipe, &temp.path().join("out-env")).unwrap();
    assert_eq!(
        install_and_read(
            &env_override,
            &temp.path().join("root-env"),
            "usr/share/makeflags.txt"
        ),
        "-j2"
    );

    let cli = build_package_with_options(
        &recipe,
        &temp.path().join("out-cli"),
        BuildOptions {
            sandbox: true,
            jobs: Some(3),
        },
    )
    .unwrap();
    assert_eq!(
        install_and_read(
            &cli,
            &temp.path().join("root-cli"),
            "usr/share/makeflags.txt"
        ),
        "-j3"
    );
}

#[test]
fn build_output_is_written_to_log_file() {
    let temp = TempDir::new().unwrap();
    let recipe = write_recipe(
        &temp,
        r#"
name = "sandbox-log"
version = "1.0"
source = "src"
requires = []
build = ["echo log-marker"]
package = ["touch \"$PKGDIR/log-file\""]
"#,
    );
    let _ = build_package(&recipe, &temp.path().join("out")).unwrap();
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cache"))
        })
        .expect("无法定位缓存目录");
    let log = cache.join("xrpkg/logs/sandbox-log-build.log");
    let text = fs::read_to_string(&log).unwrap();
    assert!(text.contains("log-marker"), "{text}");
}
