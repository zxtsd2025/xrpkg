//! 测试共享工具。
//!
//! 每个测试二进制都会编译整个模块；只用到其中一部分属于正常情况。
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use tempfile::TempDir;

/// 当前测试二进制独占的缓存根；进程存活期间不会被删除。
static TEST_CACHE_ROOT: OnceLock<TempDir> = OnceLock::new();

/// 为当前测试二进制建立独立缓存根，并让 `builder::cache_root` 指向它。
///
/// 每个测试二进制是独立进程，因此 `OnceLock` 只需初始化一次；同一二进制内
/// 的测试共享该目录，不会和并行运行的其他测试目标争用 `~/.cache/xrpkg`。
/// 必须在任何下载/构建之前调用。
pub fn isolate_cache() -> &'static Path {
    let directory = TEST_CACHE_ROOT.get_or_init(|| {
        let directory = TempDir::new().expect("创建测试缓存目录失败");
        std::env::set_var("XDG_CACHE_HOME", directory.path());
        directory
    });
    directory.path()
}

/// 测试用临时 GPG 身份：私钥在 GNUPGHOME，公钥导出为独立 keyring。
pub struct TestGpg {
    pub home: PathBuf,
    pub keyring: PathBuf,
    pub key_id: String,
}

impl TestGpg {
    pub fn new(temp: &Path) -> Self {
        let home = temp.join("gpg-home");
        fs::create_dir_all(&home).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let status = Command::new("gpg")
            .args([
                "--homedir",
                home.to_str().unwrap(),
                "--batch",
                "--passphrase",
                "",
                "--quick-generate-key",
                "xrpkg test <test@xrpkg.invalid>",
                "ed25519",
                "sign",
                "0",
            ])
            .status()
            .expect("生成测试 GPG 密钥失败");
        assert!(status.success());
        let output = Command::new("gpg")
            .args([
                "--homedir",
                home.to_str().unwrap(),
                "--with-colons",
                "--list-keys",
            ])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&output.stdout);
        let key_id = listing
            .lines()
            .find(|line| line.starts_with("fpr:"))
            .and_then(|line| line.split(':').nth(9))
            .expect("无法读取测试 GPG 指纹")
            .to_string();
        let keyring = temp.join("trusted.gpg");
        let exported = Command::new("gpg")
            .args(["--homedir", home.to_str().unwrap(), "--export", &key_id])
            .output()
            .unwrap();
        assert!(exported.status.success());
        fs::write(&keyring, exported.stdout).unwrap();
        Self {
            home,
            keyring,
            key_id,
        }
    }

    pub fn sign(&self, file: &Path) {
        let signature = xrpkg::sign::signature_path(file);
        let status = Command::new("gpg")
            .args([
                "--homedir",
                self.home.to_str().unwrap(),
                "--batch",
                "--yes",
                "--detach-sign",
                "--local-user",
                &self.key_id,
                "--output",
                signature.to_str().unwrap(),
                file.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "签名 {} 失败", file.display());
    }

    pub fn sign_repo(&self, repo_dir: &Path) {
        for entry in fs::read_dir(repo_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("xpkg") {
                self.sign(&path);
            }
        }
        self.sign(&repo_dir.join("xrpkg-repo.toml"));
    }
}

pub fn write_config(path: &Path, repos: &[(&str, &Path)], keyring: &Path) {
    let mut text = format!("keyring = \"{}\"\n", keyring.display());
    for (name, url) in repos {
        text.push_str(&format!(
            "[[repos]]\nname = \"{name}\"\nurl = \"file://{}\"\n",
            url.display()
        ));
    }
    fs::write(path, text).unwrap();
}
