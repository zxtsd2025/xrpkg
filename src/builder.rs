//! 原生源码构建与 `.xpkg` 打包。
//!
//! 构建脚本是 TOML：
//! ```toml
//! name = "hello"
//! version = "1.0"
//! source = "hello"
//! requires = []
//! build = ["cc -O2 -o hello hello.c"]
//! package = ["install -Dm755 hello \"$PKGDIR/usr/bin/hello\""]
//! ```
//!
//! 构建在 `/var/tmp/xrpkg-build/<unique>/` 下准备源码，默认在
//! `/var/tmp/xrpkg-build-sandbox` 的 chroot/mount namespace 中执行。
//! 源码缓存写入 `~/.cache/xrpkg/downloads`，日志写入 `~/.cache/xrpkg/logs`。

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use liblzma::read::XzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::capability::{Capability, Version};
use crate::hooks::{HookScript, ALL_PHASES};
use crate::xpkg::{ensure_directory, sanitize_relative, validate_symlink_target, write_file};

/// 本模块使用的错误类型；当前统一用可读字符串。
pub type Error = String;

/// 构建脚本的 TOML 结构。
#[derive(Debug, Deserialize)]
struct BuildScript {
    name: String,
    version: String,
    #[serde(default)]
    source: SourceSpec,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    build: Vec<String>,
    #[serde(default)]
    package: Vec<String>,
    #[serde(default)]
    hooks: HooksSpec,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    patches: Vec<String>,
}

/// 配方身份：只包含 `name` 和 `version`，供 build-world 决定归档名和数据库身份。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipeIdentity {
    pub name: String,
    pub version: String,
}

/// 只读取配方的 `name`/`version`，不下载源码、不执行 build/package。
pub fn read_recipe_identity(script_path: &Path) -> Result<RecipeIdentity, Error> {
    let script_text = fs::read_to_string(script_path)
        .map_err(|error| format!("读取构建脚本 `{}` 失败: {error}", script_path.display()))?;
    let script: BuildScript = toml::from_str(&script_text)
        .map_err(|error| format!("解析构建脚本 `{}` 失败: {error}", script_path.display()))?;
    Ok(RecipeIdentity {
        name: script.name,
        version: script.version,
    })
}

/// 源码来源：本地目录，或一组远程/本地归档。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SourceSpec {
    Local(String),
    Archives(Vec<SourceArchive>),
}

impl Default for SourceSpec {
    fn default() -> Self {
        Self::Local(".".to_string())
    }
}

/// 生命周期脚本路径，相对于构建脚本所在目录。
#[derive(Debug, Default, Deserialize)]
struct HooksSpec {
    pre_install: Option<String>,
    post_install: Option<String>,
    pre_remove: Option<String>,
    post_remove: Option<String>,
    pre_upgrade: Option<String>,
    post_upgrade: Option<String>,
}

/// 一个带 SHA256 的源码归档。
#[derive(Debug, Deserialize)]
struct SourceArchive {
    url: String,
    sha256: String,
}

/// `.xpkg` 内的 metadata.toml。
#[derive(Debug, Serialize)]
struct XpkgMetadata<'a> {
    name: &'a str,
    version: &'a str,
    requires: &'a [String],
}

/// 一次构建过程的临时工作目录，离开作用域自动删除。
struct BuildRoot {
    path: PathBuf,
}

impl BuildRoot {
    /// 在系统临时目录的 xrpkg-build 下创建唯一工作目录。
    fn create(name: &str, version: &str) -> Result<Self, Error> {
        let base = build_temp_root().join("xrpkg-build");
        fs::create_dir_all(&base)
            .map_err(|error| format!("创建构建临时根目录 `{}` 失败: {error}", base.display()))?;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("读取系统时间失败: {error}"))?
            .as_nanos();
        let path = base.join(format!("{name}-{version}-{}-{nonce}", std::process::id()));
        fs::create_dir(&path)
            .map_err(|error| format!("创建构建工作目录 `{}` 失败: {error}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for BuildRoot {
    /// 删除整个构建工作目录，包括源码副本和未打包的 PKGDIR。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// 构建执行选项。
#[derive(Clone, Copy, Debug)]
pub struct BuildOptions {
    /// 是否在 user/mount namespace + chroot 中执行 build/package。
    pub sandbox: bool,
    /// 覆盖 MAKEFLAGS 的 -jN；None 时使用 [env] 或默认 -j8。
    pub jobs: Option<usize>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            sandbox: true,
            jobs: None,
        }
    }
}

/// 读取构建脚本，执行 build/package，生成 `<name>-<version>.xpkg`。
pub fn build_package(script_path: &Path, output_dir: &Path) -> Result<PathBuf, Error> {
    build_package_with_options(script_path, output_dir, BuildOptions::default())
}

/// 带选项的构建入口；`--no-sandbox` 使用 `sandbox=false`。
pub fn build_package_with_options(
    script_path: &Path,
    output_dir: &Path,
    options: BuildOptions,
) -> Result<PathBuf, Error> {
    let script_text = fs::read_to_string(script_path)
        .map_err(|error| format!("读取构建脚本 `{}` 失败: {error}", script_path.display()))?;
    let script: BuildScript = toml::from_str(&script_text)
        .map_err(|error| format!("解析构建脚本 `{}` 失败: {error}", script_path.display()))?;
    validate_script(&script)?;

    let recipe_dir = script_path.parent().unwrap_or_else(|| Path::new("."));
    let build_root = BuildRoot::create(&script.name, &script.version)?;
    let source_copy = build_root.path.join("source");
    let pkgdir = build_root.path.join("pkgdir");
    let home = build_root.path.join("home");
    let tmp = build_root.path.join("tmp");
    fs::create_dir_all(&pkgdir)
        .map_err(|error| format!("创建 PKGDIR `{}` 失败: {error}", pkgdir.display()))?;
    fs::create_dir_all(&home).map_err(|error| format!("创建构建 HOME 失败: {error}"))?;
    fs::create_dir_all(&tmp).map_err(|error| format!("创建构建 TMPDIR 失败: {error}"))?;
    prepare_source(&script.source, recipe_dir, &source_copy)?;
    copy_patches(&script.patches, recipe_dir, &source_copy)?;
    let makeflags = options
        .jobs
        .map(|jobs| format!("-j{jobs}"))
        .or_else(|| script.env.get("MAKEFLAGS").cloned())
        .unwrap_or_else(|| "-j8".to_string());
    let log_dir = cache_root()?.join("logs");
    fs::create_dir_all(&log_dir).map_err(|error| format!("创建日志目录失败: {error}"))?;
    let build_log = log_dir.join(format!("{}-build.log", script.name));
    let package_log = log_dir.join(format!("{}-package.log", script.name));
    let _ = fs::write(&build_log, b"");
    let _ = fs::write(&package_log, b"");
    println!("==> 构建日志: {}", build_log.display());

    if options.sandbox {
        run_steps_sandboxed(
            "build",
            &script.build,
            &source_copy,
            &pkgdir,
            &makeflags,
            &build_log,
        )?;
        run_steps_sandboxed(
            "package",
            &script.package,
            &source_copy,
            &pkgdir,
            &makeflags,
            &package_log,
        )?;
    } else {
        let environment = BuildEnvironment {
            source_dir: &source_copy,
            pkgdir: &pkgdir,
            home: &home,
            tmp: &tmp,
            env: &script.env,
            makeflags: &makeflags,
        };
        run_steps("build", &script.build, &environment, &build_log)?;
        run_steps("package", &script.package, &environment, &package_log)?;
    }

    if !has_regular_files(&pkgdir)? {
        return Err("package 步骤没有在 PKGDIR 中产生任何普通文件".to_string());
    }

    fs::create_dir_all(output_dir)
        .map_err(|error| format!("创建输出目录 `{}` 失败: {error}", output_dir.display()))?;
    let output = output_dir.join(format!("{}-{}.xpkg", script.name, script.version));
    let metadata = XpkgMetadata {
        name: &script.name,
        version: &script.version,
        requires: &script.requires,
    };
    let hooks = load_hooks(&script.hooks, recipe_dir)?;
    write_xpkg(&pkgdir, &metadata, &hooks, &output)?;
    Ok(output)
}

/// 在沙箱中执行所有阶段命令。
fn run_steps_sandboxed(
    stage: &str,
    steps: &[String],
    source_dir: &Path,
    pkgdir: &Path,
    makeflags: &str,
    log_path: &Path,
) -> Result<(), Error> {
    let sandbox = BuildSandbox::create(source_dir, pkgdir, makeflags)?;
    for (index, step) in steps.iter().enumerate() {
        println!("==> {stage}[{}] (sandbox): {step}", index + 1);
        let mut command = Command::new("unshare");
        command
            .args([
                "--user",
                "--map-root-user",
                "--mount",
                "--pid",
                "--fork",
                "/bin/sh",
            ])
            .arg(sandbox.setup_path())
            .arg(step);
        run_logged(stage, index, step, command, log_path)
            .map_err(|error| format!("构建沙箱: {error}"))?;
    }
    Ok(())
}

/// 启动命令，实时把 stdout/stderr 同时写到终端和日志。
fn run_logged(
    stage: &str,
    index: usize,
    step: &str,
    mut command: Command,
    log_path: &Path,
) -> Result<(), Error> {
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建日志目录失败: {error}"))?;
    }
    let log = Arc::new(Mutex::new(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .map_err(|error| format!("打开构建日志 `{}` 失败: {error}", log_path.display()))?,
    ));
    {
        let mut file = log.lock().map_err(|_| "构建日志锁已损坏".to_string())?;
        writeln!(file, "\n==> {stage}[{}]: {step}", index + 1)
            .map_err(|error| format!("写入构建日志失败: {error}"))?;
        let _ = file.flush();
    }

    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("启动 {stage} 命令 `{step}` 失败: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取命令 stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法读取命令 stderr".to_string())?;
    let stdout_log = Arc::clone(&log);
    let stderr_log = Arc::clone(&log);
    let stdout_thread = thread::spawn(move || stream_output(stdout, false, stdout_log));
    let stderr_thread = thread::spawn(move || stream_output(stderr, true, stderr_log));
    let status = child
        .wait()
        .map_err(|error| format!("等待 {stage} 命令失败: {error}"))?;
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();

    if !status.success() {
        let exit = status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string());
        return Err(format!(
            "{stage} 步骤 {} 失败（退出码 {exit}）: {step}",
            index + 1
        ));
    }
    Ok(())
}

fn stream_output<R: Read>(mut reader: R, is_stderr: bool, log: Arc<Mutex<File>>) {
    let mut buffer = [0u8; 8192];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(_) => break,
        };
        let bytes = &buffer[..count];
        if is_stderr {
            let _ = io::stderr().write_all(bytes);
            let _ = io::stderr().flush();
        } else {
            let _ = io::stdout().write_all(bytes);
            let _ = io::stdout().flush();
        }
        if let Ok(mut file) = log.lock() {
            let _ = file.write_all(bytes);
            let _ = file.flush();
        }
    }
}

/// 创建中途失败时兜底删除临时沙箱。
struct TempGuard(PathBuf);

impl Drop for TempGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 构建沙箱根，离开作用域时删除；所有挂载都在临时 mount namespace 中，
/// namespace 退出后自动消失，不会污染宿主机。
struct BuildSandbox {
    path: PathBuf,
    setup: PathBuf,
}

impl BuildSandbox {
    fn create(source_dir: &Path, pkgdir: &Path, makeflags: &str) -> Result<Self, Error> {
        let base = build_temp_root().join("xrpkg-build-sandbox");
        fs::create_dir_all(&base).map_err(|error| format!("创建构建沙箱根目录失败: {error}"))?;
        let path = base.join(format!("sandbox-{}-{}", std::process::id(), nonce()));
        fs::create_dir(&path)
            .map_err(|error| format!("创建构建沙箱 `{}` 失败: {error}", path.display()))?;
        let guard = TempGuard(path.clone());

        for directory in ["usr", "proc", "dev", "tmp", "src", "pkg"] {
            fs::create_dir_all(path.join(directory))
                .map_err(|error| format!("创建沙箱目录 `{directory}` 失败: {error}"))?;
        }
        std::os::unix::fs::symlink("usr/bin", path.join("bin"))
            .map_err(|error| format!("创建沙箱 /bin 失败: {error}"))?;
        std::os::unix::fs::symlink("usr/lib", path.join("lib"))
            .map_err(|error| format!("创建沙箱 /lib 失败: {error}"))?;
        std::os::unix::fs::symlink("usr/lib", path.join("lib64"))
            .map_err(|error| format!("创建沙箱 /lib64 失败: {error}"))?;

        let setup = path.join("setup.sh");
        let setup_text = format!(
            "set -e\n\
             mount --make-rprivate /\n\
             mount --rbind -o ro /usr {sandbox}/usr\n\
             mount --rbind -o ro /proc {sandbox}/proc\n\
             mount --rbind -o ro /dev {sandbox}/dev\n\
             mount -t tmpfs -o size=2g,mode=1777 tmpfs {sandbox}/tmp\n\
             mount --bind {source} {sandbox}/src\n\
             mount --bind {pkg} {sandbox}/pkg\n\
             exec /usr/sbin/chroot {sandbox} /usr/bin/env -i \
                 PATH=/usr/bin:/bin PKGDIR=/pkg SRCDIR=/src \
                 HOME=/tmp TMPDIR=/tmp MAKEFLAGS={makeflags} \
                 /bin/bash -e -c 'cd /src && exec /bin/bash -e -c \"$1\"' _ \"$1\"\n",
            sandbox = shell_quote(&path),
            source = shell_quote(source_dir),
            pkg = shell_quote(pkgdir),
            makeflags = quote_arg(makeflags),
        );
        fs::write(&setup, setup_text)
            .map_err(|error| format!("写入构建沙箱 setup 失败: {error}"))?;
        std::mem::forget(guard);
        Ok(Self { path, setup })
    }

    fn setup_path(&self) -> &Path {
        &self.setup
    }
}

impl Drop for BuildSandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
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

/// 把配方目录中的补丁复制到源码根，供 build 步骤使用。
fn copy_patches(patches: &[String], recipe_dir: &Path, source_root: &Path) -> Result<(), Error> {
    for patch in patches {
        let relative = Path::new(patch);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(format!("补丁路径 `{patch}` 必须是配方目录内的相对路径"));
        }
        let source = recipe_dir.join(relative);
        let file_name = source
            .file_name()
            .ok_or_else(|| format!("补丁路径 `{patch}` 没有文件名"))?;
        let destination = source_root.join(file_name);
        fs::copy(&source, &destination)
            .map_err(|error| format!("复制补丁 `{}` 失败: {error}", source.display()))?;
    }
    Ok(())
}

/// 构建命令执行时需要的目录。
struct BuildEnvironment<'a> {
    source_dir: &'a Path,
    pkgdir: &'a Path,
    home: &'a Path,
    tmp: &'a Path,
    env: &'a BTreeMap<String, String>,
    makeflags: &'a str,
}

/// 校验名称、版本和 native Capability。
fn validate_script(script: &BuildScript) -> Result<(), Error> {
    if script.name.is_empty()
        || script
            .name
            .chars()
            .any(|character| character.is_whitespace() || matches!(character, '/' | '\\'))
    {
        return Err(format!("构建脚本中的包名 `{}` 非法", script.name));
    }
    Version::parse(&script.version)
        .map_err(|error| format!("构建脚本中的版本 `{}` 非法: {error}", script.version))?;
    for requirement in &script.requires {
        Capability::parse(requirement)
            .map_err(|error| format!("构建脚本中的 requires 项 `{requirement}` 非法: {error}"))?;
    }
    Ok(())
}

/// 执行构建或打包命令，并实时回显/写日志。
fn run_steps(
    stage: &str,
    steps: &[String],
    environment: &BuildEnvironment<'_>,
    log_path: &Path,
) -> Result<(), Error> {
    for (index, step) in steps.iter().enumerate() {
        println!("==> {stage}[{}]: {step}", index + 1);
        let mut command = Command::new("bash");
        command
            .arg("-e")
            .arg("-c")
            .arg(step)
            .current_dir(environment.source_dir)
            .envs(environment.env)
            .env("MAKEFLAGS", environment.makeflags)
            .env("PKGDIR", environment.pkgdir)
            .env("DESTDIR", environment.pkgdir)
            .env("SRCDIR", environment.source_dir)
            .env("BUILDDIR", environment.source_dir)
            .env("HOME", environment.home)
            .env("TMPDIR", environment.tmp);
        run_logged(stage, index, step, command, log_path)?;
    }
    Ok(())
}

/// 准备构建源码：本地目录直接复制，归档来源先下载/复制、校验，再解压。
fn prepare_source(spec: &SourceSpec, recipe_dir: &Path, source_root: &Path) -> Result<(), Error> {
    match spec {
        SourceSpec::Local(path) => {
            let source_dir = resolve_local_source(path, recipe_dir);
            if !source_dir.is_dir() {
                return Err(format!(
                    "源码目录 `{}` 不存在或不是目录",
                    source_dir.display()
                ));
            }
            copy_tree(&source_dir, source_root)
        }
        SourceSpec::Archives(archives) => {
            if archives.is_empty() {
                return Err("source 归档列表不能为空".to_string());
            }
            fs::create_dir_all(source_root).map_err(|error| {
                format!("创建 SRCDIR `{}` 失败: {error}", source_root.display())
            })?;
            let downloads = downloads_dir()?;
            for archive in archives {
                let path = fetch_source(archive, recipe_dir, &downloads)?;
                println!("==> 解压源码: {}", path.display());
                extract_archive(&path, source_root)?;
            }
            Ok(())
        }
    }
}

/// 解析本地路径或 file:// URL。
fn resolve_local_source(spec: &str, recipe_dir: &Path) -> PathBuf {
    let raw = if let Some(rest) = spec.strip_prefix("file://") {
        PathBuf::from(rest.strip_prefix("localhost").unwrap_or(rest))
    } else {
        PathBuf::from(spec)
    };
    if raw.is_absolute() {
        raw
    } else {
        recipe_dir.join(raw)
    }
}

/// 遵循 XDG 的源码下载目录：`$XDG_CACHE_HOME/xrpkg/downloads`。
fn downloads_dir() -> Result<PathBuf, Error> {
    let directory = cache_root()?.join("downloads");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("创建下载目录 `{}` 失败: {error}", directory.display()))?;
    Ok(directory)
}

/// 返回 XDG 缓存根：`$XDG_CACHE_HOME` 或 `$HOME/.cache`；build-world 也用它定位编排日志。
pub fn cache_root() -> Result<PathBuf, Error> {
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(cache).join("xrpkg"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "无法确定 HOME，无法定位 XDG 缓存目录".to_string())?;
    Ok(PathBuf::from(home).join(".cache/xrpkg"))
}

/// 构建临时根；默认 `/var/tmp`，可用 XRPKG_BUILD_TMP 覆盖。
fn build_temp_root() -> PathBuf {
    std::env::var_os("XRPKG_BUILD_TMP")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/tmp"))
}

/// 从 URL 或本地路径的最后一段提取下载文件名。
fn source_filename(spec: &str) -> Result<String, Error> {
    let without_fragment = spec.split('#').next().unwrap_or(spec);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let name = without_query
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("无法从源码地址 `{spec}` 推断归档文件名"));
    }
    Ok(name.to_string())
}

/// 获取源码到 XDG 缓存 downloads，并验证 SHA256。
fn fetch_source(
    archive: &SourceArchive,
    recipe_dir: &Path,
    downloads: &Path,
) -> Result<PathBuf, Error> {
    let filename = source_filename(&archive.url)?;
    let cached = downloads.join(&filename);
    if cached.is_file() && verify_sha256(&cached, &archive.sha256).is_ok() {
        println!("==> 源码缓存命中: {}", cached.display());
        return Ok(cached);
    }

    println!("==> 获取源码: {}", archive.url);

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("读取系统时间失败: {error}"))?
        .as_nanos();
    let partial = downloads.join(format!(".{filename}.{}-{nonce}.part", std::process::id()));
    let result = if archive.url.starts_with("http://") || archive.url.starts_with("https://") {
        download_http(&archive.url, &partial)
    } else {
        let source = resolve_local_source(&archive.url, recipe_dir);
        if !source.is_file() {
            return Err(format!("本地源码文件 `{}` 不存在", source.display()));
        }
        fs::copy(&source, &partial).map(|_| ()).map_err(|error| {
            format!(
                "复制本地源码 `{}` -> `{}` 失败: {error}",
                source.display(),
                partial.display()
            )
        })
    };
    if let Err(error) = result {
        let _ = fs::remove_file(&partial);
        return Err(error);
    }

    if let Err(error) = verify_sha256(&partial, &archive.sha256) {
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    println!("==> SHA256 校验通过: {}", cached.display());
    fs::rename(&partial, &cached).map_err(|error| {
        let _ = fs::remove_file(&partial);
        format!("保存源码缓存 `{}` 失败: {error}", cached.display())
    })?;
    Ok(cached)
}

/// 使用 ureq 的 blocking API 下载。
fn download_http(url: &str, destination: &Path) -> Result<(), Error> {
    let response = ureq::get(url)
        .set("User-Agent", "xrpkg/0.1")
        .call()
        .map_err(|error| format!("下载 `{url}` 失败: {error}"))?;
    let mut reader = response.into_reader();
    let mut file = File::create(destination)
        .map_err(|error| format!("创建下载临时文件 `{}` 失败: {error}", destination.display()))?;
    io::copy(&mut reader, &mut file)
        .map_err(|error| format!("写入下载文件 `{}` 失败: {error}", destination.display()))?;
    file.sync_all()
        .map_err(|error| format!("刷新下载文件 `{}` 失败: {error}", destination.display()))
}

/// 计算文件的 SHA256 十六进制字符串。
fn sha256_file(path: &Path) -> Result<String, Error> {
    let mut file = File::open(path)
        .map_err(|error| format!("打开源码文件 `{}` 失败: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("读取源码文件 `{}` 失败: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// 校验文件哈希；不匹配时返回期望值和实际值。
fn verify_sha256(path: &Path, expected: &str) -> Result<(), Error> {
    let actual = sha256_file(path)?;
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!(
            "源码 `{}` 的 SHA256 不匹配: 期望 {expected}，实际 {actual}",
            path.display()
        ))
    }
}

/// 支持的源码归档压缩格式。
enum ArchiveFormat {
    Gzip,
    Xz,
}

/// 通过魔数识别 .tar.gz 和 .tar.xz，避免依赖 URL 后缀。
fn detect_archive_format(path: &Path) -> Result<ArchiveFormat, Error> {
    let mut file = File::open(path)
        .map_err(|error| format!("打开源码归档 `{}` 失败: {error}", path.display()))?;
    let mut magic = [0u8; 6];
    file.read_exact(&mut magic)
        .map_err(|error| format!("读取源码归档魔数 `{}` 失败: {error}", path.display()))?;
    if magic[..2] == [0x1f, 0x8b] {
        Ok(ArchiveFormat::Gzip)
    } else if magic == [0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00] {
        Ok(ArchiveFormat::Xz)
    } else {
        Err(format!(
            "无法识别源码归档 `{}` 的压缩格式（仅支持 gzip/xz）",
            path.display()
        ))
    }
}

/// 解压 tar.gz / tar.xz 到 SRCDIR，并复用安全路径检查。
fn extract_archive(path: &Path, destination: &Path) -> Result<(), Error> {
    let format = detect_archive_format(path)?;
    let file = File::open(path)
        .map_err(|error| format!("打开源码归档 `{}` 失败: {error}", path.display()))?;
    let reader: Box<dyn Read> = match format {
        ArchiveFormat::Gzip => Box::new(GzDecoder::new(file)),
        ArchiveFormat::Xz => Box::new(XzDecoder::new(file)),
    };
    extract_tar(reader, destination)
}

/// 安全遍历 TAR，拒绝 ..、绝对路径、符号链接和硬链接。
fn extract_tar<R: Read>(reader: R, destination: &Path) -> Result<(), Error> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive
        .entries()
        .map_err(|error| format!("读取源码 TAR 目录失败: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("读取源码 TAR 条目失败: {error}"))?;
        let entry_type = entry.header().entry_type();
        if matches!(
            entry_type,
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader
        ) {
            continue;
        }
        let path = entry
            .path()
            .map_err(|error| format!("读取源码 TAR 路径失败: {error}"))?
            .into_owned();
        let normalized = path.strip_prefix(".").unwrap_or(&path).to_path_buf();
        if normalized.as_os_str().is_empty() {
            if entry.header().entry_type().is_dir() {
                continue;
            }
            return Err(format!("源码 TAR 路径 `{}` 非法", path.display()));
        }
        let relative = sanitize_relative(&normalized)
            .map_err(|error| format!("源码 TAR 路径 `{}` 非法: {error}", path.display()))?;

        if entry_type.is_dir() {
            ensure_directory(destination, &relative)?;
        } else if entry_type.is_file() {
            let size = entry.size();
            let mode = entry.header().mode().unwrap_or(0o644);
            write_file(destination, &relative, &mut entry, size, mode)?;
        } else if entry_type.is_symlink() {
            let target = entry
                .link_name()
                .map_err(|error| format!("读取源码符号链接目标失败: {error}"))?
                .ok_or_else(|| format!("源码符号链接 `{}` 缺少目标", path.display()))?;
            write_source_symlink(destination, &relative, target.as_ref())?;
        } else if entry_type.is_hard_link() {
            let target = entry
                .link_name()
                .map_err(|error| format!("读取源码硬链接目标失败: {error}"))?
                .ok_or_else(|| format!("源码硬链接 `{}` 缺少目标", path.display()))?;
            write_source_hard_link(destination, &relative, target.as_ref())?;
        } else {
            return Err(format!("源码归档包含不支持的文件类型 `{}`", path.display()));
        }
    }
    Ok(())
}

/// 在源码树内创建相对符号链接，拒绝绝对路径和逃出源码根的路径。
fn write_source_symlink(root: &Path, relative: &Path, target: &Path) -> Result<(), Error> {
    validate_source_link_target(relative, target)?;
    if let Some(parent) = relative.parent() {
        if !parent.as_os_str().is_empty() {
            ensure_directory(root, parent)?;
        }
    }
    let destination = root.join(relative);
    let _ = fs::remove_file(&destination);
    std::os::unix::fs::symlink(target, &destination).map_err(|error| {
        format!(
            "创建源码符号链接 `{}` -> `{}` 失败: {error}",
            destination.display(),
            target.display()
        )
    })
}

/// 创建源码树内硬链接。
fn write_source_hard_link(root: &Path, relative: &Path, target: &Path) -> Result<(), Error> {
    validate_source_link_target(relative, target)?;
    let target = root.join(target);
    if !target.is_file() {
        return Err(format!("源码硬链接目标 `{}` 不存在", target.display()));
    }
    if let Some(parent) = relative.parent() {
        if !parent.as_os_str().is_empty() {
            ensure_directory(root, parent)?;
        }
    }
    fs::hard_link(&target, root.join(relative)).map_err(|error| {
        format!(
            "创建源码硬链接 `{}` -> `{}` 失败: {error}",
            relative.display(),
            target.display()
        )
    })
}

/// 词法校验链接目标位于源码根内。
fn validate_source_link_target(relative: &Path, target: &Path) -> Result<(), Error> {
    if target.is_absolute() {
        return Err(format!(
            "源码链接 `{}` 的目标是绝对路径",
            relative.display()
        ));
    }
    let mut depth = relative.parent().map_or(0, |parent| {
        parent
            .components()
            .filter(|component| matches!(component, std::path::Component::Normal(_)))
            .count()
    });
    for component in target.components() {
        match component {
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if depth == 0 {
                    return Err(format!(
                        "源码链接 `{}` 的目标 `{}` 逃出源码根",
                        relative.display(),
                        target.display()
                    ));
                }
                depth -= 1;
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(format!(
                    "源码链接 `{}` 的目标不是相对路径",
                    relative.display()
                ))
            }
        }
    }
    Ok(())
}

/// 递归复制源码，保留目录结构、文件权限和 Unix 符号链接。
fn copy_tree(source: &Path, destination: &Path) -> Result<(), Error> {
    fs::create_dir_all(destination)
        .map_err(|error| format!("创建源码副本目录 `{}` 失败: {error}", destination.display()))?;

    let mut entries = fs::read_dir(source)
        .map_err(|error| format!("读取源码目录 `{}` 失败: {error}", source.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取源码目录项失败: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|error| format!("读取 `{}` 类型失败: {error}", from.display()))?;
        if file_type.is_dir() {
            copy_tree(&from, &to)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to).map_err(|error| {
                format!(
                    "复制源码文件 `{}` -> `{}` 失败: {error}",
                    from.display(),
                    to.display()
                )
            })?;
        } else if file_type.is_symlink() {
            copy_symlink(&from, &to)?;
        } else {
            return Err(format!("源码中包含不支持的条目 `{}`", from.display()));
        }
    }
    Ok(())
}

#[cfg(unix)]
/// 复制符号链接本身，而不是跟随链接目标。
fn copy_symlink(from: &Path, to: &Path) -> Result<(), Error> {
    let target = fs::read_link(from)
        .map_err(|error| format!("读取符号链接 `{}` 失败: {error}", from.display()))?;
    std::os::unix::fs::symlink(&target, to).map_err(|error| {
        format!(
            "创建源码符号链接 `{}` -> `{}` 失败: {error}",
            to.display(),
            target.display()
        )
    })
}

#[cfg(not(unix))]
/// 非 Unix 平台暂不支持符号链接源码。
fn copy_symlink(from: &Path, _to: &Path) -> Result<(), Error> {
    Err(format!(
        "当前平台不支持复制源码符号链接 `{}`",
        from.display()
    ))
}

/// 判断 PKGDIR 是否至少包含一个普通文件。
fn has_regular_files(root: &Path) -> Result<bool, Error> {
    let mut entries = fs::read_dir(root)
        .map_err(|error| format!("读取 PKGDIR `{}` 失败: {error}", root.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取 PKGDIR 条目失败: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let file_type = entry
            .file_type()
            .map_err(|error| format!("读取 `{}` 类型失败: {error}", entry.path().display()))?;
        if file_type.is_file() {
            return Ok(true);
        }
        if file_type.is_dir() && has_regular_files(&entry.path())? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 读取构建脚本声明的生命周期脚本。
fn load_hooks(spec: &HooksSpec, recipe_dir: &Path) -> Result<Vec<HookScript>, Error> {
    let paths = [
        (ALL_PHASES[0], spec.pre_install.as_deref()),
        (ALL_PHASES[1], spec.post_install.as_deref()),
        (ALL_PHASES[2], spec.pre_remove.as_deref()),
        (ALL_PHASES[3], spec.post_remove.as_deref()),
        (ALL_PHASES[4], spec.pre_upgrade.as_deref()),
        (ALL_PHASES[5], spec.post_upgrade.as_deref()),
    ];
    let mut hooks = Vec::new();
    for (phase, path) in paths {
        let Some(path) = path else {
            continue;
        };
        let path = Path::new(path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            recipe_dir.join(path)
        };
        let script = fs::read(&path)
            .map_err(|error| format!("读取 hook `{}` 失败: {error}", path.display()))?;
        if script.len() > 1024 * 1024 {
            return Err(format!("hook `{}` 超过 1 MiB", path.display()));
        }
        hooks.push(HookScript {
            phase: phase.to_string(),
            script,
        });
    }
    Ok(hooks)
}

/// 把 PKGDIR 的内容写成标准 `.xpkg`：metadata.toml + files/。
fn write_xpkg(
    pkgdir: &Path,
    metadata: &XpkgMetadata<'_>,
    hooks: &[HookScript],
    output: &Path,
) -> Result<(), Error> {
    let temp = output.with_extension("xpkg.tmp");
    let result = write_xpkg_inner(pkgdir, metadata, hooks, &temp);
    if let Err(error) = result {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    fs::rename(&temp, output).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("写入最终 `.xpkg` `{}` 失败: {error}", output.display())
    })
}

/// 实际写 tar.zst 到临时文件。
fn write_xpkg_inner(
    pkgdir: &Path,
    metadata: &XpkgMetadata<'_>,
    hooks: &[HookScript],
    temp: &Path,
) -> Result<(), Error> {
    let metadata_text =
        toml::to_string(metadata).map_err(|error| format!("序列化 metadata.toml 失败: {error}"))?;
    let file = File::create(temp)
        .map_err(|error| format!("创建临时 `.xpkg` `{}` 失败: {error}", temp.display()))?;
    let encoder = zstd::stream::write::Encoder::new(file, 3)
        .map_err(|error| format!("创建 ZSTD 编码器失败: {error}"))?;
    let mut builder = tar::Builder::new(encoder);

    let mut header = tar::Header::new_gnu();
    header.set_size(metadata_text.len() as u64);
    header.set_mode(0o644);
    builder
        .append_data(&mut header, "metadata.toml", metadata_text.as_bytes())
        .map_err(|error| format!("写入 metadata.toml 失败: {error}"))?;
    for hook in hooks {
        let mut header = tar::Header::new_gnu();
        header.set_size(hook.script.len() as u64);
        header.set_mode(0o644);
        builder
            .append_data(
                &mut header,
                format!("{}.sh", hook.phase),
                hook.script.as_slice(),
            )
            .map_err(|error| format!("写入 {} hook 失败: {error}", hook.phase))?;
    }
    append_directory(&mut builder, Path::new("files"))?;
    let mut hardlinks = HashMap::new();
    append_package_tree(
        &mut builder,
        pkgdir,
        Path::new("files"),
        Path::new(""),
        &mut hardlinks,
    )?;

    let encoder = builder
        .into_inner()
        .map_err(|error| format!("完成 TAR 归档失败: {error}"))?;
    encoder
        .finish()
        .map_err(|error| format!("完成 ZSTD 流失败: {error}"))?;
    Ok(())
}

/// 添加一个目录条目。
fn append_directory(
    builder: &mut tar::Builder<zstd::stream::write::Encoder<'static, File>>,
    path: &Path,
) -> Result<(), Error> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, path, io::empty())
        .map_err(|error| format!("写入目录 `{}` 失败: {error}", path.display()))
}

/// 递归把 PKGDIR 内容追加到 `files/` 下，保留符号链接和硬链接。
fn append_package_tree(
    builder: &mut tar::Builder<zstd::stream::write::Encoder<'static, File>>,
    pkgdir: &Path,
    archive_root: &Path,
    relative: &Path,
    hardlinks: &mut HashMap<(u64, u64), PathBuf>,
) -> Result<(), Error> {
    let directory = pkgdir.join(relative);
    let mut entries = fs::read_dir(&directory)
        .map_err(|error| format!("读取 PKGDIR 目录 `{}` 失败: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取 PKGDIR 目录项失败: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let name = entry.file_name();
        let child_relative = relative.join(&name);
        let archive_path = archive_root.join(&child_relative);
        let file_type = entry
            .file_type()
            .map_err(|error| format!("读取 `{}` 类型失败: {error}", entry.path().display()))?;

        if file_type.is_dir() {
            append_directory(builder, &archive_path)?;
            append_package_tree(builder, pkgdir, archive_root, &child_relative, hardlinks)?;
        } else if file_type.is_file() {
            let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
                format!("读取文件元数据 `{}` 失败: {error}", entry.path().display())
            })?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;

                if metadata.nlink() > 1 {
                    let key = (metadata.dev(), metadata.ino());
                    if let Some(target) = hardlinks.get(&key) {
                        append_hardlink(builder, &archive_path, target, &metadata)?;
                        continue;
                    }
                    hardlinks.insert(key, archive_path.clone());
                }
            }

            builder
                .append_path_with_name(entry.path(), &archive_path)
                .map_err(|error| {
                    format!(
                        "打包文件 `{}` 到 `{}` 失败: {error}",
                        entry.path().display(),
                        archive_path.display()
                    )
                })?;
        } else if file_type.is_symlink() {
            let link_target = fs::read_link(entry.path()).map_err(|error| {
                format!("读取符号链接 `{}` 失败: {error}", entry.path().display())
            })?;
            validate_symlink_target(pkgdir, &child_relative, &link_target)?;
            append_symlink(builder, &archive_path, &link_target)?;
        } else {
            return Err(format!(
                "PKGDIR 中包含不支持的文件类型 `{}`",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

/// 写入符号链接条目。
fn append_symlink(
    builder: &mut tar::Builder<zstd::stream::write::Encoder<'static, File>>,
    path: &Path,
    target: &Path,
) -> Result<(), Error> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    builder
        .append_link(&mut header, path, target)
        .map_err(|error| {
            format!(
                "打包符号链接 `{}` -> `{}` 失败: {error}",
                path.display(),
                target.display()
            )
        })
}

/// 写入硬链接条目。
fn append_hardlink(
    builder: &mut tar::Builder<zstd::stream::write::Encoder<'static, File>>,
    path: &Path,
    target: &Path,
    metadata: &fs::Metadata,
) -> Result<(), Error> {
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    };
    #[cfg(not(unix))]
    let mode = 0o644;

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Link);
    header.set_size(0);
    header.set_mode(mode);
    builder
        .append_link(&mut header, path, target)
        .map_err(|error| {
            format!(
                "打包硬链接 `{}` -> `{}` 失败: {error}",
                path.display(),
                target.display()
            )
        })
}
