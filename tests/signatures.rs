use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;
use xrpkg::builder::build_package;
use xrpkg::repo;
use xrpkg::state::LocalDb;

mod common;

fn build_fixture(temp: &TempDir, name: &str) -> PathBuf {
    let source = temp.path().join(format!("{name}-src"));
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("payload.txt"), format!("payload for {name}\n")).unwrap();
    let recipe = temp.path().join(format!("{name}.toml"));
    fs::write(
        &recipe,
        format!(
            r#"
name = "{name}"
version = "1.0"
source = "{name}-src"
requires = []
build = []
package = [
  "install -Dm644 payload.txt \"$PKGDIR/usr/share/signature-test/{name}.txt\"",
]
"#
        ),
    )
    .unwrap();
    build_package(&recipe, &temp.path().join(format!("out-{name}"))).unwrap()
}

fn setup(temp: &TempDir) -> (PathBuf, common::TestGpg, PathBuf, PathBuf) {
    let repo_dir = temp.path().join("repo");
    let gpg = common::TestGpg::new(temp.path());
    let package = build_fixture(temp, "signed");
    repo::repo_add(&repo_dir, &package).unwrap();
    gpg.sign_repo(&repo_dir);
    let config = temp.path().join("repos.toml");
    common::write_config(&config, &[("repo", &repo_dir)], &gpg.keyring);
    (repo_dir, gpg, config, package)
}

#[test]
fn signed_package_and_index_install_successfully() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let (_, _, config, _) = setup(&temp);
    let cache = temp.path().join("cache");
    repo::update(&config, &cache).unwrap();

    let mut db = LocalDb::open(":memory:").unwrap();
    let rootfs = temp.path().join("rootfs");
    repo::install_from_repositories(
        &mut db, &config, &cache, "signed", None, false, &rootfs, false,
    )
    .unwrap();

    assert!(rootfs.join("usr/share/signature-test/signed.txt").is_file());
    assert_eq!(db.list_installed().unwrap()[0].name, "signed");
}

#[test]
fn tampered_package_with_recomputed_sha256_is_rejected() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let (repo_dir, gpg, config, package) = setup(&temp);
    let cache = temp.path().join("cache");
    repo::update(&config, &cache).unwrap();

    // 追加垃圾字节后重新 repo-add，索引 SHA256 会更新，但旧 .sig 仍对应旧内容。
    let mut bytes = fs::read(&package).unwrap();
    bytes.extend_from_slice(b"tampered");
    fs::write(&package, bytes).unwrap();
    repo::repo_add(&repo_dir, &package).unwrap();
    gpg.sign(&repo_dir.join("xrpkg-repo.toml"));
    repo::update(&config, &cache).unwrap();

    let mut db = LocalDb::open(":memory:").unwrap();
    let rootfs = temp.path().join("rootfs");
    let error = repo::install_from_repositories(
        &mut db, &config, &cache, "signed", None, false, &rootfs, false,
    )
    .unwrap_err();

    assert!(
        error.contains("签名") || error.contains("signature"),
        "{error}"
    );
    assert!(db.list_installed().unwrap().is_empty());
    assert!(!rootfs.join("usr/share/signature-test/signed.txt").exists());
}

#[test]
fn unsigned_repository_index_is_rejected() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let repo_dir = temp.path().join("repo");
    let gpg = common::TestGpg::new(temp.path());
    let package = build_fixture(&temp, "unsigned");
    repo::repo_add(&repo_dir, &package).unwrap();
    let config = temp.path().join("repos.toml");
    common::write_config(&config, &[("repo", &repo_dir)], &gpg.keyring);

    let cache = temp.path().join("cache");
    let error = repo::update(&config, &cache).unwrap_err();

    assert!(error.contains("签名"), "{error}");
    assert!(!cache.join("indexes/repo.toml").exists());
}

#[test]
fn package_signed_by_another_key_is_rejected() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let (repo_dir, _gpg, config, _) = setup(&temp);
    let other = common::TestGpg::new(&temp.path().join("other"));
    let cache = temp.path().join("cache");
    repo::update(&config, &cache).unwrap();

    // 用另一把私钥重新签包，索引仍由原 key 签。
    other.sign(&repo_dir.join("signed-1.0.xpkg"));

    let mut db = LocalDb::open(":memory:").unwrap();
    let rootfs = temp.path().join("rootfs");
    let error = repo::install_from_repositories(
        &mut db, &config, &cache, "signed", None, false, &rootfs, false,
    )
    .unwrap_err();
    assert!(
        error.contains("签名") || error.contains("signature"),
        "{error}"
    );
    assert!(!rootfs.join("usr/share/signature-test/signed.txt").exists());
}
#[test]
fn repo_add_with_key_and_repo_sign_cli_create_detached_signatures() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let gpg = common::TestGpg::new(temp.path());
    let package = build_fixture(&temp, "cli-signed");
    let repo_dir = temp.path().join("repo");
    let bin = env!("CARGO_BIN_EXE_xrpkg");

    let output = std::process::Command::new(bin)
        .args([
            "repo-add",
            repo_dir.to_str().unwrap(),
            package.to_str().unwrap(),
            "--key",
            &gpg.key_id,
        ])
        .env("GNUPGHOME", &gpg.home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(repo_dir.join("cli-signed-1.0.xpkg.sig").is_file());
    assert!(repo_dir.join("xrpkg-repo.toml.sig").is_file());

    let index = repo_dir.join("xrpkg-repo.toml");
    fs::remove_file(xrpkg::sign::signature_path(&index)).unwrap();
    let output = std::process::Command::new(bin)
        .args(["repo-sign", &gpg.key_id, index.to_str().unwrap()])
        .env("GNUPGHOME", &gpg.home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    xrpkg::sign::verify_detached(&gpg.keyring, &xrpkg::sign::signature_path(&index), &index)
        .unwrap();
}
#[test]
fn missing_package_signature_is_rejected() {
    common::isolate_cache();
    let temp = TempDir::new().unwrap();
    let (repo_dir, _, config, _) = setup(&temp);
    let cache = temp.path().join("cache");
    repo::update(&config, &cache).unwrap();
    fs::remove_file(repo_dir.join("signed-1.0.xpkg.sig")).unwrap();

    let mut db = LocalDb::open(":memory:").unwrap();
    let rootfs = temp.path().join("rootfs");
    let error = repo::install_from_repositories(
        &mut db, &config, &cache, "signed", None, false, &rootfs, false,
    )
    .unwrap_err();
    assert!(
        error.contains("签名") || error.contains("signature"),
        "{error}"
    );
    assert!(db.list_installed().unwrap().is_empty());
}
