use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use xrpkg::builder::build_package;
use xrpkg::repo;
use xrpkg::state::LocalDb;

mod common;

fn build_fixture(temp: &TempDir, name: &str, version: &str, requires: &[&str]) -> PathBuf {
    let source = temp.path().join(format!("{name}-src"));
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("payload.txt"), format!("payload for {name}\n")).unwrap();
    let recipe = temp.path().join(format!("{name}.toml"));
    let requires = requires
        .iter()
        .map(|requirement| format!("\"{requirement}\""))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        &recipe,
        format!(
            r#"
name = "{name}"
version = "{version}"
source = "{name}-src"
requires = [{requires}]
build = []
package = [
  "install -Dm644 payload.txt \"$PKGDIR/usr/share/repo-test/{name}.txt\"",
]
"#
        ),
    )
    .unwrap();
    let output = temp.path().join(format!("out-{name}"));
    build_package(&recipe, &output).unwrap()
}

fn write_config(path: &Path, repos: &[(&str, &Path)], keyring: &Path) {
    common::write_config(path, repos, keyring);
}

#[test]
fn installs_a_and_dependency_b_from_a_local_repository() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let repo_dir = temp.path().join("repo");
    let gpg = common::TestGpg::new(temp.path());
    let b = build_fixture(&temp, "b", "1.0", &[]);
    let a = build_fixture(&temp, "a", "1.0", &["b >= 1.0"]);
    repo::repo_add(&repo_dir, &b).unwrap();
    repo::repo_add(&repo_dir, &a).unwrap();
    gpg.sign_repo(&repo_dir);

    let config = temp.path().join("repos.toml");
    write_config(&config, &[("repo", &repo_dir)], &gpg.keyring);
    let cache = temp.path().join("cache");
    repo::update(&config, &cache).unwrap();

    let hits = repo::search(&config, &cache, "a").unwrap();
    assert!(hits.iter().any(|hit| hit.name == "a"));

    let mut db = LocalDb::open(":memory:").unwrap();
    let rootfs_path = temp.path().join("rootfs");
    repo::install_from_repositories(
        &mut db,
        &config,
        &cache,
        "a",
        None,
        false,
        &rootfs_path,
        false,
    )
    .unwrap();

    let installed = db.list_installed().unwrap();
    assert_eq!(installed.len(), 2);
    assert_eq!(installed[0].name, "a");
    assert_eq!(installed[1].name, "b");
    assert!(rootfs_path.join("usr/share/repo-test/a.txt").is_file());
    assert!(rootfs_path.join("usr/share/repo-test/b.txt").is_file());
    assert!(cache.join("packages/repo/a-1.0.xpkg").is_file());
    assert!(cache.join("packages/repo/b-1.0.xpkg").is_file());
}

#[test]
fn first_configured_repository_wins_for_the_same_package_name() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let repo_one = temp.path().join("repo-one");
    let repo_two = temp.path().join("repo-two");
    let gpg = common::TestGpg::new(temp.path());
    let b1 = build_fixture(&temp, "b", "1.0", &[]);
    let b2 = build_fixture(&temp, "b", "2.0", &[]);
    let a = build_fixture(&temp, "a", "1.0", &["b >= 1.0"]);
    repo::repo_add(&repo_one, &b1).unwrap();
    repo::repo_add(&repo_one, &a).unwrap();
    repo::repo_add(&repo_two, &b2).unwrap();
    gpg.sign_repo(&repo_one);
    gpg.sign_repo(&repo_two);

    let config = temp.path().join("repos.toml");
    write_config(
        &config,
        &[("repo-one", &repo_one), ("repo-two", &repo_two)],
        &gpg.keyring,
    );
    let cache = temp.path().join("cache");
    repo::update(&config, &cache).unwrap();

    let mut db = LocalDb::open(":memory:").unwrap();
    let rootfs = temp.path().join("rootfs");
    repo::install_from_repositories(&mut db, &config, &cache, "a", None, false, &rootfs, false)
        .unwrap();

    let installed = db.list_installed().unwrap();
    let b = installed
        .iter()
        .find(|package| package.name == "b")
        .unwrap();
    assert_eq!(b.version.to_string(), "1.0");
}

#[test]
fn package_download_failure_does_not_touch_database_or_rootfs() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let repo_dir = temp.path().join("repo");
    let gpg = common::TestGpg::new(temp.path());
    let b = build_fixture(&temp, "b", "1.0", &[]);
    let a = build_fixture(&temp, "a", "1.0", &["b >= 1.0"]);
    repo::repo_add(&repo_dir, &b).unwrap();
    repo::repo_add(&repo_dir, &a).unwrap();
    gpg.sign_repo(&repo_dir);

    let config = temp.path().join("repos.toml");
    write_config(&config, &[("repo", &repo_dir)], &gpg.keyring);
    let cache = temp.path().join("cache");
    repo::update(&config, &cache).unwrap();
    fs::remove_file(repo_dir.join("b-1.0.xpkg")).unwrap();

    let mut db = LocalDb::open(":memory:").unwrap();
    let rootfs_path = temp.path().join("rootfs");
    let error = repo::install_from_repositories(
        &mut db,
        &config,
        &cache,
        "a",
        None,
        false,
        &rootfs_path,
        false,
    )
    .unwrap_err();

    assert!(error.contains("下载") || error.contains("复制"), "{error}");
    assert!(db.list_installed().unwrap().is_empty());
    assert!(!rootfs_path.exists());
}

#[test]
fn local_repository_index_is_cacheable() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let repo_dir = temp.path().join("repo");
    let gpg = common::TestGpg::new(temp.path());
    let a = build_fixture(&temp, "a", "1.0", &[]);
    repo::repo_add(&repo_dir, &a).unwrap();
    gpg.sign_repo(&repo_dir);

    let config = temp.path().join("repos.toml");
    write_config(&config, &[("repo", &repo_dir)], &gpg.keyring);
    let cache = temp.path().join("cache");
    repo::update(&config, &cache).unwrap();

    let index = cache.join("indexes/repo.toml");
    assert!(index.is_file());
    let text = fs::read_to_string(index).unwrap();
    assert!(text.contains("name = \"a\""));
    assert!(text.contains("sha256 = "));
    assert!(repo::load_cached(&config, &cache).is_ok());
}
#[test]
fn supports_legacy_repo_table_configuration() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let repo_dir = temp.path().join("core");
    let gpg = common::TestGpg::new(temp.path());
    let a = build_fixture(&temp, "a", "1.0", &[]);
    repo::repo_add(&repo_dir, &a).unwrap();
    gpg.sign_repo(&repo_dir);

    let config = temp.path().join("repos.toml");
    fs::write(
        &config,
        format!(
            "keyring = \"{}\"\n[repo.core]\nurl = \"file://{}\"\n",
            gpg.keyring.display(),
            repo_dir.display()
        ),
    )
    .unwrap();
    let cache = temp.path().join("cache");
    repo::update(&config, &cache).unwrap();

    let cached = repo::load_cached(&config, &cache).unwrap();
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].name, "core");
}

#[test]
fn install_pkg_equals_version_requires_allow_downgrade() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let repo_dir = temp.path().join("repo");
    let gpg = common::TestGpg::new(temp.path());
    let v1 = build_fixture(&temp, "a", "1.0", &[]);
    let v2 = build_fixture(&temp, "a", "2.0", &[]);
    repo::repo_add(&repo_dir, &v1).unwrap();
    repo::repo_add(&repo_dir, &v2).unwrap();
    gpg.sign_repo(&repo_dir);

    let config = temp.path().join("repos.toml");
    write_config(&config, &[("repo", &repo_dir)], &gpg.keyring);
    let cache = temp.path().join("cache");
    repo::update(&config, &cache).unwrap();
    let rootfs = temp.path().join("rootfs");
    let db = temp.path().join("local.db");
    let bin = env!("CARGO_BIN_EXE_xrpkg");
    let rootfs_arg = rootfs.to_string_lossy().into_owned();
    let db_arg = db.to_string_lossy().into_owned();
    let cache_arg = cache.to_string_lossy().into_owned();
    let config_arg = config.to_string_lossy().into_owned();

    let output = Command::new(bin)
        .args([
            "install",
            "a",
            "--config",
            &config_arg,
            "--root",
            &rootfs_arg,
            "--db",
            &db_arg,
            "--cache",
            &cache_arg,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        LocalDb::open(&db).unwrap().list_installed().unwrap()[0].version_raw,
        "2.0"
    );

    let output = Command::new(bin)
        .args([
            "install",
            "a=1.0",
            "--config",
            &config_arg,
            "--root",
            &rootfs_arg,
            "--db",
            &db_arg,
            "--cache",
            &cache_arg,
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--allow-downgrade"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        LocalDb::open(&db).unwrap().list_installed().unwrap()[0].version_raw,
        "2.0"
    );

    let output = Command::new(bin)
        .args([
            "install",
            "a=1.0",
            "--allow-downgrade",
            "--config",
            &config_arg,
            "--root",
            &rootfs_arg,
            "--db",
            &db_arg,
            "--cache",
            &cache_arg,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        LocalDb::open(&db).unwrap().list_installed().unwrap()[0].version_raw,
        "1.0"
    );
}
