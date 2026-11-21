//! 生命周期脚本与 chroot 沙箱执行。
//!
//! 沙箱使用 unprivileged user namespace + mount namespace：
//! - 目标 Rootfs 只读挂载？不，目标 Rootfs 只绑定到 `/target` 且保持可写；
//! - 宿主机 `/usr/bin` 只读绑定，提供 bash 和常用工具；
//! - bash 和工具依赖的动态库复制到沙箱；
//! - 不把宿主机 `/etc`、`/home`、`/` 暴露给脚本。

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ui;
use crate::xpkg::Rootfs;

/// 本模块使用的错误类型；当前统一用可读字符串。
pub type Error = String;

/// 生命周期阶段。
pub const PRE_INSTALL: &str = "pre_install";
pub const POST_INSTALL: &str = "post_install";
pub const PRE_REMOVE: &str = "pre_remove";
pub const POST_REMOVE: &str = "post_remove";
pub const PRE_UPGRADE: &str = "pre_upgrade";
pub const POST_UPGRADE: &str = "post_upgrade";

/// `.xpkg` 根目录中的脚本文件名。
pub const ALL_PHASES: [&str; 6] = [
    PRE_INSTALL,
    POST_INSTALL,
    PRE_REMOVE,
    POST_REMOVE,
    PRE_UPGRADE,
    POST_UPGRADE,
];

/// 一个生命周期脚本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookScript {
    pub phase: String,
    pub script: Vec<u8>,
}

/// 在 chroot 沙箱中执行一个阶段脚本。
pub fn run_hook(
    phase: &str,
    package_name: &str,
    package_version: &str,
    rootfs: &Rootfs,
    script: &[u8],
) -> Result<(), Error> {
    ui::step(&ui::pick(
        format!("执行 {phase} hook: {package_name} {package_version}"),
        format!("Running {phase} hook: {package_name} {package_version}"),
    ));
    let sandbox = Sandbox::create(rootfs, package_name, package_version, script)?;
    let output = Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--mount",
            "--pid",
            "--fork",
            "/bin/sh",
        ])
        .arg(sandbox.setup_path())
        .output()
        .map_err(|error| format!("启动 chroot 沙箱失败: {error}"))?;

    if !output.stdout.is_empty() {
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }

    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string());
        return Err(format!(
            "{phase} hook 失败（{package_name} {package_version}，退出码 {code}）"
        ));
    }
    Ok(())
}

/// 一次 hook 执行的沙箱目录；Drop 时清理。
struct Sandbox {
    path: PathBuf,
    setup: PathBuf,
}

impl Sandbox {
    /// 创建沙箱、复制 shell 依赖、写入 hook 和 setup 脚本。
    fn create(
        rootfs: &Rootfs,
        package_name: &str,
        package_version: &str,
        script: &[u8],
    ) -> Result<Self, Error> {
        let base = PathBuf::from("/var/tmp/xrpkg-hooks");
        fs::create_dir_all(&base).map_err(|error| format!("创建 hook 沙箱根目录失败: {error}"))?;
        let path = base.join(format!("hook-{}-{}", std::process::id(), nonce()));
        fs::create_dir(&path)
            .map_err(|error| format!("创建 hook 沙箱 `{}` 失败: {error}", path.display()))?;

        for directory in ["usr/bin", "usr/lib", "tmp", "etc", "target"] {
            fs::create_dir_all(path.join(directory))
                .map_err(|error| format!("创建沙箱目录 `{directory}` 失败: {error}"))?;
        }
        symlink("usr/bin", path.join("bin"))
            .map_err(|error| format!("创建沙箱 /bin 失败: {error}"))?;
        symlink("usr/lib", path.join("lib"))
            .map_err(|error| format!("创建沙箱 /lib 失败: {error}"))?;
        symlink("usr/lib", path.join("lib64"))
            .map_err(|error| format!("创建沙箱 /lib64 失败: {error}"))?;
        symlink("lib", path.join("usr/lib64"))
            .map_err(|error| format!("创建沙箱 /usr/lib64 失败: {error}"))?;

        copy_runtime_libraries(&path)?;

        let hook_path = path.join("hook.sh");
        fs::write(&hook_path, script)
            .map_err(|error| format!("写入 hook 脚本 `{}` 失败: {error}", hook_path.display()))?;

        let setup = path.join("setup.sh");
        let rootfs_path = rootfs.path().to_path_buf();
        let setup_text = format!(
            "set -e\n\
             mount --bind /usr/bin {sandbox}/usr/bin\n\
             mount -o remount,ro,bind {sandbox}/usr/bin\n\
             mount --bind {rootfs} {sandbox}/target\n\
             exec chroot {sandbox} /usr/bin/env \
                 PKG_NAME={pkg_name} PKG_VERSION={pkg_version} \
                 XRPKG_ROOTFS=/target PATH=/usr/bin:/bin HOME=/tmp TMPDIR=/tmp \
                 /bin/bash /hook.sh\n",
            sandbox = shell_quote(&path),
            rootfs = shell_quote(&rootfs_path),
            pkg_name = quote_arg(package_name),
            pkg_version = quote_arg(package_version),
        );
        fs::write(&setup, setup_text)
            .map_err(|error| format!("写入沙箱 setup 脚本 `{}` 失败: {error}", setup.display()))?;

        Ok(Self { path, setup })
    }

    fn setup_path(&self) -> &Path {
        &self.setup
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// 把 bash 和常用工具的 ELF 依赖复制到沙箱。
fn copy_runtime_libraries(sandbox: &Path) -> Result<(), Error> {
    let bash = fs::canonicalize("/bin/bash")
        .or_else(|_| fs::canonicalize("/usr/bin/bash"))
        .map_err(|error| format!("找不到宿主 bash: {error}"))?;
    let mut libraries = BTreeSet::new();
    collect_ldd_libraries(&bash, &mut libraries)?;

    let tools = [
        "sh", "env", "mkdir", "rm", "cp", "mv", "ln", "chmod", "chown", "cat", "touch", "install",
        "sed", "grep", "awk", "find", "xargs", "tar", "gzip", "xz", "zstd",
    ];
    for tool in tools {
        let path = Path::new("/usr/bin").join(tool);
        if path.is_file() {
            let _ = collect_ldd_libraries(&path, &mut libraries);
        }
    }

    for library in libraries {
        let target = sandbox.join(library.strip_prefix("/").unwrap_or(&library));
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("创建沙箱库目录失败: {error}"))?;
        }
        fs::copy(&library, &target).map_err(|error| {
            format!(
                "复制沙箱动态库 `{}` -> `{}` 失败: {error}",
                library.display(),
                target.display()
            )
        })?;
    }
    Ok(())
}

/// 运行 ldd，收集绝对路径依赖。
fn collect_ldd_libraries(binary: &Path, libraries: &mut BTreeSet<PathBuf>) -> Result<(), Error> {
    let output = Command::new("ldd")
        .arg(binary)
        .output()
        .map_err(|error| format!("运行 ldd `{}` 失败: {error}", binary.display()))?;
    if !output.status.success() {
        return Err(format!("ldd `{}` 失败", binary.display()));
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        for token in line.split_whitespace() {
            if token.starts_with('/') && Path::new(token).is_file() {
                libraries.insert(PathBuf::from(token));
                break;
            }
        }
    }
    Ok(())
}

fn shell_quote(path: &Path) -> String {
    quote_arg(&path.to_string_lossy())
}

fn quote_arg(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}
