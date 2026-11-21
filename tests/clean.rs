use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;
use xrpkg::builder::build_package;
use xrpkg::capability::Version;
use xrpkg::repo;
use xrpkg::state::LocalDb;

mod common;

fn build_fixture(temp: &TempDir, version: &str) -> PathBuf {
    let source = temp.path().join(format!("a-{version}-src"));
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("payload.txt"), format!("payload {version}\n")).unwrap();
    let recipe = temp.path().join(format!("a-{version}.toml"));
    fs::write(
        &recipe,
        format!(
            r#"
name = "a"
version = "{version}"
source = "a-{version}-src"
requires = []
build = []
package = [
  "install -Dm644 payload.txt \"$PKGDIR/usr/share/clean-test/a.txt\"",
]
"#
        ),
    )
    .unwrap();
    build_package(&recipe, &temp.path().join(format!("out-{version}"))).unwrap()
}

#[test]
fn clean_keeps_installed_and_latest_and_removes_old_cache_packages() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let repo_dir = temp.path().join("repo");
    let gpg = common::TestGpg::new(temp.path());
    let v1 = build_fixture(&temp, "1.0");
    let v2 = build_fixture(&temp, "2.0");
    let v3 = build_fixture(&temp, "3.0");
    for package in [&v1, &v2, &v3] {
        repo::repo_add(&repo_dir, package).unwrap();
    }
    gpg.sign_repo(&repo_dir);
    let config = temp.path().join("repos.toml");
    common::write_config(&config, &[("repo", &repo_dir)], &gpg.keyring);
    let cache = temp.path().join("cache");
    repo::update(&config, &cache).unwrap();

    let mut db = LocalDb::open(":memory:").unwrap();
    let rootfs = temp.path().join("rootfs");
    let two = Version::parse("2.0").unwrap();
    repo::install_from_repositories(
        &mut db,
        &config,
        &cache,
        "a",
        Some(&two),
        false,
        &rootfs,
        false,
    )
    .unwrap();
    assert_eq!(db.list_installed().unwrap()[0].version_raw, "2.0");

    let cached_dir = cache.join("packages/repo");
    fs::create_dir_all(&cached_dir).unwrap();
    for version in ["1.0", "3.0"] {
        for suffix in ["", ".sig"] {
            let name = format!("a-{version}.xpkg{suffix}");
            fs::copy(repo_dir.join(&name), cached_dir.join(&name)).unwrap();
        }
    }

    let report = repo::clean_cache(&config, &cache, &db).unwrap();

    assert_eq!(report.packages, 1);
    assert_eq!(report.signatures, 1);
    assert!(report.bytes > 0);
    assert!(!cached_dir.join("a-1.0.xpkg").exists());
    assert!(!cached_dir.join("a-1.0.xpkg.sig").exists());
    assert!(cached_dir.join("a-2.0.xpkg").is_file());
    assert!(cached_dir.join("a-2.0.xpkg.sig").is_file());
    assert!(cached_dir.join("a-3.0.xpkg").is_file());
    assert!(cached_dir.join("a-3.0.xpkg.sig").is_file());
}

#[test]
fn clean_reports_zero_for_empty_cache() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let repo_dir = temp.path().join("repo");
    let gpg = common::TestGpg::new(temp.path());
    let package = build_fixture(&temp, "1.0");
    repo::repo_add(&repo_dir, &package).unwrap();
    gpg.sign_repo(&repo_dir);
    let config = temp.path().join("repos.toml");
    common::write_config(&config, &[("repo", &repo_dir)], &gpg.keyring);
    let cache = temp.path().join("cache");
    repo::update(&config, &cache).unwrap();
    let db = LocalDb::open(":memory:").unwrap();

    let report = repo::clean_cache(&config, &cache, &db).unwrap();
    assert_eq!(report, repo::CleanReport::default());
}

#[test]
fn clean_cli_runs_and_reports_zero_for_empty_cache() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let repo_dir = temp.path().join("repo");
    let gpg = common::TestGpg::new(temp.path());
    let package = build_fixture(&temp, "1.0");
    repo::repo_add(&repo_dir, &package).unwrap();
    gpg.sign_repo(&repo_dir);
    let config = temp.path().join("repos.toml");
    common::write_config(&config, &[("repo", &repo_dir)], &gpg.keyring);
    let cache = temp.path().join("cache");
    repo::update(&config, &cache).unwrap();
    let db = temp.path().join("local.db");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_xrpkg"))
        .args([
            "clean",
            "--config",
            config.to_str().unwrap(),
            "--cache",
            cache.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("缓存清理完成"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("0 个包"));
}
