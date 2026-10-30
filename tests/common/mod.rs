use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
