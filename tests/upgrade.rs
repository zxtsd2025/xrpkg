mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use xrpkg::builder::build_package;
use xrpkg::capability::Version;
use xrpkg::installer::install_changes;
use xrpkg::resolvo_solver::resolve_install_latest;
use xrpkg::solver::{Package, SelectedPackage};
use xrpkg::state::{InstalledPackage, LocalDb};

fn build_variant(
    temp: &TempDir,
    version: &str,
    files: &[(&str, &str)],
    hooks: &[(&str, &str)],
) -> PathBuf {
    let source_name = format!("pkg-a-{version}-src");
    let source = temp.path().join(&source_name);
    fs::create_dir_all(&source).unwrap();
    for (name, contents) in files {
        fs::write(source.join(name), contents).unwrap();
    }
    let recipe = temp.path().join(format!("pkg-a-{version}.toml"));
    let package_steps = files
        .iter()
        .map(|(name, _)| format!("  \"install -Dm644 {name} \\\"$PKGDIR/bin/{name}\\\"\","))
        .collect::<Vec<_>>()
        .join("\n");
    let mut hook_text = String::new();
    for (phase, name) in hooks {
        let script = if name.ends_with("fail") {
            "exit 1\n".to_string()
        } else {
            format!(
                "mkdir -p \"$XRPKG_ROOTFS/etc\"\necho {phase} > \"$XRPKG_ROOTFS/etc/{phase}.txt\"\n"
            )
        };
        fs::write(temp.path().join(format!("{version}-{phase}.sh")), script).unwrap();
        hook_text.push_str(&format!("{phase} = \"{version}-{phase}.sh\"\n"));
    }
    fs::write(
        &recipe,
        format!(
            r#"
name = "pkg-a"
version = "{version}"
source = "{source_name}"
requires = []
build = []
package = [
{package_steps}
]
[hooks]
{hook_text}
"#
        ),
    )
    .unwrap();
    let output = temp.path().join(format!("out-{version}"));
    build_package(&recipe, &output).unwrap()
}

fn selected(version: &str) -> SelectedPackage {
    SelectedPackage {
        name: "pkg-a".to_string(),
        version: Version::parse(version).unwrap(),
    }
}

fn install_variant(
    db: &mut LocalDb,
    archive: &Path,
    version: &str,
    rootfs: &Path,
) -> Result<(), String> {
    install_variant_with(db, archive, version, rootfs, false)
}

fn install_variant_with(
    db: &mut LocalDb,
    archive: &Path,
    version: &str,
    rootfs: &Path,
    allow_downgrade: bool,
) -> Result<(), String> {
    let mut archives = HashMap::new();
    archives.insert(
        ("pkg-a".to_string(), Version::parse(version).unwrap()),
        archive.to_path_buf(),
    );
    install_changes(
        db,
        &[selected(version)],
        &archives,
        rootfs,
        false,
        allow_downgrade,
    )
}

#[test]
fn upgrades_files_hooks_and_removes_obsolete_files() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let v1 = build_variant(
        &temp,
        "1.0",
        &[("a", "a-v1\n"), ("legacy", "legacy-v1\n")],
        &[("pre_remove", "old-pre-remove")],
    );
    let v2 = build_variant(
        &temp,
        "2.0",
        &[("a", "a-v2\n"), ("new", "new-v2\n")],
        &[
            ("pre_install", "pre-install"),
            ("post_install", "post-install"),
            ("pre_upgrade", "pre-upgrade"),
            ("post_upgrade", "post-upgrade"),
        ],
    );
    let mut db = LocalDb::open(":memory:").unwrap();
    let rootfs = temp.path().join("rootfs");

    install_variant(&mut db, &v1, "1.0", &rootfs).unwrap();
    assert_eq!(fs::read_to_string(rootfs.join("bin/a")).unwrap(), "a-v1\n");
    assert!(rootfs.join("bin/legacy").is_file());

    install_variant(&mut db, &v2, "2.0", &rootfs).unwrap();

    assert_eq!(fs::read_to_string(rootfs.join("bin/a")).unwrap(), "a-v2\n");
    assert!(rootfs.join("bin/new").is_file());
    assert!(!rootfs.join("bin/legacy").exists());
    assert_eq!(
        fs::read_to_string(rootfs.join("etc/pre_upgrade.txt")).unwrap(),
        "pre_upgrade\n"
    );
    assert_eq!(
        fs::read_to_string(rootfs.join("etc/post_upgrade.txt")).unwrap(),
        "post_upgrade\n"
    );
    assert!(!rootfs.join("etc/pre_remove.txt").exists());
    assert!(!rootfs.join("etc/pre_install.txt").exists());
    assert!(!rootfs.join("etc/post_install.txt").exists());

    let installed = db.list_installed().unwrap();
    assert_eq!(installed[0].version_raw, "2.0");
    assert_eq!(db.list_files("pkg-a").unwrap(), ["bin/a", "bin/new"]);
}

#[test]
fn corrupted_new_archive_leaves_old_version_untouched() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let v1 = build_variant(
        &temp,
        "1.0",
        &[("a", "a-v1\n"), ("legacy", "legacy-v1\n")],
        &[],
    );
    let v2 = build_variant(&temp, "2.0", &[("a", "a-v2\n"), ("new", "new-v2\n")], &[]);
    let mut db = LocalDb::open(":memory:").unwrap();
    let rootfs = temp.path().join("rootfs");
    install_variant(&mut db, &v1, "1.0", &rootfs).unwrap();

    let bytes = fs::read(&v2).unwrap();
    fs::write(&v2, &bytes[..bytes.len() / 2]).unwrap();
    let error = install_variant(&mut db, &v2, "2.0", &rootfs).unwrap_err();

    assert!(!error.is_empty());
    assert_eq!(fs::read_to_string(rootfs.join("bin/a")).unwrap(), "a-v1\n");
    assert!(rootfs.join("bin/legacy").is_file());
    assert!(!rootfs.join("bin/new").exists());
    assert_eq!(db.list_installed().unwrap()[0].version_raw, "1.0");
}

#[test]
fn latest_resolver_selects_the_newer_version() {
    common::isolate_cache();
    let installed = InstalledPackage {
        name: "pkg-a".to_string(),
        version: Version::parse("1.0").unwrap(),
        version_raw: "1.0".to_string(),
        installed_at: "test".to_string(),
        origin: "native".to_string(),
    };
    let packages = vec![
        Package::new("pkg-a", Version::parse("1.0").unwrap(), Vec::new()),
        Package::new("pkg-a", Version::parse("2.0").unwrap(), Vec::new()),
    ];

    let resolution = resolve_install_latest(&packages, "pkg-a", &[installed]).unwrap();

    assert_eq!(
        resolution
            .packages()
            .iter()
            .find(|package| package.name == "pkg-a")
            .unwrap()
            .version,
        Version::parse("2.0").unwrap()
    );
}
#[test]
fn failed_post_upgrade_restores_old_files_and_database() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let v1 = build_variant(
        &temp,
        "1.0",
        &[("a", "a-v1\n"), ("legacy", "legacy-v1\n")],
        &[],
    );
    let v2 = build_variant(
        &temp,
        "2.0",
        &[("a", "a-v2\n"), ("new", "new-v2\n")],
        &[("post_upgrade", "post-upgrade-fail")],
    );

    let mut db = LocalDb::open(":memory:").unwrap();
    let rootfs = temp.path().join("rootfs");
    install_variant(&mut db, &v1, "1.0", &rootfs).unwrap();

    let error = install_variant(&mut db, &v2, "2.0", &rootfs).unwrap_err();

    assert!(error.contains("post_upgrade"), "{error}");
    assert_eq!(fs::read_to_string(rootfs.join("bin/a")).unwrap(), "a-v1\n");
    assert!(rootfs.join("bin/legacy").is_file());
    assert!(!rootfs.join("bin/new").exists());
    assert_eq!(db.list_installed().unwrap()[0].version_raw, "1.0");
}

#[test]
fn explicit_downgrade_replaces_files_and_removes_newer_only_files() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let v1 = build_variant(
        &temp,
        "1.0",
        &[("a", "a-v1\n"), ("only1", "only-v1\n")],
        &[
            ("pre_upgrade", "pre-upgrade"),
            ("post_upgrade", "post-upgrade"),
        ],
    );
    let v2 = build_variant(
        &temp,
        "2.0",
        &[("a", "a-v2\n"), ("only2", "only-v2\n")],
        &[],
    );
    let mut db = LocalDb::open(":memory:").unwrap();
    let rootfs = temp.path().join("rootfs");
    install_variant(&mut db, &v2, "2.0", &rootfs).unwrap();

    let error = install_variant(&mut db, &v1, "1.0", &rootfs).unwrap_err();
    assert!(error.contains("--allow-downgrade"), "{error}");
    assert_eq!(fs::read_to_string(rootfs.join("bin/a")).unwrap(), "a-v2\n");
    assert!(rootfs.join("bin/only2").is_file());

    install_variant_with(&mut db, &v1, "1.0", &rootfs, true).unwrap();

    assert_eq!(fs::read_to_string(rootfs.join("bin/a")).unwrap(), "a-v1\n");
    assert!(rootfs.join("bin/only1").is_file());
    assert!(!rootfs.join("bin/only2").exists());
    assert_eq!(db.list_installed().unwrap()[0].version_raw, "1.0");
    assert_eq!(db.list_files("pkg-a").unwrap(), ["bin/a", "bin/only1"]);
    assert!(rootfs.join("etc/pre_upgrade.txt").is_file());
    assert!(rootfs.join("etc/post_upgrade.txt").is_file());
}
