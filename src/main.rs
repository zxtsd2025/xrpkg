//! xrpkg CLI 入口：Rootfs 初始化、内核/基础用户态构建、签名仓库安装/升级/降级/缓存清理、隔离构建、卸载/hooks，以及兼容轨道 .deb/.rpm/.pkg.tar.zst 的安装-记录-卸载闭环。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use xrpkg::abi::{AbiGuard, MAX_GLIBC_VERSION};
use xrpkg::builder;
use xrpkg::capability::{Capability, Version};
use xrpkg::compat_arch;
use xrpkg::compat_deb;
use xrpkg::compat_rpm;
use xrpkg::deps_map::{default_config_path, DependencyMapper};
use xrpkg::hooks::{self, HookScript, POST_INSTALL, PRE_INSTALL, PRE_REMOVE};
use xrpkg::installer;
use xrpkg::metadata::load_repository;
use xrpkg::mirrors;
use xrpkg::repo;
use xrpkg::resolvo_solver::{self, incremental_plan};
use xrpkg::sign;
use xrpkg::solver::{Package, SelectedPackage};
use xrpkg::state::{InstalledPackage, LocalDb, PackageInstall};
use xrpkg::sync;
use xrpkg::transaction::{self, Transaction};
use xrpkg::ui;
use xrpkg::uninstall;
use xrpkg::world;
use xrpkg::xpkg::{self, Rootfs};

/// 默认安装根目录；可以通过 `--root <dir>` 覆盖。
const DEFAULT_ROOTFS: &str = "/var/tmp/xrpkg-rootfs";

/// 恢复 Unix 默认的 SIGPIPE 行为。
///
/// Rust 运行时默认忽略 SIGPIPE，导致 `xrpkg --help | head` 这类管道在
/// 读端提前关闭时触发 println! panic。恢复 SIG_DFL 后进程会像常规 CLI
/// 一样安静退出，而不是打印 backtrace。
#[cfg(unix)]
fn restore_sigpipe() {
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    // SAFETY: SIG_DFL 是合法 handler；signal(2) 不访问 Rust 管理的状态。
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

fn main() {
    restore_sigpipe();
    if let Err(error) = run() {
        ui::error(&error);
        std::process::exit(1);
    }
}

/// 命令行模拟配置；没有引入 clap。
struct Cli {
    rootfs: PathBuf,
    deb: Option<PathBuf>,
    rpm: Option<PathBuf>,
    arch: Option<PathBuf>,
    build: Option<PathBuf>,
    remove: Option<String>,
    repo_add: Option<(PathBuf, PathBuf)>,
    repo_add_key: Option<String>,
    repo_sign: Option<(String, PathBuf)>,
    update: bool,
    clean: bool,
    search: Option<String>,
    install: Option<String>,
    sync: Option<String>,
    sync_all: bool,
    prefer_arch: bool,
    mirrors: Option<PathBuf>,
    repo_dir: Option<PathBuf>,
    config: Option<PathBuf>,
    no_hooks: bool,
    db: Option<PathBuf>,
    cache: Option<PathBuf>,
    allow_downgrade: bool,
    no_sandbox: bool,
    no_deps: bool,
    jobs: Option<usize>,
    init: bool,
    build_world: bool,
    list: bool,
    help: bool,
    version: bool,
}

/// 在 Rootfs 中生成最小可启动/可 chroot 的基础配置文件。
fn init_rootfs(rootfs: &Path) -> Result<(), String> {
    for directory in [
        "etc", "dev", "proc", "sys", "tmp", "run", "root", "home", "var", "usr/bin", "usr/lib",
    ] {
        fs::create_dir_all(rootfs.join(directory))
            .map_err(|error| format!("创建 Rootfs 目录 `{directory}` 失败: {error}"))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(rootfs.join("tmp"), fs::Permissions::from_mode(0o1777));
        let _ = fs::set_permissions(rootfs.join("root"), fs::Permissions::from_mode(0o700));
    }
    for (name, target) in [
        ("bin", "usr/bin"),
        ("sbin", "usr/bin"),
        ("lib", "usr/lib"),
        ("lib64", "usr/lib"),
    ] {
        let link = rootfs.join(name);
        if !link.exists() {
            std::os::unix::fs::symlink(target, &link)
                .map_err(|error| format!("创建 Rootfs 链接 `{name}` 失败: {error}"))?;
        }
    }
    let files = [
        (
            "etc/passwd",
            "root:x:0:0:root:/root:/bin/bash\nnobody:x:65534:65534:nobody:/:/sbin/nologin\n",
        ),
        ("etc/group", "root:x:0:\nnogroup:x:65534:\n"),
        (
            "etc/hosts",
            "127.0.0.1 localhost\n::1 localhost ip6-localhost ip6-loopback\n",
        ),
        (
            "etc/resolv.conf",
            "nameserver 1.1.1.1\nnameserver 8.8.8.8\n",
        ),
        (
            "etc/nsswitch.conf",
            "passwd: files\ngroup: files\nshadow: files\nhosts: files dns\n",
        ),
        ("etc/ld.so.conf", "/usr/local/lib\n/opt/lib\n"),
    ];
    for (path, contents) in files {
        let target = rootfs.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("创建配置目录 `{}` 失败: {error}", parent.display()))?;
        }
        fs::write(&target, contents)
            .map_err(|error| format!("写入配置 `{}` 失败: {error}", target.display()))?;
    }
    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 解析 `包名` 或 `包名=版本`；版本必须是 native Version。
fn parse_install_request(input: &str) -> Result<(String, Option<Version>), String> {
    let Some((name, version_text)) = input.split_once('=') else {
        return Ok((input.to_string(), None));
    };
    let name = name.trim();
    let version_text = version_text.trim();
    if name.is_empty() {
        return Err(ui::pick(
            format!("安装请求 `{input}` 缺少包名"),
            format!("Install request `{input}` is missing a package name"),
        ));
    }
    if name
        .chars()
        .any(|character| character.is_whitespace() || matches!(character, '/' | '\\' | '='))
    {
        return Err(ui::pick(
            format!("包名 `{name}` 非法"),
            format!("Invalid package name `{name}`"),
        ));
    }
    if version_text.is_empty() {
        return Err(ui::pick(
            format!("安装请求 `{input}` 缺少版本"),
            format!("Install request `{input}` is missing a version"),
        ));
    }
    let version = Version::parse(version_text).map_err(|error| {
        ui::pick(
            format!("安装请求 `{input}` 的版本非法: {error}"),
            format!("Invalid version in install request `{input}`: {error}"),
        )
    })?;
    Ok((name.to_string(), Some(version)))
}

/// 返回 `--db` 指定的数据库；否则根据二进制位置选择项目库或系统库。
fn database_path(project_root: &Path, cli: &Cli) -> PathBuf {
    if let Some(db) = &cli.db {
        return db.clone();
    }
    // 从构建目录内的二进制运行时沿用项目数据库；装进 /usr/bin 后自动使用
    // 系统数据库，避免 `xrpkg list` 误读开发目录里的空库。
    let project_db = project_root.join("var/lib/xrpkg/local.db");
    let system_db = PathBuf::from("/var/lib/xrpkg/local.db");
    match std::env::current_exe() {
        Ok(exe) if exe.starts_with(project_root) => project_db,
        Ok(_) => system_db,
        // 没有 /proc 的最小 chroot 无法读取 current_exe；系统数据库存在时优先它。
        Err(_) if system_db.is_file() => system_db,
        Err(_) => project_db,
    }
}

/// 返回 `--config` 指定的配置，否则使用 ~/.config/xrpkg/repos.toml。
fn resolve_config_path(cli: &Cli) -> Result<PathBuf, String> {
    match &cli.config {
        Some(path) => Ok(path.clone()),
        None => repo::default_config_path(),
    }
}

/// 返回 `--mirrors` 指定的镜像配置，否则使用项目内 config/mirrors.toml。
fn resolve_mirrors_path(project_root: &Path, cli: &Cli) -> PathBuf {
    cli.mirrors
        .clone()
        .unwrap_or_else(|| mirrors::default_config_path(project_root))
}

/// 加载仓库、解析参数，然后进入原生轨道或兼容轨道。
fn run() -> Result<(), String> {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cli = parse_cli()?;

    let foreign_count = usize::from(cli.deb.is_some())
        + usize::from(cli.rpm.is_some())
        + usize::from(cli.arch.is_some());
    let action_count = foreign_count
        + usize::from(cli.build.is_some())
        + usize::from(cli.remove.is_some())
        + usize::from(cli.repo_add.is_some())
        + usize::from(cli.repo_sign.is_some())
        + usize::from(cli.update)
        + usize::from(cli.clean)
        + usize::from(cli.init)
        + usize::from(cli.search.is_some())
        + usize::from(cli.install.is_some())
        + usize::from(cli.sync.is_some())
        + usize::from(cli.sync_all)
        + usize::from(cli.build_world)
        + usize::from(cli.list);
    if cli.help {
        ui::print_help();
        return Ok(());
    }
    if cli.version {
        ui::print_version();
        return Ok(());
    }
    if action_count > 1 {
        return Err(ui::pick(
            "构建、卸载、仓库、同步和兼容轨道操作只能选择一个",
            "Only one of build, remove, repository, sync, or compatibility operation may be selected",
        ));
    }
    if action_count == 0 {
        ui::print_help();
        return Ok(());
    }

    if cli.list {
        let db = LocalDb::open(database_path(project_root, &cli))?;
        list(&db)?;
        return Ok(());
    }

    if cli.build_world {
        // `--config` 在 build-world 下指向系统清单，默认 recipes/system.toml。
        let config_path = cli
            .config
            .clone()
            .unwrap_or_else(|| project_root.join("recipes/system.toml"));
        let repo_dir = cli
            .repo_dir
            .clone()
            .unwrap_or_else(|| project_root.join("repo"));
        let log_path = builder::cache_root()?.join("logs").join("build-world.log");
        let options = world::WorldOptions {
            sandbox: !cli.no_sandbox,
            jobs: cli.jobs,
            no_hooks: cli.no_hooks,
            log_path,
        };
        world::build_world(
            &config_path,
            &repo_dir,
            &cli.rootfs,
            &database_path(project_root, &cli),
            &options,
        )?;
        return Ok(());
    }

    if cli.sync_all || cli.sync.is_some() {
        let mirrors_path = resolve_mirrors_path(project_root, &cli);
        let options = sync::SyncOptions {
            jobs: cli.jobs.unwrap_or_else(sync::default_jobs),
        };
        let repo_dir = cli
            .repo_dir
            .clone()
            .unwrap_or_else(|| project_root.join("repo"));
        let deps_map = default_config_path(project_root);
        if cli.sync_all {
            for mirror in mirrors::load(&mirrors_path)? {
                ui::phase(&ui::pick(
                    format!("同步镜像 {}: {}", mirror.name, mirror.url),
                    format!("Syncing mirror {}: {}", mirror.name, mirror.url),
                ));
                sync::sync_mirror_with_options(&mirror.url, &repo_dir, &deps_map, &options)?;
            }
        } else if let Some(target) = &cli.sync {
            let url = if mirrors::is_direct_location(target) {
                target.clone()
            } else {
                mirrors::resolve_target(&mirrors::load(&mirrors_path)?, target)?
            };
            ui::phase(&ui::pick(
                format!("同步镜像: {url}"),
                format!("Syncing mirror: {url}"),
            ));
            sync::sync_mirror_with_options(&url, &repo_dir, &deps_map, &options)?;
        }
        return Ok(());
    }

    if let Some((key_id, file)) = &cli.repo_sign {
        let signature = sign::sign_detached(key_id, file)?;
        ui::ok(&ui::pick(
            format!("已生成签名: {}", signature.display()),
            format!("Signature created: {}", signature.display()),
        ));
        return Ok(());
    }

    if let Some((repo_dir, package_path)) = &cli.repo_add {
        let index = match &cli.repo_add_key {
            Some(key_id) => repo::repo_add_signed(repo_dir, package_path, key_id)?,
            None => repo::repo_add(repo_dir, package_path)?,
        };
        ui::ok(&ui::pick(
            format!(
                "仓库索引已更新: {} ({} 个包)",
                repo_dir.join("xrpkg-repo.toml").display(),
                index.packages.len()
            ),
            format!(
                "Repository index updated: {} ({} packages)",
                repo_dir.join("xrpkg-repo.toml").display(),
                index.packages.len()
            ),
        ));
        return Ok(());
    }

    if cli.init {
        init_rootfs(&cli.rootfs)?;
        ui::ok(&ui::pick(
            format!("基础 Rootfs 配置已写入: {}", cli.rootfs.display()),
            format!("Base root filesystem initialized: {}", cli.rootfs.display()),
        ));
        return Ok(());
    }
    let cache_dir = cli
        .cache
        .clone()
        .unwrap_or_else(|| project_root.join("var/lib/xrpkg/cache"));
    if cli.clean {
        let config_path = resolve_config_path(&cli)?;
        let db = LocalDb::open(database_path(project_root, &cli))?;
        let report = repo::clean_cache(&config_path, &cache_dir, &db)?;
        ui::phase(ui::t("Cleaning cache", "清理缓存"));
        ui::ok(&ui::pick(
            format!(
                "缓存清理完成: 删除 {} 个包、{} 个签名，释放 {}",
                report.packages,
                report.signatures,
                format_bytes(report.bytes)
            ),
            format!(
                "Cache cleaned: {} packages, {} signatures, {} freed",
                report.packages,
                report.signatures,
                format_bytes(report.bytes)
            ),
        ));
        return Ok(());
    }
    if cli.update {
        let config_path = resolve_config_path(&cli)?;
        repo::update(&config_path, &cache_dir)?;
        return Ok(());
    }
    if let Some(keyword) = &cli.search {
        let config_path = resolve_config_path(&cli)?;
        let hits = repo::search(&config_path, &cache_dir, keyword)?;
        if hits.is_empty() {
            ui::note(&ui::pick(
                format!("没有找到包含 `{keyword}` 的包"),
                format!("No packages matching `{keyword}`"),
            ));
        } else {
            ui::phase(&ui::pick(
                format!("搜索 `{keyword}`: {} 个结果", hits.len()),
                format!("Search `{keyword}`: {} result(s)", hits.len()),
            ));
            let rows = hits
                .iter()
                .map(|hit| [hit.repo.clone(), hit.name.clone(), hit.version.clone()])
                .collect::<Vec<_>>();
            ui::table(
                [
                    ui::t("Repository", "仓库"),
                    ui::t("Package", "包"),
                    ui::t("Version", "版本"),
                ],
                &rows,
            );
        }
        return Ok(());
    }
    if let Some(package_name) = &cli.install {
        let requested_path = Path::new(package_name);
        if requested_path.is_file() {
            let packages = load_repository(project_root.join("packages.toml"))?;
            let mut db = LocalDb::open(database_path(project_root, &cli))?;
            let installed = db.list_installed()?;
            let mapper = DependencyMapper::load_or_create(default_config_path(project_root))?;
            if package_name.ends_with(".xpkg") {
                install_local_xpkg(
                    &mut db,
                    requested_path,
                    &cli.rootfs,
                    &packages,
                    &installed,
                    cli.no_hooks,
                    cli.no_deps,
                    cli.allow_downgrade,
                )?;
            } else if package_name.ends_with(".deb") {
                install_deb(
                    &mut db,
                    requested_path,
                    &cli.rootfs,
                    &packages,
                    &installed,
                    &mapper,
                    cli.no_hooks,
                )?;
            } else if package_name.ends_with(".rpm") {
                install_rpm(
                    &mut db,
                    requested_path,
                    &cli.rootfs,
                    &packages,
                    &installed,
                    &mapper,
                    cli.no_hooks,
                )?;
            } else if package_name.ends_with(".pkg.tar.zst") {
                install_arch(
                    &mut db,
                    requested_path,
                    &cli.rootfs,
                    &packages,
                    &installed,
                    &mapper,
                    cli.no_hooks,
                    cli.no_deps,
                )?;
            } else {
                return Err(ui::pick(
                    format!(
                        "无法识别外部包 `{package_name}`；支持 .xpkg、.deb、.rpm、.pkg.tar.zst"
                    ),
                    format!(
                        "Unrecognized package `{package_name}`; expected .xpkg, .deb, .rpm, or .pkg.tar.zst"
                    ),
                ));
            }
            return Ok(());
        }

        let config_path = resolve_config_path(&cli)?;
        let mut db = LocalDb::open(database_path(project_root, &cli))?;
        let (requested, requested_version) = parse_install_request(package_name)?;
        let mirrors_path = resolve_mirrors_path(project_root, &cli);
        let outcome = repo::install_from_repositories_with_mirrors(
            &mut db,
            &config_path,
            &cache_dir,
            &requested,
            requested_version.as_ref(),
            cli.allow_downgrade,
            &cli.rootfs,
            cli.no_hooks,
            Some(&mirrors_path),
            cli.prefer_arch,
        )?;
        if let repo::RepositoryInstall::Arch(download) = outcome {
            let packages = load_repository(project_root.join("packages.toml"))?;
            let installed = db.list_installed()?;
            let mapper = DependencyMapper::load_or_create(default_config_path(project_root))?;
            ui::phase(&ui::pick(
                format!(
                    "Arch 兼容包: {} {} ({})",
                    download.name,
                    download.version,
                    download.path.display()
                ),
                format!(
                    "Arch compatibility package: {} {} ({})",
                    download.name,
                    download.version,
                    download.path.display()
                ),
            ));
            install_arch(
                &mut db,
                &download.path,
                &cli.rootfs,
                &packages,
                &installed,
                &mapper,
                cli.no_hooks,
                cli.no_deps,
            )?;
        }
        return Ok(());
    }

    if let Some(package_name) = &cli.remove {
        let mut db = LocalDb::open(database_path(project_root, &cli))?;
        let rootfs = Rootfs::prepare(&cli.rootfs)?;
        let report = uninstall::remove_package(&mut db, &rootfs, package_name, cli.no_hooks)?;
        ui::phase(&ui::pick(
            format!("卸载 {package_name}"),
            format!("Removing {package_name}"),
        ));
        ui::ok(&ui::pick(
            format!("卸载完成: {package_name}"),
            format!("Removal complete: {package_name}"),
        ));
        ui::note(&ui::pick(
            format!("删除文件: {}", report.removed_files),
            format!("Files removed: {}", report.removed_files),
        ));
        ui::note(&ui::pick(
            format!("共享文件保留: {}", report.skipped_shared),
            format!("Shared files kept: {}", report.skipped_shared),
        ));
        ui::note(&ui::pick(
            format!("已不存在: {}", report.missing),
            format!("Already absent: {}", report.missing),
        ));
        ui::note(&ui::pick(
            format!("删除空目录: {}", report.removed_dirs),
            format!("Empty directories removed: {}", report.removed_dirs),
        ));
        return Ok(());
    }

    if let Some(script_path) = &cli.build {
        build_and_install(
            script_path,
            &cli.rootfs,
            project_root,
            &database_path(project_root, &cli),
            cli.no_hooks,
            cli.allow_downgrade,
            !cli.no_sandbox,
            cli.jobs,
        )?;
        return Ok(());
    }

    let packages = load_repository(project_root.join("packages.toml"))?;
    let mut db = LocalDb::open(database_path(project_root, &cli))?;

    if foreign_count > 0 {
        let installed = db.list_installed()?;
        let mapper = DependencyMapper::load_or_create(default_config_path(project_root))?;
        if let Some(deb_path) = &cli.deb {
            install_deb(
                &mut db,
                deb_path,
                &cli.rootfs,
                &packages,
                &installed,
                &mapper,
                cli.no_hooks,
            )?;
        } else if let Some(rpm_path) = &cli.rpm {
            install_rpm(
                &mut db,
                rpm_path,
                &cli.rootfs,
                &packages,
                &installed,
                &mapper,
                cli.no_hooks,
            )?;
        } else if let Some(arch_path) = &cli.arch {
            install_arch(
                &mut db,
                arch_path,
                &cli.rootfs,
                &packages,
                &installed,
                &mapper,
                cli.no_hooks,
                cli.no_deps,
            )?;
        }
        return Ok(());
    }

    Ok(())
}

/// 解析子命令和可选的 `--root <dir>`、`--config <file>`、`--build <recipe>` 等。
fn parse_cli() -> Result<Cli, String> {
    let mut rootfs = PathBuf::from(DEFAULT_ROOTFS);
    let mut deb = None;
    let mut rpm = None;
    let mut arch = None;
    let mut build = None;
    let mut remove = None;
    let mut repo_add = None;
    let mut repo_add_key = None;
    let mut repo_sign = None;
    let mut update = false;
    let mut clean = false;
    let mut search = None;
    let mut install = None;
    let mut sync = None;
    let mut sync_all = false;
    let mut prefer_arch = false;
    let mut mirrors = None;
    let mut repo_dir = None;
    let mut config = None;
    let mut no_hooks = false;
    let mut db = None;
    let mut cache = None;
    let mut allow_downgrade = false;
    let mut no_sandbox = false;
    let mut no_deps = false;
    let mut jobs = None;
    let mut init = false;
    let mut build_world = false;
    let mut list = false;
    let mut help = false;
    let mut version = false;
    let mut arguments = std::env::args().skip(1).collect::<Vec<_>>();

    if !arguments.is_empty() {
        let first = arguments.remove(0);
        match first.as_str() {
            "repo-add" => {
                if arguments.len() < 2 {
                    return Err(ui::pick(
                        "用法: xrpkg repo-add <repo_dir> <pkg.xpkg>",
                        "Usage: xrpkg repo-add <repo_dir> <pkg.xpkg>",
                    ));
                }
                let repo_dir = PathBuf::from(arguments.remove(0));
                let package = PathBuf::from(arguments.remove(0));
                repo_add = Some((repo_dir, package));
            }
            "repo-sign" => {
                if arguments.len() < 2 {
                    return Err(ui::pick(
                        "用法: xrpkg repo-sign <key_id> <file>",
                        "Usage: xrpkg repo-sign <key_id> <file>",
                    ));
                }
                let key_id = arguments.remove(0);
                let file = PathBuf::from(arguments.remove(0));
                repo_sign = Some((key_id, file));
            }
            "update" => update = true,
            "clean" => clean = true,
            "init" => init = true,
            "build-world" => build_world = true,
            "list" => list = true,
            "help" | "--help" | "-h" => help = true,
            "version" | "--version" | "-V" => version = true,
            "search" => {
                if arguments.is_empty() {
                    return Err(ui::pick(
                        "用法: xrpkg search <keyword>",
                        "Usage: xrpkg search <keyword>",
                    ));
                }
                search = Some(arguments.remove(0));
            }
            "install" => {
                if arguments.is_empty() {
                    return Err(ui::pick(
                        "用法: xrpkg install <pkg_name>",
                        "Usage: xrpkg install <package>",
                    ));
                }
                install = Some(arguments.remove(0));
            }
            "sync" => {
                if arguments.is_empty() {
                    return Err(ui::pick(
                        "用法: xrpkg sync <url|mirror_name> | xrpkg sync --all",
                        "Usage: xrpkg sync <url|mirror_name> | xrpkg sync --all",
                    ));
                }
                let value = arguments.remove(0);
                if value.starts_with("--") {
                    // 让全局参数解析器处理 --all / --jobs 等；顺序可自由。
                    arguments.insert(0, value);
                } else {
                    sync = Some(value);
                }
            }
            _ => arguments.insert(0, first),
        }
    }
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--root" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| ui::pick("`--root` 需要一个目录参数", "`--root` requires a directory"))?;
                rootfs = PathBuf::from(value);
            }
            "--config" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| ui::pick("`--config` 需要一个 repos.toml 路径", "`--config` requires a repos.toml path"))?;
                config = Some(PathBuf::from(value));
            }
            "--mirrors" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| ui::pick("`--mirrors` 需要一个 mirrors.toml 路径", "`--mirrors` requires a mirrors.toml path"))?;
                mirrors = Some(PathBuf::from(value));
            }
            "--repo-dir" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| ui::pick("`--repo-dir` 需要一个仓库目录", "`--repo-dir` requires a repository directory"))?;
                repo_dir = Some(PathBuf::from(value));
            }
            "--no-hooks" => no_hooks = true,
            "--allow-downgrade" => allow_downgrade = true,
            "--prefer-arch" => prefer_arch = true,
            "--all" => sync_all = true,
            "--no-sandbox" => no_sandbox = true,
            "--no-deps" => no_deps = true,
            "--help" | "-h" => help = true,
            "--version" | "-V" => version = true,
            "--jobs" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| ui::pick("`--jobs` 需要一个正整数", "`--jobs` requires a positive integer"))?;
                let parsed = value
                    .parse::<usize>()
                    .map_err(|_| ui::pick(format!("`--jobs` 的值 `{value}` 不是正整数"), format!("`--jobs` value `{value}` is not a positive integer")))?;
                if parsed == 0 {
                    return Err(ui::pick("`--jobs` 必须大于 0", "`--jobs` must be greater than 0"));
                }
                jobs = Some(parsed);
            }
            "--key" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| ui::pick("`--key` 需要一个 GPG key id", "`--key` requires a GPG key id"))?;
                repo_add_key = Some(value);
            }
            "--db" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| ui::pick("`--db` 需要一个 SQLite 路径", "`--db` requires a SQLite path"))?;
                db = Some(PathBuf::from(value));
            }
            "--cache" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| ui::pick("`--cache` 需要一个目录", "`--cache` requires a directory"))?;
                cache = Some(PathBuf::from(value));
            }
            "--deb" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| ui::pick("`--deb` 需要一个 .deb 文件路径", "`--deb` requires a .deb file path"))?;
                deb = Some(PathBuf::from(value));
            }
            "--rpm" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| ui::pick("`--rpm` 需要一个 .rpm 文件路径", "`--rpm` requires a .rpm file path"))?;
                rpm = Some(PathBuf::from(value));
            }
            "--arch" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| ui::pick("`--arch` 需要一个 .pkg.tar.zst 文件路径", "`--arch` requires a .pkg.tar.zst file path"))?;
                arch = Some(PathBuf::from(value));
            }
            "--build" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| ui::pick("`--build` 需要一个构建脚本路径", "`--build` requires a recipe path"))?;
                build = Some(PathBuf::from(value));
            }
            "--remove" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| ui::pick("`--remove` 需要一个包名", "`--remove` requires a package name"))?;
                remove = Some(value);
            }
            _ => {
                return Err(ui::pick(
                    format!(
                        "未知参数 `{argument}`；支持 init/install/update/clean/search/list/sync/repo-add/repo-sign/build-world 子命令，或 `--root <dir>`、`--build <recipe>`、`--remove <pkg>`、`--deb <file>`、`--rpm <file>`、`--arch <file>`、`--config <file>`、`--mirrors <file>`、`--repo-dir <dir>`、`--cache <dir>`、`--key <gpg-key>`、`--allow-downgrade`、`--prefer-arch`、`--jobs <n>`、`--no-sandbox`、`--no-deps` 和 `--no-hooks`"
                    ),
                    format!(
                        "Unknown argument `{argument}`; use init/install/update/clean/search/list/sync/repo-add/repo-sign/build-world, or --root/--build/--remove/--deb/--rpm/--arch/--config/--mirrors/--repo-dir/--cache/--key/--allow-downgrade/--prefer-arch/--jobs/--no-sandbox/--no-deps/--no-hooks"
                    ),
                ))
            }
        }
    }

    Ok(Cli {
        rootfs,
        deb,
        rpm,
        arch,
        build,
        remove,
        repo_add,
        repo_add_key,
        repo_sign,
        update,
        clean,
        search,
        install,
        sync,
        sync_all,
        prefer_arch,
        mirrors,
        repo_dir,
        config,
        no_hooks,
        db,
        cache,
        allow_downgrade,
        no_sandbox,
        no_deps,
        jobs,
        init,
        build_world,
        list,
        help,
        version,
    })
}

#[allow(clippy::too_many_arguments)]
/// 构建源码包并自动安装到 native 数据库和 Rootfs。
fn build_and_install(
    script_path: &Path,
    rootfs_path: &Path,
    project_root: &Path,
    db_path: &Path,
    no_hooks: bool,
    allow_downgrade: bool,
    sandbox: bool,
    jobs: Option<usize>,
) -> Result<(), String> {
    let output_dir = project_root.join("repo");
    let archive = builder::build_package_with_options(
        script_path,
        &output_dir,
        builder::BuildOptions { sandbox, jobs },
    )?;
    ui::ok(&ui::pick(
        format!("生成 .xpkg: {}", archive.display()),
        format!("Created .xpkg: {}", archive.display()),
    ));

    let metadata = xpkg::read_metadata(&archive)?;
    let mut packages = load_repository(project_root.join("packages.toml"))?;
    packages
        .retain(|package| !(package.name == metadata.name && package.version == metadata.version));
    packages.push(Package::new(
        metadata.name.clone(),
        metadata.version.clone(),
        metadata.requires.clone(),
    ));

    let mut db = LocalDb::open(db_path)?;
    install(
        &mut db,
        &packages,
        &metadata.name,
        &output_dir,
        rootfs_path,
        no_hooks,
        allow_downgrade,
    )?;
    Ok(())
}

/// 将外部包的文件和 hooks 写入 native SQLite 数据库。
fn commit_external_package(
    db: &mut LocalDb,
    name: &str,
    version_raw: &str,
    origin: &str,
    files: &[PathBuf],
    hooks: &[HookScript],
) -> Result<(), String> {
    let install = PackageInstall {
        package: SelectedPackage {
            name: name.to_string(),
            version: Version::parse("0").expect("0 必定是合法版本"),
        },
        version_raw: version_raw.to_string(),
        origin: origin.to_string(),
        files: files
            .iter()
            .map(|file| file.to_string_lossy().replace('\\', "/"))
            .collect(),
        hooks: hooks.to_vec(),
    };
    db.commit(&[install])
}

/// 外部包升级时需要保留的旧状态。
struct ExternalUpgradeData {
    old_files: Vec<String>,
    old_hooks: Vec<HookScript>,
    old_version_raw: String,
}

enum ExternalUpgrade {
    Fresh,
    AlreadyInstalled,
    Upgrade(ExternalUpgradeData),
}

/// 查询外部包是否是升级；不执行任何 hook 或文件操作。
fn find_external_upgrade(
    db: &LocalDb,
    package_name: &str,
    new_version_raw: &str,
) -> Result<ExternalUpgrade, String> {
    let Some(old) = db
        .list_installed()?
        .into_iter()
        .find(|package| package.name == package_name)
    else {
        return Ok(ExternalUpgrade::Fresh);
    };
    if old.version_raw == new_version_raw {
        return Ok(ExternalUpgrade::AlreadyInstalled);
    }
    Ok(ExternalUpgrade::Upgrade(ExternalUpgradeData {
        old_files: db.list_files(package_name)?,
        old_hooks: db.list_hooks(package_name)?,
        old_version_raw: old.version_raw,
    }))
}

/// 删除外部包旧版本有、新版本没有且未被共享的文件；返回提交后要清理的目录。
fn remove_external_obsolete(
    db: &LocalDb,
    transaction: &mut Transaction,
    package_name: &str,
    old_files: &[String],
    new_files: &[PathBuf],
) -> Result<std::collections::BTreeSet<PathBuf>, String> {
    let new_files = new_files
        .iter()
        .map(|file| file.to_string_lossy().replace('\\', "/"))
        .collect::<std::collections::BTreeSet<_>>();
    let mut removed = Vec::new();
    for file in old_files {
        if new_files.contains(file) || db.file_claimed_by_other(package_name, file)? {
            continue;
        }
        let relative = xpkg::sanitize_relative(Path::new(file))
            .map_err(|error| format!("旧文件路径 `{file}` 非法: {error}"))?;
        transaction.remove_file(&relative)?;
        removed.push(relative);
    }
    Ok(transaction::parent_directories(removed))
}

/// 外部兼容包的统一提交阶段：事务化发布、升级快照、hook 和数据库提交。
#[allow(clippy::too_many_arguments)]
fn finish_external_install(
    db: &mut LocalDb,
    target: &Rootfs,
    staging: &Rootfs,
    name: &str,
    version_raw: &str,
    origin: &str,
    files: &[PathBuf],
    hooks: &[HookScript],
    old: Option<ExternalUpgradeData>,
    no_hooks: bool,
) -> Result<(), String> {
    let mut transaction = Transaction::begin(target, Some(db))?;
    transaction.record_install(name, version_raw, origin)?;

    let result = (|| -> Result<std::collections::BTreeSet<PathBuf>, String> {
        if !no_hooks {
            if let Some(old) = &old {
                run_external_hooks(
                    &old.old_hooks,
                    PRE_REMOVE,
                    name,
                    &old.old_version_raw,
                    target,
                )?;
            }
            run_external_hooks(hooks, PRE_INSTALL, name, version_raw, target)?;
        }

        let publish_paths = files
            .iter()
            .map(|file| file.to_string_lossy().replace('\\', "/"))
            .collect::<Vec<_>>();
        xpkg::publish_files_txn(&mut transaction, staging, &publish_paths)?;

        let directories = if let Some(old) = &old {
            remove_external_obsolete(db, &mut transaction, name, &old.old_files, files)?
        } else {
            std::collections::BTreeSet::new()
        };

        if !no_hooks {
            run_external_hooks(hooks, POST_INSTALL, name, version_raw, target)?;
        }

        commit_external_package(db, name, version_raw, origin, files, hooks)?;
        Ok(directories)
    })();

    match result {
        Ok(directories) => {
            transaction.commit();
            let _ = transaction::remove_empty_directories(target.path(), &directories);
            Ok(())
        }
        Err(error) => {
            let rollback = transaction.rollback();
            Err(match rollback {
                Ok(()) => format!("{error}; 已回滚文件变更"),
                Err(rollback_error) => format!("{error}; 回滚失败: {rollback_error}"),
            })
        }
    }
}

/// 运行外部兼容包的某个 hook 阶段。
fn run_external_hooks(
    hooks: &[HookScript],
    phase: &str,
    package: &str,
    version: &str,
    rootfs: &Rootfs,
) -> Result<(), String> {
    for hook in hooks {
        if hook.phase == phase {
            hooks::run_hook(phase, package, version, rootfs, &hook.script)?;
        }
    }
    Ok(())
}

/// 打印映射后的依赖并交给 resolvo 做可满足性预检。
fn preflight_requirements(
    packages: &[Package],
    installed: &[InstalledPackage],
    requirements: &[Capability],
) -> Result<(), String> {
    ui::phase(ui::t("Dependency preflight", "依赖预检"));
    if requirements.is_empty() {
        ui::note(ui::t("(none)", "(无)"));
    } else {
        for requirement in requirements {
            ui::note(&requirement.to_string());
        }
    }

    resolvo_solver::resolve_requirements(packages, requirements, installed)?;
    ui::ok(ui::t("Dependency preflight passed", "依赖预检通过"));
    Ok(())
}

/// 兼容轨道：解析 .pkg.tar.zst、暂存提取、ABI 校验后发布到目标 Rootfs。
#[allow(clippy::too_many_arguments)]
fn install_arch(
    db: &mut LocalDb,
    arch_path: &Path,
    rootfs_path: &Path,
    packages: &[Package],
    installed: &[InstalledPackage],
    mapper: &DependencyMapper,
    no_hooks: bool,
    no_deps: bool,
) -> Result<(), String> {
    let metadata = compat_arch::parse_arch_metadata(arch_path)?;
    ui::phase(&ui::pick(
        format!("Arch 包: {} {}", metadata.pkgname, metadata.pkgver),
        format!("Arch package: {} {}", metadata.pkgname, metadata.pkgver),
    ));
    if metadata.depends.is_empty() {
        ui::note(ui::t("Dependencies: (none)", "依赖: (无)"));
    } else {
        ui::note(ui::t("Dependencies:", "依赖:"));
        for dependency in &metadata.depends {
            ui::note(dependency);
        }
    }

    if no_deps {
        ui::note(ui::t(
            "Dependency preflight: skipped (--no-deps)",
            "依赖预检: 跳过（--no-deps）",
        ));
    } else {
        let requirements = metadata.mapped_requirements(mapper)?;
        preflight_requirements(packages, installed, &requirements)?;
    }

    let hooks = compat_arch::read_arch_hooks(arch_path)?;
    let target = Rootfs::prepare(rootfs_path)?;
    let old = match find_external_upgrade(db, &metadata.pkgname, &metadata.pkgver)? {
        ExternalUpgrade::Fresh => None,
        ExternalUpgrade::AlreadyInstalled => {
            ui::note(&ui::pick(
                format!("{} 已经安装，无需操作", metadata.pkgname),
                format!("{} is already installed", metadata.pkgname),
            ));
            return Ok(());
        }
        ExternalUpgrade::Upgrade(data) => Some(data),
    };

    let staging_dir = StagingDir::create()?;
    let staging = Rootfs::prepare(staging_dir.path())?;
    let files = compat_arch::extract_arch_data(arch_path, staging.path())?;

    let abi_guard = AbiGuard::new(MAX_GLIBC_VERSION)?;
    for file in &files {
        let staged = staging.path().join(file);
        let staged_metadata = fs::symlink_metadata(&staged)
            .map_err(|error| format!("检查暂存文件 `{}` 失败: {error}", staged.display()))?;
        // 符号链接本身不是 ELF；其目标若是 ELF，已作为普通文件单独检查。
        if staged_metadata.file_type().is_symlink() {
            continue;
        }
        let relative = file.to_string_lossy().replace('\\', "/");
        abi_guard.check_file(&metadata.pkgname, &relative, &staged)?;
    }

    finish_external_install(
        db,
        &target,
        &staging,
        &metadata.pkgname,
        &metadata.pkgver,
        "arch",
        &files,
        &hooks,
        old,
        no_hooks,
    )?;

    ui::ok(&ui::pick(
        format!(
            "ABI 校验通过（GLIBC 基线 {MAX_GLIBC_VERSION}），发布到 {}",
            target.path().display()
        ),
        format!(
            "ABI check passed (GLIBC baseline {MAX_GLIBC_VERSION}); published to {}",
            target.path().display()
        ),
    ));
    let file_count = ui::count(files.len(), "file", "files", "个文件");
    ui::note(&ui::pick(
        format!("安装文件 ({file_count})"),
        format!("Files installed: {file_count}"),
    ));
    ui::ok(ui::t(
        "Compatibility track: recorded in installed_packages/installed_files/installed_hooks; origin [arch]",
        "兼容轨道：已写入 installed_packages/installed_files 和 installed_hooks；来源 [arch]",
    ));
    Ok(())
}

/// 兼容轨道：解析 .rpm、暂存提取、ABI 校验后发布到目标 Rootfs。
///
/// 本阶段不把 RPM epoch/version/release 和依赖映射进 native 数据库。
fn install_rpm(
    db: &mut LocalDb,
    rpm_path: &Path,
    rootfs_path: &Path,
    packages: &[Package],
    installed: &[InstalledPackage],
    mapper: &DependencyMapper,
    no_hooks: bool,
) -> Result<(), String> {
    let metadata = compat_rpm::parse_rpm_metadata(rpm_path)?;
    let epoch = metadata
        .epoch
        .map(|epoch| format!("{epoch}:"))
        .unwrap_or_default();
    ui::phase(&ui::pick(
        format!(
            "RPM 包: {} {}{}-{}",
            metadata.name, epoch, metadata.version, metadata.release
        ),
        format!(
            "RPM package: {} {}{}-{}",
            metadata.name, epoch, metadata.version, metadata.release
        ),
    ));
    if metadata.requires.is_empty() {
        ui::note(ui::t("Requires: (none)", "Requires: (无)"));
    } else {
        ui::note(ui::t("Requires:", "Requires:"));
        for requirement in &metadata.requires {
            ui::note(requirement);
        }
    }

    let requirements = metadata.mapped_requirements(mapper)?;
    preflight_requirements(packages, installed, &requirements)?;

    let staging_dir = StagingDir::create()?;
    let staging = Rootfs::prepare(staging_dir.path())?;
    let files = compat_rpm::extract_rpm_data(rpm_path, staging.path())?;

    let abi_guard = AbiGuard::new(MAX_GLIBC_VERSION)?;
    for file in &files {
        let relative = file.to_string_lossy().replace('\\', "/");
        abi_guard.check_file(&metadata.name, &relative, &staging.path().join(file))?;
    }

    let hooks = compat_rpm::read_rpm_hooks(rpm_path)?;
    let target = Rootfs::prepare(rootfs_path)?;
    let hook_version = format!("{}{}-{}", epoch, metadata.version, metadata.release);
    let old = match find_external_upgrade(db, &metadata.name, &hook_version)? {
        ExternalUpgrade::Fresh => None,
        ExternalUpgrade::AlreadyInstalled => {
            ui::note(&ui::pick(
                format!("{} 已经安装，无需操作", metadata.name),
                format!("{} is already installed", metadata.name),
            ));
            return Ok(());
        }
        ExternalUpgrade::Upgrade(data) => Some(data),
    };

    finish_external_install(
        db,
        &target,
        &staging,
        &metadata.name,
        &hook_version,
        "rpm",
        &files,
        &hooks,
        old,
        no_hooks,
    )?;

    ui::ok(&ui::pick(
        format!(
            "ABI 校验通过（GLIBC 基线 {MAX_GLIBC_VERSION}），发布到 {}",
            target.path().display()
        ),
        format!(
            "ABI check passed (GLIBC baseline {MAX_GLIBC_VERSION}); published to {}",
            target.path().display()
        ),
    ));
    let file_count = ui::count(files.len(), "file", "files", "个文件");
    ui::note(&ui::pick(
        format!("安装文件 ({file_count})"),
        format!("Files installed: {file_count}"),
    ));
    ui::ok(ui::t(
        "Compatibility track: recorded in installed_packages/installed_files/installed_hooks; origin [arch]",
        "兼容轨道：已写入 installed_packages/installed_files 和 installed_hooks；来源 [arch]",
    ));
    Ok(())
}

/// 兼容轨道：解析 .deb、暂存提取、ABI 校验后发布到目标 Rootfs。
///
/// 本阶段不把 Debian 版本和复杂依赖映射进 native 数据库，因此不写
/// installed_packages/installed_files；只完成元数据解析、文件落盘和 ABI 拦截。
fn install_deb(
    db: &mut LocalDb,
    deb_path: &Path,
    rootfs_path: &Path,
    packages: &[Package],
    installed: &[InstalledPackage],
    mapper: &DependencyMapper,
    no_hooks: bool,
) -> Result<(), String> {
    let metadata = compat_deb::parse_deb_metadata(deb_path)?;
    ui::phase(&ui::pick(
        format!("Debian 包: {} {}", metadata.package, metadata.version),
        format!("Debian package: {} {}", metadata.package, metadata.version),
    ));
    ui::note(&ui::pick(
        format!("Depends: {}", metadata.depends.as_deref().unwrap_or("(无)")),
        format!(
            "Depends: {}",
            metadata.depends.as_deref().unwrap_or("(none)")
        ),
    ));

    let requirements = metadata.mapped_requirements(mapper)?;
    preflight_requirements(packages, installed, &requirements)?;

    let staging_dir = StagingDir::create()?;
    let staging = Rootfs::prepare(staging_dir.path())?;
    let files = compat_deb::extract_deb_data(deb_path, staging.path())?;

    let abi_guard = AbiGuard::new(MAX_GLIBC_VERSION)?;
    for file in &files {
        let relative = file.to_string_lossy().replace('\\', "/");
        abi_guard.check_file(&metadata.package, &relative, &staging.path().join(file))?;
    }

    let hooks = compat_deb::read_deb_hooks(deb_path)?;
    let target = Rootfs::prepare(rootfs_path)?;
    let old = match find_external_upgrade(db, &metadata.package, &metadata.version)? {
        ExternalUpgrade::Fresh => None,
        ExternalUpgrade::AlreadyInstalled => {
            ui::note(&ui::pick(
                format!("{} 已经安装，无需操作", metadata.package),
                format!("{} is already installed", metadata.package),
            ));
            return Ok(());
        }
        ExternalUpgrade::Upgrade(data) => Some(data),
    };

    finish_external_install(
        db,
        &target,
        &staging,
        &metadata.package,
        &metadata.version,
        "deb",
        &files,
        &hooks,
        old,
        no_hooks,
    )?;

    ui::ok(&ui::pick(
        format!(
            "ABI 校验通过（GLIBC 基线 {MAX_GLIBC_VERSION}），发布到 {}",
            target.path().display()
        ),
        format!(
            "ABI check passed (GLIBC baseline {MAX_GLIBC_VERSION}); published to {}",
            target.path().display()
        ),
    ));
    let file_count = ui::count(files.len(), "file", "files", "个文件");
    ui::note(&ui::pick(
        format!("安装文件 ({file_count})"),
        format!("Files installed: {file_count}"),
    ));
    ui::ok(ui::t(
        "Compatibility track: recorded in installed_packages/installed_files/installed_hooks; origin [arch]",
        "兼容轨道：已写入 installed_packages/installed_files 和 installed_hooks；来源 [arch]",
    ));
    Ok(())
}

/// 直接安装本地 `.xpkg`：校验元数据、预检依赖，再走统一的 ABI 与事务提交。
#[allow(clippy::too_many_arguments)]
fn install_local_xpkg(
    db: &mut LocalDb,
    archive: &Path,
    rootfs_path: &Path,
    packages: &[Package],
    installed: &[InstalledPackage],
    no_hooks: bool,
    no_deps: bool,
    allow_downgrade: bool,
) -> Result<(), String> {
    let metadata = xpkg::read_metadata(archive)?;
    ui::phase(&ui::pick(
        format!(
            "本地 .xpkg: {} {} ({})",
            metadata.name,
            metadata.version,
            archive.display()
        ),
        format!(
            "Local .xpkg: {} {} ({})",
            metadata.name,
            metadata.version,
            archive.display()
        ),
    ));

    if installed.iter().any(|package| {
        package.name == metadata.name && package.version_raw == metadata.version.to_string()
    }) {
        ui::note(&ui::pick(
            format!("{} 已经安装，无需操作", metadata.name),
            format!("{} is already installed", metadata.name),
        ));
        return Ok(());
    }

    if no_deps {
        ui::note(ui::t(
            "Dependency preflight: skipped (--no-deps)",
            "依赖预检: 跳过（--no-deps）",
        ));
    } else {
        preflight_requirements(packages, installed, &metadata.requires)?;
    }

    let selected = SelectedPackage {
        name: metadata.name.clone(),
        version: metadata.version.clone(),
    };
    let mut archives = HashMap::new();
    archives.insert(
        (selected.name.clone(), selected.version.clone()),
        archive.to_path_buf(),
    );
    installer::install_changes(
        db,
        &[selected],
        &archives,
        rootfs_path,
        no_hooks,
        allow_downgrade,
    )
}

/// 模拟 `install <pkg_name>`：求解增量计划，然后进入带 ABI 守门的提交阶段。
fn install(
    db: &mut LocalDb,
    packages: &[Package],
    requested: &str,
    repo_dir: &Path,
    rootfs_path: &Path,
    no_hooks: bool,
    allow_downgrade: bool,
) -> Result<(), String> {
    let installed = db.list_installed()?;
    let resolution = resolvo_solver::resolve_install_latest(packages, requested, &installed)?;
    let changes = incremental_plan(&resolution, &installed);
    if changes.is_empty() {
        ui::note(&ui::pick(
            format!("{requested} 已经安装，无需操作"),
            format!("{requested} is already installed"),
        ));
        return Ok(());
    }

    ui::phase(ui::t("Install plan", "安装计划"));
    let rows = changes
        .iter()
        .map(|package| [package.name.clone(), package.version.to_string()])
        .collect::<Vec<_>>();
    ui::table2([ui::t("Package", "包"), ui::t("Version", "版本")], &rows);

    let mut archives = HashMap::new();
    for package in &changes {
        let archive = package_archive_path(repo_dir, package)?;
        archives.insert((package.name.clone(), package.version.clone()), archive);
    }
    installer::install_changes(
        db,
        &changes,
        &archives,
        rootfs_path,
        no_hooks,
        allow_downgrade,
    )
}

/// 临时暂存目录；离开作用域时自动清理。
struct StagingDir {
    path: PathBuf,
}

impl StagingDir {
    /// 在系统临时目录下创建唯一暂存目录。
    fn create() -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("读取系统时间失败: {error}"))?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("xrpkg-stage-{}-{nonce}", std::process::id()));
        fs::create_dir(&path)
            .map_err(|error| format!("创建暂存目录 `{}` 失败: {error}", path.display()))?;
        Ok(Self { path })
    }

    /// 返回暂存目录路径。
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagingDir {
    /// 删除暂存目录；ABI 校验失败时这就是安装回滚。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// 根据计划中的包名和版本定位 repo/<name>-<version>.xpkg。
fn package_archive_path(repo_dir: &Path, package: &SelectedPackage) -> Result<PathBuf, String> {
    if package.name.is_empty() || package.name.contains('/') || package.name.contains('\\') {
        return Err(format!("包名 `{}` 不能包含路径分隔符", package.name));
    }

    let archive = repo_dir.join(format!("{}-{}.xpkg", package.name, package.version));
    if !archive.is_file() {
        return Err(format!("找不到包文件 `{}`", archive.display()));
    }
    Ok(archive)
}

/// 模拟 `list`：按包名排序列出数据库中的已安装包。
fn list(db: &LocalDb) -> Result<(), String> {
    let installed = db.list_installed()?;
    ui::phase(&ui::pick(
        format!("已安装包: {}", installed.len()),
        format!("Installed packages: {}", installed.len()),
    ));
    let rows = installed
        .iter()
        .map(|package| {
            [
                package.name.clone(),
                package.version_raw.clone(),
                format!("[{}]", package.origin),
            ]
        })
        .collect::<Vec<_>>();
    ui::table(
        [
            ui::t("Name", "名称"),
            ui::t("Version", "版本"),
            ui::t("Origin", "来源"),
        ],
        &rows,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use tar::{Builder, Header};
    use tempfile::TempDir;

    use super::*;
    use xrpkg::capability::Version;

    const BASE_ELF: &[u8] = include_bytes!("../tests/fixtures/libevil.so");

    fn app_xpkg(path: &Path, glibc_version: &str) {
        let mut elf = BASE_ELF.to_vec();
        let old = b"GLIBC_2.00";
        let new = format!("GLIBC_{glibc_version}");
        let position = elf
            .windows(old.len())
            .position(|window| window == old)
            .unwrap();
        elf[position..position + new.len()].copy_from_slice(new.as_bytes());

        let metadata = "name = \"app\"\nversion = \"1.0\"\nrequires = []\n";
        let file = File::create(path).unwrap();
        let encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
        let mut builder = Builder::new(encoder);

        let mut header = Header::new_gnu();
        header.set_size(metadata.len() as u64);
        header.set_mode(0o644);
        builder
            .append_data(&mut header, "metadata.toml", metadata.as_bytes())
            .unwrap();

        let mut header = Header::new_gnu();
        header.set_size(elf.len() as u64);
        header.set_mode(0o644);
        builder
            .append_data(&mut header, "files/lib/libevil.so", &elf[..])
            .unwrap();

        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
    }

    #[test]
    fn abi_failure_leaves_rootfs_and_database_untouched() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        app_xpkg(&repo.join("app-1.0.xpkg"), "2.45");

        let mut db = LocalDb::open(temp.path().join("local.db")).unwrap();
        let rootfs = temp.path().join("rootfs");
        let changes = vec![SelectedPackage {
            name: "app".to_string(),
            version: Version::parse("1.0").unwrap(),
        }];

        let mut archives = HashMap::new();
        archives.insert(
            ("app".to_string(), Version::parse("1.0").unwrap()),
            repo.join("app-1.0.xpkg"),
        );
        let error = installer::install_changes(&mut db, &changes, &archives, &rootfs, false, false)
            .expect_err("GLIBC_2.45 必须被 ABI 守门员拒绝");

        assert!(error.contains("GLIBC_2.45"), "{error}");
        assert!(db.list_installed().unwrap().is_empty());
        assert!(db.list_files("app").unwrap().is_empty());
        assert!(!rootfs.exists(), "ABI 失败时不应创建目标 Rootfs");
    }

    #[test]
    fn abi_pass_publishes_files_and_commits_records() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        app_xpkg(&repo.join("app-1.0.xpkg"), "2.44");

        let mut db = LocalDb::open(temp.path().join("local.db")).unwrap();
        let rootfs = temp.path().join("rootfs");
        let changes = vec![SelectedPackage {
            name: "app".to_string(),
            version: Version::parse("1.0").unwrap(),
        }];

        let mut archives = HashMap::new();
        archives.insert(
            ("app".to_string(), Version::parse("1.0").unwrap()),
            repo.join("app-1.0.xpkg"),
        );
        installer::install_changes(&mut db, &changes, &archives, &rootfs, false, false).unwrap();

        assert_eq!(
            fs::read(rootfs.join("lib/libevil.so")).unwrap().len(),
            BASE_ELF.len()
        );
        assert_eq!(db.list_installed().unwrap()[0].name, "app");
        assert_eq!(db.list_files("app").unwrap(), ["lib/libevil.so"]);
    }
}
